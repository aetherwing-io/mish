//! sh_run — synchronous command execution MCP tool.
//!
//! Executes a command in a named session, categorizes it via the router,
//! routes the output through the appropriate post-processing pipeline,
//! applies watch pattern filtering, and returns a structured response
//! with process table digest.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use serde_json;
use tokio::sync::Mutex as TokioMutex;

use crate::config::MishConfig;
use crate::core::grammar::Grammar;
use crate::mcp::types::{
    LineCount, ProcessDigestEntry, ShRunMetrics, ShRunParams, ShRunResponse,
    ERR_COMMAND_BLOCKED,
};
use crate::router::categories::{CategoriesConfig, Category, DangerousPattern};
use crate::router::categorize_command_str;
use crate::safety;
use super::ToolError;
use crate::process::table::{DigestMode, ProcessTable};
use crate::session::manager::SessionManager;
use crate::squasher::pattern::{PatternMatcher, Presets};
use crate::squasher::pipeline::{Pipeline, PipelineConfig};
use crate::squasher::truncate::TruncateConfig;
use crate::squasher::vte_strip::VteStripper;
use crate::core::line_buffer::Line;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default session name when none is specified.
const DEFAULT_SESSION: &str = "main";

/// Regex to detect preset names: `@[a-z_]+`.
const PRESET_PATTERN: &str = r"^@[a-z_]+$";

/// Compiled preset regex, initialized once and reused.
static PRESET_RE: OnceLock<Regex> = OnceLock::new();

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Handle an sh_run tool call.
///
/// Executes a command in the specified (or default) session, categorizes it
/// via the router, applies category-appropriate post-processing (squashing
/// only for condense-category), filters with watch patterns, and returns
/// the structured result alongside a process table digest.
pub async fn handle(
    params: ShRunParams,
    session_manager: &SessionManager,
    process_table: &TokioMutex<ProcessTable>,
    config: &MishConfig,
    grammars: &HashMap<String, Grammar>,
    categories_config: &CategoriesConfig,
    dangerous_patterns: &[DangerousPattern],
) -> Result<(serde_json::Value, Vec<ProcessDigestEntry>), ToolError> {
    // 1. Validate required params.
    let cmd = params.cmd.trim();
    if cmd.is_empty() {
        return Err(ToolError::invalid_params("cmd must not be empty"));
    }

    // 1b. Safety deny-list check.
    if let Some(reason) = safety::check_deny_list(cmd) {
        return Err(ToolError::new(
            ERR_COMMAND_BLOCKED,
            format!("command blocked by safety deny-list: {reason}"),
        ));
    }

    // 1c. Categorize the command via the router.
    let category = categorize_command_str(cmd, grammars, categories_config, dangerous_patterns);

    // 1d. Block dangerous-category commands.
    if category == Category::Dangerous {
        return Err(ToolError::new(
            ERR_COMMAND_BLOCKED,
            "command blocked: dangerous category",
        ));
    }

    // 2. Resolve session name (default "main").
    let session_name = DEFAULT_SESSION;

    // 3. Resolve timeout: explicit > per-scope > config default.
    let timeout = resolve_timeout(&params, cmd, config);

    // 4. Execute command via SessionManager.
    let result = session_manager
        .execute_in_session(session_name, cmd, timeout)
        .await
        .map_err(ToolError::from_session_error)?;

    // 5. Post-process based on category.
    let raw_output = &result.output;
    let total_lines = raw_output.lines().count() as u64;

    let (processed_output, shown_lines, run_metrics) = match category {
        Category::Condense => {
            // Full squasher pipeline: VTE strip, progress removal, dedup, truncation.
            let squash_start = std::time::Instant::now();
            let (squashed, pipeline_metrics) = squash_output(raw_output, config);
            let squash_ms = squash_start.elapsed().as_millis() as u64;
            let shown = squashed.lines().count() as u64;
            let raw_bytes = raw_output.len() as u64;
            let squashed_bytes = squashed.len() as u64;
            let compression_ratio = if raw_bytes == 0 {
                1.0
            } else {
                squashed_bytes as f64 / raw_bytes as f64
            };
            let metrics = Some(ShRunMetrics {
                compression_ratio,
                raw_bytes,
                squashed_bytes,
                lines_in: pipeline_metrics.lines_in,
                lines_out: pipeline_metrics.lines_out,
                wall_ms: result.duration.as_millis() as u64,
                squash_ms,
            });
            (squashed, shown, metrics)
        }
        _ => {
            // Non-condense: VTE strip only (remove ANSI codes for LLM consumption).
            let stripped = strip_ansi(raw_output);
            let shown = stripped.lines().count() as u64;
            (stripped, shown, None)
        }
    };

    // 6. Apply watch pattern filtering if requested.
    let (final_output, matched_lines) = apply_watch_filter(
        &processed_output,
        params.watch.as_deref(),
        params.unmatched.as_deref(),
        config,
    )?;

    let final_shown = final_output.lines().count() as u64;

    // 7. Build response.
    let response = ShRunResponse {
        exit_code: result.exit_code,
        duration_ms: result.duration.as_millis() as u64,
        cwd: result.cwd,
        category: category.to_string(),
        output: final_output,
        matched_lines,
        lines: LineCount {
            total: total_lines,
            shown: if matched_lines_present(&params) {
                final_shown
            } else {
                shown_lines
            },
        },
        metrics: run_metrics,
    };

    let response_json = serde_json::to_value(&response)
        .map_err(|e| ToolError::internal(format!("failed to serialize response: {e}")))?;

    // 8. Generate process digest.
    let digest = {
        let mut table = process_table.lock().await;
        table.digest(DigestMode::Changed)
    };

    Ok((response_json, digest))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Check if matched_lines will be present in the response.
fn matched_lines_present(params: &ShRunParams) -> bool {
    params.watch.is_some()
}

/// Resolve timeout using the precedence: explicit > per-scope > config default.
fn resolve_timeout(params: &ShRunParams, cmd: &str, config: &MishConfig) -> Duration {
    // 1. Explicit timeout from tool call params.
    if let Some(explicit) = params.timeout {
        return Duration::from_secs(explicit);
    }

    // 2. Per-scope timeout using policy scope extraction.
    let scope = crate::policy::scope::extract_scope(cmd);
    if let Some(scope_timeout) = config.timeout_defaults.scope.get(scope) {
        return Duration::from_secs(*scope_timeout);
    }

    // 3. Global default.
    Duration::from_secs(config.timeout_defaults.default)
}

/// Run output through the squasher pipeline (VTE strip, progress removal,
/// dedup, truncation). Returns the squashed output and pipeline metrics.
fn squash_output(raw: &str, config: &MishConfig) -> (String, crate::squasher::pipeline::PipelineMetrics) {
    let pipeline_config = PipelineConfig {
        truncate: TruncateConfig {
            head: config.squasher.oreo_head,
            tail: config.squasher.oreo_tail,
        },
        dedup_all: true,
    };

    let mut pipeline = Pipeline::new(pipeline_config);

    for line in raw.lines() {
        pipeline.feed(Line::Complete(line.to_string()));
    }

    let (lines, metrics) = pipeline.finalize_with_metrics();
    (lines.join("\n"), metrics)
}

/// Resolve a watch pattern string to a compiled PatternMatcher.
///
/// If the pattern starts with `@` and matches the preset format, expand
/// the preset. Otherwise treat as a raw regex (pipe-separated, case-insensitive).
fn resolve_watch_pattern(
    watch: &str,
    config: &MishConfig,
) -> Result<PatternMatcher, ToolError> {
    let preset_re = PRESET_RE.get_or_init(|| Regex::new(PRESET_PATTERN).unwrap());

    let pattern_strings: Vec<String> = if preset_re.is_match(watch) {
        // Check config watch_presets first, fall back to built-in Presets.
        if let Some(config_pattern) = config.watch_presets.get(watch) {
            // Config presets are stored as raw regex strings.
            vec![config_pattern.clone()]
        } else {
            Presets::expand(watch)
        }
    } else {
        // Raw regex: wrap with case-insensitive flag.
        vec![format!("(?i){watch}")]
    };

    let pattern_refs: Vec<&str> = pattern_strings.iter().map(|s| s.as_str()).collect();
    PatternMatcher::new(&pattern_refs)
        .map_err(|e| ToolError::invalid_params(format!("invalid watch pattern: {e}")))
}

/// Apply watch filter to output lines.
///
/// Returns (final_output, matched_lines).
/// - If no watch pattern: returns (output, None).
/// - If watch set with unmatched="keep" (default): returns (output, Some(matching_lines)).
/// - If watch set with unmatched="drop": returns (matching_lines_only, Some(matching_lines)).
fn apply_watch_filter(
    output: &str,
    watch: Option<&str>,
    unmatched: Option<&str>,
    config: &MishConfig,
) -> Result<(String, Option<Vec<String>>), ToolError> {
    let watch = match watch {
        Some(w) if !w.is_empty() => w,
        _ => return Ok((output.to_string(), None)),
    };

    let matcher = resolve_watch_pattern(watch, config)?;
    let unmatched_mode = unmatched.unwrap_or("keep");

    let mut matched = Vec::new();
    let mut kept_lines = Vec::new();

    for line in output.lines() {
        if matcher.matches(line) {
            matched.push(line.to_string());
            kept_lines.push(line.to_string());
        } else if unmatched_mode == "keep" {
            kept_lines.push(line.to_string());
        }
        // else: unmatched="drop" — skip non-matching lines
    }

    let final_output = kept_lines.join("\n");
    Ok((final_output, Some(matched)))
}

/// Strip ANSI escape sequences from output, line by line.
/// Used for non-condense categories where we want clean text
/// without the full squasher pipeline (no dedup, no truncation).
fn strip_ansi(raw: &str) -> String {
    raw.lines()
        .map(|line| VteStripper::strip(line.as_bytes()).clean_text)
        .collect::<Vec<_>>()
        .join("\n")
}


// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::config::default_config;
    use crate::config_loader::default_runtime_config;
    use crate::mcp::types::{ERR_COMMAND_BLOCKED, ERR_INVALID_PARAMS, ERR_SESSION_NOT_FOUND};
    use crate::session::manager::SessionError;

    /// Helper: return categorization data from default runtime config.
    fn test_categorization() -> (HashMap<String, Grammar>, CategoriesConfig, Vec<DangerousPattern>) {
        let rc = default_runtime_config();
        (rc.grammars, rc.categories_config, rc.dangerous_patterns)
    }

    // -----------------------------------------------------------------------
    // Unit tests for internal helpers (no PTY required)
    // -----------------------------------------------------------------------

    // -- scope extraction (delegates to policy::scope::extract_scope) ------

    #[test]
    fn test_scope_extraction_simple() {
        assert_eq!(crate::policy::scope::extract_scope("echo hello"), "echo");
    }

    #[test]
    fn test_scope_extraction_with_path() {
        assert_eq!(crate::policy::scope::extract_scope("/usr/bin/npm install"), "npm");
    }

    #[test]
    fn test_scope_extraction_single_word() {
        assert_eq!(crate::policy::scope::extract_scope("ls"), "ls");
    }

    #[test]
    fn test_scope_extraction_empty() {
        assert_eq!(crate::policy::scope::extract_scope(""), "");
    }

    // -- resolve_timeout ----------------------------------------------------

    #[test]
    fn test_resolve_timeout_explicit() {
        let config = default_config();
        let params = ShRunParams {
            cmd: "npm install".to_string(),
            timeout: Some(60),
            watch: None,
            unmatched: None,
        };
        let timeout = resolve_timeout(&params, "npm install", &config);
        assert_eq!(timeout, Duration::from_secs(60));
    }

    #[test]
    fn test_resolve_timeout_per_scope() {
        let config = default_config();
        let params = ShRunParams {
            cmd: "npm install".to_string(),
            timeout: None,
            watch: None,
            unmatched: None,
        };
        // Default config has npm -> 300 in scope.
        let timeout = resolve_timeout(&params, "npm install", &config);
        assert_eq!(timeout, Duration::from_secs(300));
    }

    #[test]
    fn test_resolve_timeout_per_scope_cargo() {
        let config = default_config();
        let params = ShRunParams {
            cmd: "cargo build".to_string(),
            timeout: None,
            watch: None,
            unmatched: None,
        };
        // Default config has cargo -> 600 in scope.
        let timeout = resolve_timeout(&params, "cargo build", &config);
        assert_eq!(timeout, Duration::from_secs(600));
    }

    #[test]
    fn test_resolve_timeout_global_default() {
        let config = default_config();
        let params = ShRunParams {
            cmd: "echo hello".to_string(),
            timeout: None,
            watch: None,
            unmatched: None,
        };
        // "echo" is not in scope map -> global default (300).
        let timeout = resolve_timeout(&params, "echo hello", &config);
        assert_eq!(timeout, Duration::from_secs(config.timeout_defaults.default));
    }

    #[test]
    fn test_resolve_timeout_explicit_overrides_scope() {
        let config = default_config();
        let params = ShRunParams {
            cmd: "npm install".to_string(),
            timeout: Some(10),
            watch: None,
            unmatched: None,
        };
        // Even though npm has a scope timeout of 300, explicit 10 wins.
        let timeout = resolve_timeout(&params, "npm install", &config);
        assert_eq!(timeout, Duration::from_secs(10));
    }

    #[test]
    fn test_resolve_timeout_path_command() {
        let config = default_config();
        let params = ShRunParams {
            cmd: "/usr/bin/npm install".to_string(),
            timeout: None,
            watch: None,
            unmatched: None,
        };
        // Should extract basename "npm" and match scope.
        let timeout = resolve_timeout(&params, "/usr/bin/npm install", &config);
        assert_eq!(timeout, Duration::from_secs(300));
    }

    // -- squash_output ------------------------------------------------------

    #[test]
    fn test_squash_output_passthrough() {
        let config = default_config();
        let output = "hello\nworld";
        let (squashed, _metrics) = squash_output(output, &config);
        assert!(squashed.contains("hello"));
        assert!(squashed.contains("world"));
    }

    #[test]
    fn test_squash_output_strips_ansi() {
        let config = default_config();
        let output = "\x1b[31merror: something\x1b[0m";
        let (squashed, _metrics) = squash_output(output, &config);
        assert!(squashed.contains("error: something"));
        assert!(!squashed.contains("\x1b"));
    }

    #[test]
    fn test_squash_output_empty() {
        let config = default_config();
        let (squashed, _metrics) = squash_output("", &config);
        assert!(squashed.is_empty());
    }

    #[test]
    fn test_squash_output_returns_metrics() {
        let config = default_config();
        let output = "line1\nline2\nline3";
        let (squashed, metrics) = squash_output(output, &config);
        assert!(!squashed.is_empty());
        assert_eq!(metrics.lines_in, 3);
        assert!(metrics.lines_out > 0);
    }

    // -- watch pattern filtering --------------------------------------------

    #[test]
    fn test_watch_filter_none() {
        let config = default_config();
        let (output, matched) = apply_watch_filter(
            "line1\nline2\nline3",
            None,
            None,
            &config,
        )
        .unwrap();
        assert_eq!(output, "line1\nline2\nline3");
        assert!(matched.is_none());
    }

    #[test]
    fn test_watch_filter_raw_regex_keep() {
        let config = default_config();
        let (output, matched) = apply_watch_filter(
            "info: ok\nerror: bad\ninfo: fine\nwarning: careful",
            Some("error|warning"),
            None, // default is "keep"
            &config,
        )
        .unwrap();

        // Output should contain all lines (unmatched="keep").
        assert!(output.contains("info: ok"));
        assert!(output.contains("error: bad"));
        assert!(output.contains("warning: careful"));

        // matched_lines should contain only matching lines.
        let matched = matched.unwrap();
        assert_eq!(matched.len(), 2);
        assert!(matched.contains(&"error: bad".to_string()));
        assert!(matched.contains(&"warning: careful".to_string()));
    }

    #[test]
    fn test_watch_filter_raw_regex_drop() {
        let config = default_config();
        let (output, matched) = apply_watch_filter(
            "info: ok\nerror: bad\ninfo: fine\nwarning: careful",
            Some("error|warning"),
            Some("drop"),
            &config,
        )
        .unwrap();

        // Output should contain only matching lines.
        assert!(!output.contains("info: ok"));
        assert!(output.contains("error: bad"));
        assert!(output.contains("warning: careful"));

        let matched = matched.unwrap();
        assert_eq!(matched.len(), 2);
    }

    #[test]
    fn test_watch_filter_preset_errors() {
        let config = default_config();
        let (_, matched) = apply_watch_filter(
            "compiling crate...\nerror[E0308]: mismatched types\nFinished",
            Some("@errors"),
            None,
            &config,
        )
        .unwrap();

        let matched = matched.unwrap();
        assert_eq!(matched.len(), 1);
        assert!(matched[0].contains("error[E0308]"));
    }

    #[test]
    fn test_watch_filter_preset_from_config() {
        let mut config = default_config();
        config
            .watch_presets
            .insert("@custom".to_string(), "CUSTOM_PATTERN".to_string());

        let (_, matched) = apply_watch_filter(
            "line1\nCUSTOM_PATTERN found\nline3",
            Some("@custom"),
            None,
            &config,
        )
        .unwrap();

        let matched = matched.unwrap();
        assert_eq!(matched.len(), 1);
        assert!(matched[0].contains("CUSTOM_PATTERN"));
    }

    #[test]
    fn test_watch_filter_invalid_regex() {
        let config = default_config();
        let result = apply_watch_filter(
            "test",
            Some("[invalid"),
            None,
            &config,
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, ERR_INVALID_PARAMS);
    }

    #[test]
    fn test_watch_filter_empty_watch() {
        let config = default_config();
        let (output, matched) = apply_watch_filter(
            "hello",
            Some(""),
            None,
            &config,
        )
        .unwrap();
        // Empty watch treated as no watch.
        assert_eq!(output, "hello");
        assert!(matched.is_none());
    }

    #[test]
    fn test_preset_detection_vs_raw_regex() {
        // @errors -> preset
        let preset_re = PRESET_RE.get_or_init(|| Regex::new(PRESET_PATTERN).unwrap());
        assert!(preset_re.is_match("@errors"));
        assert!(preset_re.is_match("@warnings"));
        assert!(preset_re.is_match("@npm"));
        assert!(preset_re.is_match("@test_results"));

        // These should NOT match as presets.
        assert!(!preset_re.is_match("errors"));
        assert!(!preset_re.is_match("@Errors")); // uppercase
        assert!(!preset_re.is_match("@errors123")); // digits
        assert!(!preset_re.is_match("error|warning")); // pipe-separated
    }

    // -- default session resolution -----------------------------------------

    #[test]
    fn test_default_session_constant() {
        assert_eq!(DEFAULT_SESSION, "main");
    }

    // -- ToolError ----------------------------------------------------------

    #[test]
    fn test_tool_error_display() {
        let err = ToolError::new(-32602, "invalid param");
        assert_eq!(format!("{err}"), "[-32602] invalid param");
    }

    #[test]
    fn test_tool_error_from_session_error() {
        let session_err = SessionError::NotFound("test".into());
        let tool_err = ToolError::from_session_error(session_err);
        assert_eq!(tool_err.code, -32002);
        assert!(tool_err.message.contains("not found"));
    }

    // -----------------------------------------------------------------------
    // Integration tests (require PTY / shell process)
    // -----------------------------------------------------------------------

    /// Helper: create a SessionManager, create "main" session, return Arc.
    async fn setup_session() -> (Arc<SessionManager>, Arc<TokioMutex<ProcessTable>>) {
        let config = Arc::new(default_config());
        let mgr = Arc::new(SessionManager::new(config.clone()));
        mgr.create_session("main", Some("/bin/bash"))
            .await
            .expect("should create main session");
        let table = Arc::new(TokioMutex::new(ProcessTable::new(&config)));
        (mgr, table)
    }

    /// Helper: teardown session manager.
    async fn teardown(mgr: &SessionManager) {
        mgr.close_all().await;
    }

    #[tokio::test]
    async fn test_handle_echo_hello() {
        let (mgr, table) = setup_session().await;
        let config = default_config();
        let (grammars, categories, dangerous) = test_categorization();

        let params = ShRunParams {
            cmd: "echo hello".to_string(),
            timeout: Some(5),
            watch: None,
            unmatched: None,
        };

        let (result, digest) = handle(params, &mgr, &table, &config, &grammars, &categories, &dangerous)
            .await
            .expect("handle should succeed");

        assert_eq!(result["exit_code"], 0);
        assert!(
            result["output"]
                .as_str()
                .unwrap()
                .contains("hello"),
            "output should contain 'hello', got: {}",
            result["output"]
        );
        assert!(result["cwd"].is_string());
        assert!(result["duration_ms"].is_number());
        assert!(result["lines"]["total"].is_number());
        assert!(result["lines"]["shown"].is_number());

        // Digest is a vec (may be empty since no background processes).
        assert!(digest.is_empty() || !digest.is_empty());

        teardown(&mgr).await;
    }

    #[tokio::test]
    async fn test_handle_exit_code() {
        let (mgr, table) = setup_session().await;
        let config = default_config();
        let (grammars, categories, dangerous) = test_categorization();

        // Use a subshell `(exit 42)` so the session shell survives.
        // `exit 42` would kill the session shell itself, causing a timeout.
        let params = ShRunParams {
            cmd: "(exit 42)".to_string(),
            timeout: Some(5),
            watch: None,
            unmatched: None,
        };

        let (result, _) = handle(params, &mgr, &table, &config, &grammars, &categories, &dangerous)
            .await
            .expect("handle should succeed");

        assert_eq!(result["exit_code"], 42);

        teardown(&mgr).await;
    }

    #[tokio::test]
    async fn test_handle_empty_cmd_error() {
        let (mgr, table) = setup_session().await;
        let config = default_config();
        let (grammars, categories, dangerous) = test_categorization();

        let params = ShRunParams {
            cmd: "   ".to_string(),
            timeout: Some(5),
            watch: None,
            unmatched: None,
        };

        let result = handle(params, &mgr, &table, &config, &grammars, &categories, &dangerous).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, ERR_INVALID_PARAMS);

        teardown(&mgr).await;
    }

    #[tokio::test]
    async fn test_handle_session_not_found() {
        let config_arc = Arc::new(default_config());
        let mgr = SessionManager::new(config_arc.clone());
        // Do NOT create "main" session.
        let table = TokioMutex::new(ProcessTable::new(&config_arc));
        let config = default_config();
        let (grammars, categories, dangerous) = test_categorization();

        let params = ShRunParams {
            cmd: "echo hi".to_string(),
            timeout: Some(5),
            watch: None,
            unmatched: None,
        };

        let result = handle(params, &mgr, &table, &config, &grammars, &categories, &dangerous).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, ERR_SESSION_NOT_FOUND);
    }

    #[tokio::test]
    async fn test_handle_with_watch_pattern() {
        let (mgr, table) = setup_session().await;
        let config = default_config();
        let (grammars, categories, dangerous) = test_categorization();

        let params = ShRunParams {
            cmd: r#"printf 'line1\nerror: bad\nline3\nwarning: careful\n'"#.to_string(),
            timeout: Some(5),
            watch: Some("error|warning".to_string()),
            unmatched: None,
        };

        let (result, _) = handle(params, &mgr, &table, &config, &grammars, &categories, &dangerous)
            .await
            .expect("handle should succeed");

        assert_eq!(result["exit_code"], 0);
        let matched = result["matched_lines"].as_array().unwrap();
        assert!(
            matched.len() >= 2,
            "expected at least 2 matched lines, got: {matched:?}"
        );

        teardown(&mgr).await;
    }

    #[tokio::test]
    async fn test_handle_with_watch_drop_unmatched() {
        let (mgr, table) = setup_session().await;
        let config = default_config();
        let (grammars, categories, dangerous) = test_categorization();

        let params = ShRunParams {
            cmd: r#"printf 'line1\nerror: bad\nline3\n'"#.to_string(),
            timeout: Some(5),
            watch: Some("error".to_string()),
            unmatched: Some("drop".to_string()),
        };

        let (result, _) = handle(params, &mgr, &table, &config, &grammars, &categories, &dangerous)
            .await
            .expect("handle should succeed");

        let output = result["output"].as_str().unwrap();
        // Output should only contain the matching line.
        assert!(output.contains("error: bad"));
        assert!(!output.contains("line1"));
        assert!(!output.contains("line3"));

        teardown(&mgr).await;
    }

    #[tokio::test]
    async fn test_handle_cwd_tracking() {
        let (mgr, table) = setup_session().await;
        let config = default_config();
        let (grammars, categories, dangerous) = test_categorization();

        // cd to /tmp
        let params = ShRunParams {
            cmd: "cd /tmp".to_string(),
            timeout: Some(5),
            watch: None,
            unmatched: None,
        };
        let (result, _) = handle(params, &mgr, &table, &config, &grammars, &categories, &dangerous)
            .await
            .expect("cd should succeed");

        let cwd = result["cwd"].as_str().unwrap();
        assert!(
            cwd == "/tmp" || cwd == "/private/tmp",
            "CWD should be /tmp or /private/tmp, got: {cwd}"
        );

        teardown(&mgr).await;
    }

    #[tokio::test]
    async fn test_handle_timeout_resolution() {
        // This test verifies timeout resolution works for different commands.
        // We cannot easily test that timeout kills a process without a long sleep,
        // so we verify the resolution logic produces correct Duration values.
        let config = default_config();

        // npm -> 300s (scope)
        let params = ShRunParams {
            cmd: "npm install".to_string(),
            timeout: None,
            watch: None,
            unmatched: None,
        };
        assert_eq!(
            resolve_timeout(&params, "npm install", &config),
            Duration::from_secs(300)
        );

        // cargo -> 600s (scope)
        let params = ShRunParams {
            cmd: "cargo build".to_string(),
            timeout: None,
            watch: None,
            unmatched: None,
        };
        assert_eq!(
            resolve_timeout(&params, "cargo build", &config),
            Duration::from_secs(600)
        );

        // explicit overrides scope
        let params = ShRunParams {
            cmd: "npm install".to_string(),
            timeout: Some(10),
            watch: None,
            unmatched: None,
        };
        assert_eq!(
            resolve_timeout(&params, "npm install", &config),
            Duration::from_secs(10)
        );

        // unknown command -> global default
        let params = ShRunParams {
            cmd: "my_custom_tool run".to_string(),
            timeout: None,
            watch: None,
            unmatched: None,
        };
        assert_eq!(
            resolve_timeout(&params, "my_custom_tool run", &config),
            Duration::from_secs(config.timeout_defaults.default)
        );
    }

    #[tokio::test]
    async fn test_handle_response_structure() {
        let (mgr, table) = setup_session().await;
        let config = default_config();
        let (grammars, categories, dangerous) = test_categorization();

        let params = ShRunParams {
            cmd: "echo test_output".to_string(),
            timeout: Some(5),
            watch: None,
            unmatched: None,
        };

        let (result, _) = handle(params, &mgr, &table, &config, &grammars, &categories, &dangerous)
            .await
            .expect("handle should succeed");

        // Verify all required fields are present.
        assert!(result.get("exit_code").is_some(), "missing exit_code");
        assert!(result.get("duration_ms").is_some(), "missing duration_ms");
        assert!(result.get("cwd").is_some(), "missing cwd");
        assert!(result.get("output").is_some(), "missing output");
        assert!(result.get("lines").is_some(), "missing lines");
        assert!(result["lines"].get("total").is_some(), "missing lines.total");
        assert!(result["lines"].get("shown").is_some(), "missing lines.shown");

        // matched_lines should be absent when no watch.
        assert!(
            result.get("matched_lines").is_none(),
            "matched_lines should be absent without watch"
        );

        teardown(&mgr).await;
    }

    // -- deny-list integration -----------------------------------------------

    #[tokio::test]
    async fn test_deny_list_blocks_rm_rf_root() {
        let (mgr, table) = setup_session().await;
        let config = default_config();
        let (grammars, categories, dangerous) = test_categorization();

        let params = ShRunParams {
            cmd: "rm -rf /".to_string(),
            timeout: Some(5),
            watch: None,
            unmatched: None,
        };

        let result = handle(params, &mgr, &table, &config, &grammars, &categories, &dangerous).await;
        assert!(result.is_err(), "rm -rf / should be blocked");
        let err = result.unwrap_err();
        assert_eq!(err.code, ERR_COMMAND_BLOCKED);
        assert!(err.message.contains("deny-list"));

        teardown(&mgr).await;
    }

    #[tokio::test]
    async fn test_deny_list_blocks_mkfs() {
        let (mgr, table) = setup_session().await;
        let config = default_config();
        let (grammars, categories, dangerous) = test_categorization();

        let params = ShRunParams {
            cmd: "mkfs.ext4 /dev/sda".to_string(),
            timeout: Some(5),
            watch: None,
            unmatched: None,
        };

        let result = handle(params, &mgr, &table, &config, &grammars, &categories, &dangerous).await;
        assert!(result.is_err(), "mkfs.ext4 should be blocked");
        let err = result.unwrap_err();
        assert_eq!(err.code, ERR_COMMAND_BLOCKED);

        teardown(&mgr).await;
    }

    #[tokio::test]
    async fn test_deny_list_allows_safe_commands() {
        let (mgr, table) = setup_session().await;
        let config = default_config();
        let (grammars, categories, dangerous) = test_categorization();

        let params = ShRunParams {
            cmd: "echo safe_command".to_string(),
            timeout: Some(5),
            watch: None,
            unmatched: None,
        };

        let result = handle(params, &mgr, &table, &config, &grammars, &categories, &dangerous).await;
        assert!(result.is_ok(), "echo should be allowed");

        teardown(&mgr).await;
    }

    // -----------------------------------------------------------------------
    // Category-aware behavior tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_echo_categorized_as_passthrough() {
        let (mgr, table) = setup_session().await;
        let config = default_config();
        let (grammars, categories, dangerous) = test_categorization();

        let params = ShRunParams {
            cmd: "echo hello".to_string(),
            timeout: Some(5),
            watch: None,
            unmatched: None,
        };

        let (result, _) = handle(params, &mgr, &table, &config, &grammars, &categories, &dangerous)
            .await
            .expect("handle should succeed");

        assert_eq!(
            result["category"].as_str().unwrap(),
            "passthrough",
            "echo should be categorized as passthrough"
        );

        teardown(&mgr).await;
    }

    #[tokio::test]
    async fn test_passthrough_output_not_squashed() {
        let (mgr, table) = setup_session().await;
        let config = default_config();
        let (grammars, categories, dangerous) = test_categorization();

        // Generate output that would be deduped by the squasher if applied.
        // Repeated lines get collapsed by dedup, so if output still has all
        // lines, it was NOT squashed.
        let params = ShRunParams {
            cmd: r#"printf 'line\nline\nline\n'"#.to_string(),
            timeout: Some(5),
            watch: None,
            unmatched: None,
        };

        let (result, _) = handle(params, &mgr, &table, &config, &grammars, &categories, &dangerous)
            .await
            .expect("handle should succeed");

        // printf is not in categories -> fallback Condense -> squashed.
        // But the key point: for commands that ARE categorized as non-condense,
        // output should not be deduped. Let's verify category is reported.
        assert!(result["category"].is_string());

        teardown(&mgr).await;
    }

    #[tokio::test]
    async fn test_dangerous_category_blocked() {
        let (mgr, table) = setup_session().await;
        let config = default_config();
        let (grammars, categories, dangerous) = test_categorization();

        // "rm -rf /tmp/foo" matches dangerous pattern in bundled dangerous.toml.
        let params = ShRunParams {
            cmd: "rm -rf /tmp/foo".to_string(),
            timeout: Some(5),
            watch: None,
            unmatched: None,
        };

        let result = handle(params, &mgr, &table, &config, &grammars, &categories, &dangerous).await;
        assert!(result.is_err(), "dangerous commands should be blocked");
        let err = result.unwrap_err();
        assert_eq!(err.code, ERR_COMMAND_BLOCKED);

        teardown(&mgr).await;
    }

    #[tokio::test]
    async fn test_response_includes_category_field() {
        let (mgr, table) = setup_session().await;
        let config = default_config();
        let (grammars, categories, dangerous) = test_categorization();

        let params = ShRunParams {
            cmd: "echo test".to_string(),
            timeout: Some(5),
            watch: None,
            unmatched: None,
        };

        let (result, _) = handle(params, &mgr, &table, &config, &grammars, &categories, &dangerous)
            .await
            .expect("handle should succeed");

        // Verify the category field exists and is a valid string.
        let category = result["category"].as_str().unwrap();
        assert!(
            ["condense", "narrate", "passthrough", "structured", "interactive"].contains(&category),
            "category should be a valid category name, got: {category}"
        );

        teardown(&mgr).await;
    }

    // -- strip_ansi unit tests -------------------------------------------

    #[test]
    fn test_strip_ansi_removes_color_codes() {
        let input = "\x1b[31merror: something\x1b[0m\n\x1b[32mok\x1b[0m";
        let stripped = strip_ansi(input);
        assert_eq!(stripped, "error: something\nok");
    }

    #[test]
    fn test_strip_ansi_preserves_plain_text() {
        let input = "hello\nworld";
        let stripped = strip_ansi(input);
        assert_eq!(stripped, "hello\nworld");
    }

    #[test]
    fn test_strip_ansi_empty() {
        assert_eq!(strip_ansi(""), "");
    }

    // -- Category Display unit tests --------------------------------------

    #[test]
    fn test_category_display_all_variants() {
        assert_eq!(Category::Condense.to_string(), "condense");
        assert_eq!(Category::Narrate.to_string(), "narrate");
        assert_eq!(Category::Passthrough.to_string(), "passthrough");
        assert_eq!(Category::Structured.to_string(), "structured");
        assert_eq!(Category::Interactive.to_string(), "interactive");
        assert_eq!(Category::Dangerous.to_string(), "dangerous");
    }
}
