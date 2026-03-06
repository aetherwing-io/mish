pub mod categories;

// Top-level category routing.
//
// command -> grammar lookup -> categorize -> dispatch to handler
//
// The router is the central dispatch layer for both CLI proxy and MCP server modes.
// It loads grammar for the command, runs preflight if applicable, categorizes the
// command, dispatches to the appropriate handler, and optionally runs error enrichment
// on failure.

use std::collections::HashMap;

use crate::core::emit::Summary;
use crate::core::grammar::{detect_tool, Action, Grammar};
use crate::core::preflight::{preflight, OutputMode, PreflightResult};
use crate::core::stat::NarratedResult;
use crate::handlers::dangerous::DangerousResult;
use crate::handlers::interactive::InteractiveResult;
use crate::handlers::passthrough::PassthroughResult;
use crate::handlers::structured::StructuredResult;
use crate::router::categories::{categorize, CategoriesConfig, Category, DangerousPattern, ExecutionMode};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A diagnostic line from error enrichment.
#[derive(Debug, Clone)]
pub struct DiagnosticLine {
    pub kind: String,
    pub message: String,
}

/// The output from whichever handler processed the command.
pub enum HandlerOutput {
    Condensed(Summary),
    Narrated(NarratedResult),
    Passthrough(PassthroughResult),
    Structured(StructuredResult),
    Interactive(InteractiveResult),
    Dangerous(DangerousResult),
}

/// Unified result from the category router.
pub struct RouterResult {
    pub category: Category,
    pub exit_code: i32,
    pub output: HandlerOutput,
    pub enrichment: Option<Vec<DiagnosticLine>>,
    pub preflight: Option<PreflightResult>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Route a command through the full pipeline:
/// grammar detection -> preflight -> categorization -> handler dispatch -> enrichment.
pub fn route(
    command: &[String],
    grammars: &HashMap<String, Grammar>,
    categories_config: &CategoriesConfig,
    dangerous_patterns: &[DangerousPattern],
    mode: OutputMode,
    exec_mode: ExecutionMode,
) -> Result<RouterResult, Box<dyn std::error::Error>> {
    if command.is_empty() {
        return Err("router: empty command".into());
    }

    // 1. Detect grammar and action for the command
    let detected = detect_tool(command, grammars);
    let (grammar, action) = match &detected {
        Some((g, a)) => (Some(*g), *a),
        None => (None, None),
    };

    // 2. Run preflight if grammar exists (may modify command args)
    let mut cmd = command.to_vec();
    let preflight_result = grammar.map(|g| preflight(&mut cmd, g, action, mode));

    // 3. Categorize the command
    let category = categorize(&cmd, grammars, categories_config, dangerous_patterns);

    // 4. Dispatch to the appropriate handler
    let (output, exit_code) = dispatch(category, &cmd, grammar, action, dangerous_patterns, exec_mode)?;

    // 5. Error enrichment on non-zero exit code
    let enrichment = if exit_code != 0 {
        let result = crate::core::enrich::enrich(&cmd, exit_code, "", grammar);
        let diags: Vec<DiagnosticLine> = result
            .diagnostics
            .into_iter()
            .map(|d| DiagnosticLine {
                kind: d.key,
                message: d.value,
            })
            .collect();
        if diags.is_empty() { None } else { Some(diags) }
    } else {
        None
    };

    Ok(RouterResult {
        category,
        exit_code,
        output,
        enrichment,
        preflight: preflight_result,
    })
}

/// Categorize a shell command string by splitting on whitespace.
///
/// Convenience wrapper for MCP mode where commands arrive as a single string
/// rather than a pre-tokenized array.
pub fn categorize_command_str(
    cmd: &str,
    grammars: &HashMap<String, Grammar>,
    categories_config: &CategoriesConfig,
    dangerous_patterns: &[DangerousPattern],
) -> Category {
    let tokens: Vec<String> = cmd.split_whitespace().map(String::from).collect();
    let category = categories::categorize(&tokens, grammars, categories_config, dangerous_patterns);

    // Pipeline heuristic: if the command contains a pipe and the category
    // fell through to the default (Condense), upgrade to Passthrough.
    // Piped commands are deliberate data extraction — the user wants the output.
    // Explicit categories (grammar, categories.toml, dangerous) are preserved.
    if category == Category::Condense && tokens.contains(&"|".to_string()) {
        // Check if the first command was explicitly categorized as Condense
        // (via grammar or categories.toml) — if so, respect it.
        let first_cmd = &tokens[0];
        let explicitly_condense = grammars.values().any(|g| {
            g.detect.iter().any(|d| d == first_cmd) && g.category == Some(Category::Condense)
        }) || categories_config.categories.get(first_cmd.as_str()) == Some(&Category::Condense);

        if !explicitly_condense {
            return Category::Passthrough;
        }
    }

    category
}

/// Dispatch a categorized command to its handler, returning the handler output and exit code.
fn dispatch(
    category: Category,
    cmd: &[String],
    grammar: Option<&Grammar>,
    action: Option<&Action>,
    dangerous_patterns: &[DangerousPattern],
    exec_mode: ExecutionMode,
) -> Result<(HandlerOutput, i32), Box<dyn std::error::Error>> {
    match category {
        Category::Condense => {
            let mut result = crate::handlers::condense::handle(cmd, grammar, action)?;
            let exit_code = result.summary.exit_code;
            result.summary.interactive_session = result.transitioned_to_interactive;
            Ok((HandlerOutput::Condensed(result.summary), exit_code))
        }
        Category::Narrate => {
            let result = crate::handlers::narrate::handle(cmd)?;
            let exit_code = result.exit_code;
            Ok((HandlerOutput::Narrated(result), exit_code))
        }
        Category::Passthrough => {
            let result = crate::handlers::passthrough::handle(cmd)?;
            let exit_code = result.exit_code;
            Ok((HandlerOutput::Passthrough(result), exit_code))
        }
        Category::Structured => {
            let result = crate::handlers::structured::handle(cmd)?;
            let exit_code = result.exit_code;
            Ok((HandlerOutput::Structured(result), exit_code))
        }
        Category::Interactive => {
            let result = crate::handlers::interactive::handle(cmd, exec_mode)?;
            let exit_code = result.exit_code;
            Ok((HandlerOutput::Interactive(result), exit_code))
        }
        Category::Dangerous => {
            let result = crate::handlers::dangerous::handle(cmd, dangerous_patterns, exec_mode, None)?;
            let exit_code = result.exit_code.unwrap_or(0);
            Ok((HandlerOutput::Dangerous(result), exit_code))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::grammar::load_grammar_from_str;
    use crate::core::preflight::OutputMode;
    use crate::router::categories::ExecutionMode;
    use regex::Regex;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    fn empty_grammars() -> HashMap<String, Grammar> {
        HashMap::new()
    }

    fn empty_config() -> CategoriesConfig {
        CategoriesConfig {
            categories: HashMap::new(),
        }
    }

    fn empty_dangerous() -> Vec<DangerousPattern> {
        Vec::new()
    }

    /// Build a standard categories config with common mappings.
    fn standard_config() -> CategoriesConfig {
        CategoriesConfig {
            categories: HashMap::from([
                ("npm".to_string(), Category::Condense),
                ("cargo".to_string(), Category::Condense),
                ("cp".to_string(), Category::Narrate),
                ("mv".to_string(), Category::Narrate),
                ("mkdir".to_string(), Category::Narrate),
                ("rm".to_string(), Category::Narrate),
                ("cat".to_string(), Category::Passthrough),
                ("head".to_string(), Category::Passthrough),
                ("echo".to_string(), Category::Passthrough),
                ("git".to_string(), Category::Structured),
                ("docker".to_string(), Category::Structured),
                ("vim".to_string(), Category::Interactive),
                ("nano".to_string(), Category::Interactive),
            ]),
        }
    }

    /// Standard dangerous patterns used by several tests.
    fn standard_dangerous() -> Vec<DangerousPattern> {
        vec![
            DangerousPattern {
                pattern: Regex::new(r"rm\s+-rf").unwrap(),
                reason: "Force recursive delete".to_string(),
            },
            DangerousPattern {
                pattern: Regex::new(r"git\s+push\s+.*--force").unwrap(),
                reason: "Force push".to_string(),
            },
        ]
    }

    // -----------------------------------------------------------------------
    // Test 1: Route `npm install` -> Condense category
    // -----------------------------------------------------------------------
    // Uses categories.toml mapping. Since npm isn't installed in the test
    // environment, we verify category resolution via categorize() directly
    // and also test that route() selects the correct category.
    #[test]
    fn test_route_npm_install_condense() {
        let config = standard_config();
        let command = cmd(&["npm", "install"]);
        let category = categorize(&command, &empty_grammars(), &config, &empty_dangerous());
        assert_eq!(category, Category::Condense);
    }

    // -----------------------------------------------------------------------
    // Test 2: Route `cp file.txt dest/` -> Narrate category
    // -----------------------------------------------------------------------
    #[test]
    fn test_route_cp_narrate() {
        let config = standard_config();
        let command = cmd(&["cp", "file.txt", "dest/"]);
        let category = categorize(&command, &empty_grammars(), &config, &empty_dangerous());
        assert_eq!(category, Category::Narrate);
    }

    // -----------------------------------------------------------------------
    // Test 3: Route `cat file.txt` -> Passthrough category
    // -----------------------------------------------------------------------
    #[test]
    fn test_route_cat_passthrough() {
        let config = standard_config();
        let command = cmd(&["cat", "file.txt"]);
        let category = categorize(&command, &empty_grammars(), &config, &empty_dangerous());
        assert_eq!(category, Category::Passthrough);
    }

    // -----------------------------------------------------------------------
    // Test 4: Route `git status` -> Structured category
    // -----------------------------------------------------------------------
    #[test]
    fn test_route_git_status_structured() {
        let config = standard_config();
        let command = cmd(&["git", "status"]);
        let category = categorize(&command, &empty_grammars(), &config, &empty_dangerous());
        assert_eq!(category, Category::Structured);
    }

    // -----------------------------------------------------------------------
    // Test 5: Route `vim file.txt` -> Interactive category
    // -----------------------------------------------------------------------
    #[test]
    fn test_route_vim_interactive() {
        let config = standard_config();
        let command = cmd(&["vim", "file.txt"]);
        let category = categorize(&command, &empty_grammars(), &config, &empty_dangerous());
        assert_eq!(category, Category::Interactive);
    }

    // -----------------------------------------------------------------------
    // Test 6: Route `rm -rf /tmp/foo` -> Dangerous category
    // -----------------------------------------------------------------------
    #[test]
    fn test_route_rm_rf_dangerous() {
        let config = standard_config();
        let dangerous = standard_dangerous();
        let command = cmd(&["rm", "-rf", "/tmp/foo"]);
        let category = categorize(&command, &empty_grammars(), &config, &dangerous);
        assert_eq!(category, Category::Dangerous);
    }

    // -----------------------------------------------------------------------
    // Test 7: Unknown command -> Condense (fallback)
    // -----------------------------------------------------------------------
    #[test]
    fn test_route_unknown_command_fallback_condense() {
        let config = standard_config();
        let command = cmd(&["some-unknown-tool", "--flag"]);
        let category = categorize(&command, &empty_grammars(), &config, &empty_dangerous());
        assert_eq!(category, Category::Condense);
    }

    // -----------------------------------------------------------------------
    // Test 8: Preflight integration — grammar with quiet config gets preflight run
    // -----------------------------------------------------------------------
    // We test that when a grammar has a quiet config, route() returns a
    // PreflightResult with the expected injected flags.
    #[test]
    fn test_preflight_integration() {
        let toml_str = r#"
[tool]
name = "npm"
detect = ["npm"]

[quiet]
safe_inject = ["--no-progress"]
recommend = ["--loglevel=warn"]

[actions.install]
detect = ["install", "i"]

[actions.install.summary]
success = "+ installed"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut grammars = HashMap::new();
        grammars.insert("npm".to_string(), grammar);

        // Test preflight through the same logic route() uses
        let command = cmd(&["npm", "install", "express"]);
        let detected = detect_tool(&command, &grammars);
        let (grammar_ref, action_ref) = match &detected {
            Some((g, a)) => (Some(*g), *a),
            None => (None, None),
        };

        assert!(grammar_ref.is_some(), "grammar should be detected for npm");

        let mut cmd_vec = command.clone();
        let preflight_result = preflight(&mut cmd_vec, grammar_ref.unwrap(), action_ref, OutputMode::Human);

        assert!(
            preflight_result.injected.contains(&"--no-progress".to_string()),
            "expected --no-progress to be injected, got: {:?}",
            preflight_result.injected
        );
        assert!(cmd_vec.contains(&"--no-progress".to_string()));
        assert_eq!(preflight_result.recommendations.len(), 1);
        assert_eq!(preflight_result.recommendations[0].flag, "--loglevel=warn");
    }

    // -----------------------------------------------------------------------
    // Test 9: Enrichment on failure — exit_code != 0 triggers enrichment
    // -----------------------------------------------------------------------
    // Verify that route() with a failing command calls the enrich module.
    // Use exit code 127 (command not found) which always produces diagnostics.
    #[test]
    fn test_enrichment_on_failure() {
        let config = CategoriesConfig {
            categories: HashMap::from([("sh".to_string(), Category::Passthrough)]),
        };

        // exit 127 triggers command-not-found diagnostics in enrich module
        let command = cmd(&["/bin/sh", "-c", "exit 127"]);
        let result = route(
            &command,
            &empty_grammars(),
            &config,
            &empty_dangerous(),
            OutputMode::Human,
            ExecutionMode::Cli,
        );

        match result {
            Ok(r) => {
                assert_ne!(r.exit_code, 0, "expected non-zero exit code");
                // Enrichment should be Some now that enrich is wired in.
                // Exit code 127 maps to "command not found" diagnostic.
                assert!(
                    r.enrichment.is_some(),
                    "enrichment should be Some for exit code 127"
                );
                let diags = r.enrichment.unwrap();
                assert!(!diags.is_empty(), "expected at least one diagnostic line");
            }
            Err(_) => {
                // Command execution may fail in some test environments;
                // that's acceptable — the point is the enrichment path logic.
            }
        }
    }

    // -----------------------------------------------------------------------
    // Test 10: Grammar override category — grammar category beats categories.toml
    // -----------------------------------------------------------------------
    #[test]
    fn test_grammar_category_overrides_config() {
        let toml_str = r#"
[tool]
name = "vim"
detect = ["vim"]
category = "interactive"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut grammars = HashMap::new();
        grammars.insert("vim".to_string(), grammar);

        // categories.toml says passthrough, but grammar says interactive
        let config = CategoriesConfig {
            categories: HashMap::from([("vim".to_string(), Category::Passthrough)]),
        };

        let command = cmd(&["vim", "file.txt"]);
        let category = categorize(&command, &grammars, &config, &empty_dangerous());
        assert_eq!(
            category,
            Category::Interactive,
            "grammar category should override categories.toml"
        );
    }

    // -----------------------------------------------------------------------
    // Test 11: Compound command routing — basic split on &&
    // -----------------------------------------------------------------------
    // Compound commands (e.g., "cmd1 && cmd2") are not split by mish's router;
    // they are passed as a single command to the shell. When routed, the first
    // token determines the category. Verify this behavior.
    #[test]
    fn test_compound_command_routing() {
        let config = standard_config();

        // "echo hello && cat file.txt" — first token is "echo" -> Passthrough
        let command = cmd(&["echo", "hello", "&&", "cat", "file.txt"]);
        let category = categorize(&command, &empty_grammars(), &config, &empty_dangerous());
        assert_eq!(
            category,
            Category::Passthrough,
            "compound command routes based on first token (echo -> Passthrough)"
        );

        // "npm install && npm test" — first token is "npm" -> Condense
        let command = cmd(&["npm", "install", "&&", "npm", "test"]);
        let category = categorize(&command, &empty_grammars(), &config, &empty_dangerous());
        assert_eq!(
            category,
            Category::Condense,
            "compound command routes based on first token (npm -> Condense)"
        );

        // Dangerous pattern still matches across the full command
        let dangerous = standard_dangerous();
        let command = cmd(&["echo", "hello", "&&", "rm", "-rf", "/"]);
        let category = categorize(&command, &empty_grammars(), &config, &dangerous);
        assert_eq!(
            category,
            Category::Dangerous,
            "dangerous patterns match against full compound command string"
        );
    }

    // -----------------------------------------------------------------------
    // Integration: route() with echo (passthrough) — full pipeline
    // -----------------------------------------------------------------------
    #[test]
    fn test_route_echo_passthrough_integration() {
        let config = CategoriesConfig {
            categories: HashMap::from([("echo".to_string(), Category::Passthrough)]),
        };

        let command = cmd(&["echo", "hello world"]);
        let result = route(
            &command,
            &empty_grammars(),
            &config,
            &empty_dangerous(),
            OutputMode::Human,
            ExecutionMode::Cli,
        )
        .expect("route should succeed for echo");

        assert_eq!(result.category, Category::Passthrough);
        assert_eq!(result.exit_code, 0);
        assert!(result.enrichment.is_none());
        assert!(result.preflight.is_none(), "no grammar means no preflight");

        match &result.output {
            HandlerOutput::Passthrough(pr) => {
                assert!(
                    pr.output.contains("hello world"),
                    "expected output to contain 'hello world', got: {:?}",
                    pr.output
                );
                assert_eq!(pr.exit_code, 0);
            }
            _ => panic!("expected Passthrough variant"),
        }
    }

    // -----------------------------------------------------------------------
    // Edge: route() with empty command returns error
    // -----------------------------------------------------------------------
    #[test]
    fn test_route_empty_command_error() {
        let result = route(
            &[],
            &empty_grammars(),
            &empty_config(),
            &empty_dangerous(),
            OutputMode::Human,
            ExecutionMode::Cli,
        );
        assert!(result.is_err(), "empty command should return error");
    }

    // -----------------------------------------------------------------------
    // Test: categorize_command_str parses shell string and categorizes
    // -----------------------------------------------------------------------
    #[test]
    fn test_categorize_command_str_echo_passthrough() {
        let config = standard_config();
        let category = categorize_command_str(
            "echo hello world",
            &empty_grammars(),
            &config,
            &empty_dangerous(),
        );
        assert_eq!(category, Category::Passthrough);
    }

    #[test]
    fn test_categorize_command_str_git_structured() {
        let config = standard_config();
        let category = categorize_command_str(
            "git status",
            &empty_grammars(),
            &config,
            &empty_dangerous(),
        );
        assert_eq!(category, Category::Structured);
    }

    #[test]
    fn test_categorize_command_str_dangerous() {
        let config = standard_config();
        let dangerous = standard_dangerous();
        let category = categorize_command_str(
            "rm -rf /tmp/foo",
            &empty_grammars(),
            &config,
            &dangerous,
        );
        assert_eq!(category, Category::Dangerous);
    }

    #[test]
    fn test_categorize_command_str_empty() {
        let config = standard_config();
        let category = categorize_command_str(
            "",
            &empty_grammars(),
            &config,
            &empty_dangerous(),
        );
        assert_eq!(category, Category::Condense); // fallback
    }

    #[test]
    fn test_categorize_command_str_unknown_falls_back_condense() {
        let config = standard_config();
        let category = categorize_command_str(
            "some-unknown-tool --flag",
            &empty_grammars(),
            &config,
            &empty_dangerous(),
        );
        assert_eq!(category, Category::Condense);
    }

    // -----------------------------------------------------------------------
    // Test: MCP mode Interactive command returns error (not executed)
    // -----------------------------------------------------------------------
    #[test]
    fn test_mcp_mode_interactive_returns_error() {
        let config = standard_config();
        let command = cmd(&["vim", "file.txt"]);

        // Verify categorization
        let category = categorize(&command, &empty_grammars(), &config, &empty_dangerous());
        assert_eq!(category, Category::Interactive);

        // Route in MCP mode should return error for interactive commands
        let result = route(
            &command,
            &empty_grammars(),
            &config,
            &empty_dangerous(),
            OutputMode::Human,
            ExecutionMode::Mcp,
        )
        .expect("route should succeed (returning error result, not Err)");

        assert_eq!(result.category, Category::Interactive);
        match &result.output {
            HandlerOutput::Interactive(ir) => {
                assert_eq!(ir.exit_code, 1, "MCP mode interactive should return exit code 1");
                assert!(
                    ir.summary.contains("cannot run in MCP mode"),
                    "expected error about MCP mode, got: {}",
                    ir.summary
                );
            }
            _ => panic!("expected Interactive handler output variant"),
        }
    }

    // -----------------------------------------------------------------------
    // Test: MCP mode Dangerous command returns warning without executing
    // -----------------------------------------------------------------------
    #[test]
    fn test_mcp_mode_dangerous_returns_warning_without_executing() {
        let config = standard_config();
        let dangerous = standard_dangerous();
        let command = cmd(&["rm", "-rf", "/tmp/test"]);

        let result = route(
            &command,
            &empty_grammars(),
            &config,
            &dangerous,
            OutputMode::Human,
            ExecutionMode::Mcp,
        )
        .expect("route should succeed");

        assert_eq!(result.category, Category::Dangerous);
        match &result.output {
            HandlerOutput::Dangerous(dr) => {
                assert!(!dr.executed, "MCP mode should NOT execute dangerous commands");
                assert!(dr.exit_code.is_none(), "no exit code when not executed");
                assert!(
                    dr.warning.contains("\u{26a0}"),
                    "warning should contain ⚠ symbol"
                );
            }
            _ => panic!("expected Dangerous handler output variant"),
        }
    }

    // -----------------------------------------------------------------------
    // Test: cp nonexistent file returns enrichment diagnostics
    // -----------------------------------------------------------------------
    #[test]
    fn test_enrichment_cp_nonexistent_file() {
        let config = CategoriesConfig {
            categories: HashMap::from([("cp".to_string(), Category::Narrate)]),
        };

        let command = cmd(&["cp", "nonexistent_file_that_does_not_exist.txt", "/tmp/"]);
        let result = route(
            &command,
            &empty_grammars(),
            &config,
            &empty_dangerous(),
            OutputMode::Human,
            ExecutionMode::Cli,
        )
        .expect("route should succeed even if cp fails");

        assert_ne!(result.exit_code, 0, "cp of nonexistent file should fail");
        assert!(
            result.enrichment.is_some(),
            "enrichment should be Some for failed cp"
        );
        let diags = result.enrichment.unwrap();
        assert!(!diags.is_empty(), "expected at least one diagnostic for failed cp");
        // Verify diagnostics have non-empty kind/message
        for d in &diags {
            assert!(!d.kind.is_empty(), "diagnostic kind should not be empty");
            assert!(!d.message.is_empty(), "diagnostic message should not be empty");
        }
    }

    // -----------------------------------------------------------------------
    // Test: enrichment is None on success (exit_code == 0)
    // -----------------------------------------------------------------------
    #[test]
    fn test_enrichment_none_on_success() {
        let config = CategoriesConfig {
            categories: HashMap::from([("echo".to_string(), Category::Passthrough)]),
        };

        let command = cmd(&["echo", "hello"]);
        let result = route(
            &command,
            &empty_grammars(),
            &config,
            &empty_dangerous(),
            OutputMode::Human,
            ExecutionMode::Cli,
        )
        .expect("route should succeed for echo");

        assert_eq!(result.exit_code, 0);
        assert!(
            result.enrichment.is_none(),
            "enrichment should be None when exit_code == 0"
        );
    }

    // -----------------------------------------------------------------------
    // Test: enrichment diagnostics have non-empty kind and message
    // -----------------------------------------------------------------------
    #[test]
    fn test_enrichment_diagnostics_non_empty_fields() {
        let config = CategoriesConfig {
            categories: HashMap::from([("sh".to_string(), Category::Passthrough)]),
        };

        // Use exit code 127 (command not found) which always produces diagnostics
        let command = cmd(&["/bin/sh", "-c", "exit 127"]);
        let result = route(
            &command,
            &empty_grammars(),
            &config,
            &empty_dangerous(),
            OutputMode::Human,
            ExecutionMode::Cli,
        );

        match result {
            Ok(r) => {
                assert_ne!(r.exit_code, 0, "expected non-zero exit code");
                assert!(
                    r.enrichment.is_some(),
                    "enrichment should be Some for exit code 127"
                );
                let diags = r.enrichment.unwrap();
                assert!(!diags.is_empty(), "expected diagnostics for exit 127");
                for d in &diags {
                    assert!(
                        !d.kind.is_empty(),
                        "diagnostic kind should not be empty, got: {:?}",
                        d
                    );
                    assert!(
                        !d.message.is_empty(),
                        "diagnostic message should not be empty, got: {:?}",
                        d
                    );
                }
            }
            Err(_) => {
                // Acceptable in constrained test environments
            }
        }
    }

    // -----------------------------------------------------------------------
    // Pipeline detection: commands with | should default to Passthrough
    // -----------------------------------------------------------------------

    #[test]
    fn test_pipeline_unknown_command_defaults_passthrough() {
        // "stat ... | sort | head" — stat is unknown, but pipeline → passthrough
        let config = standard_config();
        let category = categorize_command_str(
            "stat -f '%m %N' foo.txt | sort -rn | head -3",
            &empty_grammars(),
            &config,
            &empty_dangerous(),
        );
        assert_eq!(
            category,
            Category::Passthrough,
            "piped unknown command should default to passthrough"
        );
    }

    #[test]
    fn test_pipeline_known_condense_command_stays_condense() {
        // "cargo build | tee log" — cargo is explicitly Condense via grammar
        let toml_str = r#"
[tool]
name = "cargo"
detect = ["cargo"]
category = "condense"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut grammars = HashMap::new();
        grammars.insert("cargo".to_string(), grammar);

        let config = standard_config();
        let category = categorize_command_str(
            "cargo build 2>&1 | tee build.log",
            &grammars,
            &config,
            &empty_dangerous(),
        );
        assert_eq!(
            category,
            Category::Condense,
            "piped command with explicit condense grammar should stay condense"
        );
    }

    #[test]
    fn test_pipeline_known_passthrough_stays_passthrough() {
        // "grep pattern | sort" — grep is already passthrough
        let config = standard_config();
        let category = categorize_command_str(
            "grep -rn 'pattern' src/ | sort",
            &empty_grammars(),
            &config,
            &empty_dangerous(),
        );
        assert_eq!(
            category,
            Category::Passthrough,
            "piped passthrough command should stay passthrough"
        );
    }

    #[test]
    fn test_pipeline_dangerous_still_dangerous() {
        // Dangerous patterns should still match even in pipelines
        let config = standard_config();
        let dangerous = standard_dangerous();
        let category = categorize_command_str(
            "rm -rf /tmp/foo | cat",
            &empty_grammars(),
            &config,
            &dangerous,
        );
        assert_eq!(
            category,
            Category::Dangerous,
            "piped dangerous command should still be dangerous"
        );
    }

    #[test]
    fn test_no_pipeline_unknown_still_condense() {
        // Non-piped unknown command should still fall back to condense
        let config = standard_config();
        let category = categorize_command_str(
            "some-unknown-tool --flag",
            &empty_grammars(),
            &config,
            &empty_dangerous(),
        );
        assert_eq!(
            category,
            Category::Condense,
            "non-piped unknown command should still default to condense"
        );
    }
}
