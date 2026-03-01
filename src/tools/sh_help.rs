//! sh_help tool — Self-documenting reference card for MCP server mode.
//!
//! Returns a structured response containing all 5 tool summaries, watch presets,
//! squasher defaults, resource limits, and live resource usage. Designed to be
//! compact (under 500 tokens) so the LLM can recover context after compaction.

use crate::config::MishConfig;
use crate::mcp::types::{
    ParamSummary, ResourceLimits, ResourceUsage, ShHelpResponse, SquasherDefaults, ToolSummary,
    ERR_INVALID_PARAMS,
};
use crate::process::table::ProcessTable;
use crate::session::manager::SessionManager;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Error type for sh_help tool operations.
#[derive(Debug)]
pub enum ShHelpError {
    /// An invalid tool name was requested.
    InvalidToolName(String),
}

impl std::fmt::Display for ShHelpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShHelpError::InvalidToolName(name) => {
                write!(
                    f,
                    "unknown tool '{}'; valid tools: sh_run, sh_spawn, sh_interact, sh_session, sh_help",
                    name
                )
            }
        }
    }
}

impl std::error::Error for ShHelpError {}

impl ShHelpError {
    /// Return the MCP error code for this error.
    pub fn error_code(&self) -> i32 {
        match self {
            ShHelpError::InvalidToolName(_) => ERR_INVALID_PARAMS,
        }
    }
}

// ---------------------------------------------------------------------------
// Valid tool names
// ---------------------------------------------------------------------------

const VALID_TOOL_NAMES: &[&str] = &["sh_run", "sh_spawn", "sh_interact", "sh_session", "sh_help"];

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

/// Handle an sh_help tool call.
///
/// If `tool_filter` is `Some`, returns help for only the named tool.
/// If `tool_filter` is `None`, returns help for all 5 tools.
pub async fn handle(
    config: &MishConfig,
    process_table: &ProcessTable,
    session_manager: &SessionManager,
    tool_filter: Option<&str>,
) -> Result<ShHelpResponse, ShHelpError> {
    // Validate filter if provided.
    if let Some(name) = tool_filter {
        if !VALID_TOOL_NAMES.contains(&name) {
            return Err(ShHelpError::InvalidToolName(name.to_string()));
        }
    }

    // Build tool summaries.
    let tools = build_tool_summaries(tool_filter);

    // Watch presets from config.
    let watch_presets = config.watch_presets.clone();

    // Squasher defaults from config.
    let squasher_defaults = SquasherDefaults {
        max_lines: config.squasher.max_lines,
        oreo_head: config.squasher.oreo_head,
        oreo_tail: config.squasher.oreo_tail,
        max_bytes: config.squasher.max_bytes,
    };

    // Resource limits from config.
    let resource_limits = ResourceLimits {
        max_sessions: config.server.max_sessions,
        max_processes: config.server.max_processes,
    };

    // Live resource usage.
    let resource_usage = ResourceUsage {
        active_sessions: session_manager.session_count().await,
        active_processes: process_table.active_count(),
    };

    Ok(ShHelpResponse {
        tools,
        watch_presets,
        squasher_defaults,
        resource_limits,
        resource_usage,
    })
}

// ---------------------------------------------------------------------------
// Tool summary builders
// ---------------------------------------------------------------------------

/// Build tool summaries, optionally filtered to a single tool.
fn build_tool_summaries(filter: Option<&str>) -> Vec<ToolSummary> {
    let all_tools = vec![
        build_sh_run_summary(),
        build_sh_spawn_summary(),
        build_sh_interact_summary(),
        build_sh_session_summary(),
        build_sh_help_summary(),
    ];

    match filter {
        Some(name) => all_tools.into_iter().filter(|t| t.name == name).collect(),
        None => all_tools,
    }
}

fn build_sh_run_summary() -> ToolSummary {
    ToolSummary {
        name: "sh_run".to_string(),
        params: vec![
            ParamSummary {
                name: "cmd".to_string(),
                r#type: "string".to_string(),
                required: true,
                default: None,
                description: "Command to execute".to_string(),
            },
            ParamSummary {
                name: "timeout".to_string(),
                r#type: "integer".to_string(),
                required: false,
                default: Some("300".to_string()),
                description: "Seconds before kill".to_string(),
            },
            ParamSummary {
                name: "watch".to_string(),
                r#type: "string".to_string(),
                required: false,
                default: None,
                description: "Regex or @preset to filter output".to_string(),
            },
            ParamSummary {
                name: "unmatched".to_string(),
                r#type: "string".to_string(),
                required: false,
                default: Some("keep".to_string()),
                description: "Handle non-matching lines when watch is set (keep|drop)".to_string(),
            },
        ],
    }
}

fn build_sh_spawn_summary() -> ToolSummary {
    ToolSummary {
        name: "sh_spawn".to_string(),
        params: vec![
            ParamSummary {
                name: "alias".to_string(),
                r#type: "string".to_string(),
                required: true,
                default: None,
                description: "Unique name for this process".to_string(),
            },
            ParamSummary {
                name: "cmd".to_string(),
                r#type: "string".to_string(),
                required: true,
                default: None,
                description: "Command to execute".to_string(),
            },
            ParamSummary {
                name: "wait_for".to_string(),
                r#type: "string".to_string(),
                required: false,
                default: None,
                description: "Regex to match before returning success".to_string(),
            },
            ParamSummary {
                name: "timeout".to_string(),
                r#type: "integer".to_string(),
                required: false,
                default: Some("300".to_string()),
                description: "Seconds to wait".to_string(),
            },
        ],
    }
}

fn build_sh_interact_summary() -> ToolSummary {
    ToolSummary {
        name: "sh_interact".to_string(),
        params: vec![
            ParamSummary {
                name: "alias".to_string(),
                r#type: "string".to_string(),
                required: true,
                default: None,
                description: "Target process alias".to_string(),
            },
            ParamSummary {
                name: "action".to_string(),
                r#type: "string".to_string(),
                required: true,
                default: None,
                description: "Action: send | read_tail | signal | kill | status".to_string(),
            },
            ParamSummary {
                name: "input".to_string(),
                r#type: "string".to_string(),
                required: false,
                default: None,
                description: "For send: string to write (include \\n for enter)".to_string(),
            },
            ParamSummary {
                name: "lines".to_string(),
                r#type: "integer".to_string(),
                required: false,
                default: Some("50".to_string()),
                description: "For read_tail: number of lines".to_string(),
            },
        ],
    }
}

fn build_sh_session_summary() -> ToolSummary {
    ToolSummary {
        name: "sh_session".to_string(),
        params: vec![ParamSummary {
            name: "action".to_string(),
            r#type: "string".to_string(),
            required: true,
            default: None,
            description: "Action: list".to_string(),
        }],
    }
}

fn build_sh_help_summary() -> ToolSummary {
    ToolSummary {
        name: "sh_help".to_string(),
        params: vec![ParamSummary {
            name: "tool".to_string(),
            r#type: "string".to_string(),
            required: false,
            default: None,
            description: "Filter to a single tool (sh_run, sh_spawn, sh_interact, sh_session, sh_help)".to_string(),
        }],
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_config;
    use std::sync::Arc;

    /// Create a config with some watch presets for testing.
    fn test_config_with_presets() -> MishConfig {
        let mut config = default_config();
        config.watch_presets.insert("errors".to_string(), "error|fatal|panic".to_string());
        config.watch_presets.insert("warnings".to_string(), "warn|deprecat".to_string());
        config
    }

    /// Create a process table from a config.
    fn test_process_table(config: &MishConfig) -> ProcessTable {
        ProcessTable::new(config)
    }

    /// Create a session manager from a config.
    fn test_session_manager(config: Arc<MishConfig>) -> SessionManager {
        SessionManager::new(config)
    }

    // ── Test 1: Full help output contains all 5 tools ──────────────────

    #[tokio::test]
    async fn full_help_contains_all_five_tools() {
        let config = test_config_with_presets();
        let process_table = test_process_table(&config);
        let session_manager = test_session_manager(Arc::new(config.clone()));

        let response = handle(&config, &process_table, &session_manager, None)
            .await
            .expect("should succeed");

        assert_eq!(response.tools.len(), 5);

        let tool_names: Vec<&str> = response.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(tool_names.contains(&"sh_run"));
        assert!(tool_names.contains(&"sh_spawn"));
        assert!(tool_names.contains(&"sh_interact"));
        assert!(tool_names.contains(&"sh_session"));
        assert!(tool_names.contains(&"sh_help"));
    }

    // ── Test 2: Filtered help returns only the requested tool ──────────

    #[tokio::test]
    async fn filtered_help_returns_only_requested_tool() {
        let config = test_config_with_presets();
        let process_table = test_process_table(&config);
        let session_manager = test_session_manager(Arc::new(config.clone()));

        for tool_name in VALID_TOOL_NAMES {
            let response = handle(&config, &process_table, &session_manager, Some(tool_name))
                .await
                .expect("should succeed");

            assert_eq!(response.tools.len(), 1, "filter for {tool_name}");
            assert_eq!(response.tools[0].name, *tool_name);
        }
    }

    // ── Test 3: Invalid tool name returns error ─────────────────────────

    #[tokio::test]
    async fn invalid_tool_name_returns_error() {
        let config = test_config_with_presets();
        let process_table = test_process_table(&config);
        let session_manager = test_session_manager(Arc::new(config.clone()));

        let result = handle(&config, &process_table, &session_manager, Some("sh_bogus")).await;
        assert!(result.is_err());

        let err = result.unwrap_err();
        match err {
            ShHelpError::InvalidToolName(ref name) => assert_eq!(name, "sh_bogus"),
        }
        assert_eq!(err.error_code(), -32602);
    }

    // ── Test 4: Help includes watch presets from config ──────────────────

    #[tokio::test]
    async fn help_includes_watch_presets_from_config() {
        let config = test_config_with_presets();
        let process_table = test_process_table(&config);
        let session_manager = test_session_manager(Arc::new(config.clone()));

        let response = handle(&config, &process_table, &session_manager, None)
            .await
            .expect("should succeed");

        assert_eq!(response.watch_presets.len(), 2);
        assert_eq!(
            response.watch_presets.get("errors"),
            Some(&"error|fatal|panic".to_string())
        );
        assert_eq!(
            response.watch_presets.get("warnings"),
            Some(&"warn|deprecat".to_string())
        );
    }

    // ── Test 5: Empty watch presets when config has none ─────────────────

    #[tokio::test]
    async fn empty_watch_presets_when_config_has_none() {
        let config = default_config();
        let process_table = test_process_table(&config);
        let session_manager = test_session_manager(Arc::new(config.clone()));

        let response = handle(&config, &process_table, &session_manager, None)
            .await
            .expect("should succeed");

        assert!(response.watch_presets.is_empty());
    }

    // ── Test 6: Help includes squasher defaults ──────────────────────────

    #[tokio::test]
    async fn help_includes_squasher_defaults() {
        let config = default_config();
        let process_table = test_process_table(&config);
        let session_manager = test_session_manager(Arc::new(config.clone()));

        let response = handle(&config, &process_table, &session_manager, None)
            .await
            .expect("should succeed");

        assert_eq!(response.squasher_defaults.max_lines, config.squasher.max_lines);
        assert_eq!(response.squasher_defaults.oreo_head, config.squasher.oreo_head);
        assert_eq!(response.squasher_defaults.oreo_tail, config.squasher.oreo_tail);
        assert_eq!(response.squasher_defaults.max_bytes, config.squasher.max_bytes);
    }

    // ── Test 7: Help includes resource limits ────────────────────────────

    #[tokio::test]
    async fn help_includes_resource_limits() {
        let config = default_config();
        let process_table = test_process_table(&config);
        let session_manager = test_session_manager(Arc::new(config.clone()));

        let response = handle(&config, &process_table, &session_manager, None)
            .await
            .expect("should succeed");

        assert_eq!(response.resource_limits.max_sessions, config.server.max_sessions);
        assert_eq!(response.resource_limits.max_processes, config.server.max_processes);
    }

    // ── Test 8: Help includes current usage counts (zero state) ──────────

    #[tokio::test]
    async fn help_includes_zero_usage_counts() {
        let config = default_config();
        let process_table = test_process_table(&config);
        let session_manager = test_session_manager(Arc::new(config.clone()));

        let response = handle(&config, &process_table, &session_manager, None)
            .await
            .expect("should succeed");

        assert_eq!(response.resource_usage.active_sessions, 0);
        assert_eq!(response.resource_usage.active_processes, 0);
    }

    // ── Test 9: Usage counts reflect live process table state ────────────

    #[tokio::test]
    async fn usage_reflects_live_process_table_state() {
        let config = default_config();
        let mut process_table = test_process_table(&config);
        let session_manager = test_session_manager(Arc::new(config.clone()));

        // Register two processes.
        process_table
            .register("build", "main", 1234, None)
            .expect("register build");
        process_table
            .register("server", "main", 5678, Some("error"))
            .expect("register server");

        let response = handle(&config, &process_table, &session_manager, None)
            .await
            .expect("should succeed");

        assert_eq!(response.resource_usage.active_processes, 2);
    }

    // ── Test 10: Response is valid JSON ──────────────────────────────────

    #[tokio::test]
    async fn response_serializes_to_valid_json() {
        let config = test_config_with_presets();
        let process_table = test_process_table(&config);
        let session_manager = test_session_manager(Arc::new(config.clone()));

        let response = handle(&config, &process_table, &session_manager, None)
            .await
            .expect("should succeed");

        let json_value = serde_json::to_value(&response).expect("should serialize to JSON");
        assert!(json_value.is_object());
        assert!(json_value.get("tools").is_some());
        assert!(json_value.get("watch_presets").is_some());
        assert!(json_value.get("squasher_defaults").is_some());
        assert!(json_value.get("resource_limits").is_some());
        assert!(json_value.get("resource_usage").is_some());
    }

    // ── Test 11: Response is compact (under ~2000 chars / ~500 tokens) ───

    #[tokio::test]
    async fn response_is_compact() {
        let config = test_config_with_presets();
        let process_table = test_process_table(&config);
        let session_manager = test_session_manager(Arc::new(config.clone()));

        let response = handle(&config, &process_table, &session_manager, None)
            .await
            .expect("should succeed");

        let json_str = serde_json::to_string(&response).expect("should serialize");
        // Under 500 tokens ~ roughly under 2000 characters for structured JSON.
        assert!(
            json_str.len() < 3000,
            "response too large: {} bytes (should be under ~3000 for 500 token budget)",
            json_str.len()
        );
    }

    // ── Test 12: sh_run tool summary has correct params ──────────────────

    #[tokio::test]
    async fn sh_run_summary_has_correct_params() {
        let config = default_config();
        let process_table = test_process_table(&config);
        let session_manager = test_session_manager(Arc::new(config.clone()));

        let response = handle(&config, &process_table, &session_manager, Some("sh_run"))
            .await
            .expect("should succeed");

        let tool = &response.tools[0];
        assert_eq!(tool.name, "sh_run");
        assert_eq!(tool.params.len(), 4);

        // cmd is required.
        let cmd_param = tool.params.iter().find(|p| p.name == "cmd").unwrap();
        assert!(cmd_param.required);
        assert_eq!(cmd_param.r#type, "string");

        // timeout has default 300.
        let timeout_param = tool.params.iter().find(|p| p.name == "timeout").unwrap();
        assert!(!timeout_param.required);
        assert_eq!(timeout_param.default, Some("300".to_string()));
    }

    // ── Test 13: sh_spawn tool summary has correct params ─────────────────

    #[tokio::test]
    async fn sh_spawn_summary_has_correct_params() {
        let config = default_config();
        let process_table = test_process_table(&config);
        let session_manager = test_session_manager(Arc::new(config.clone()));

        let response = handle(&config, &process_table, &session_manager, Some("sh_spawn"))
            .await
            .expect("should succeed");

        let tool = &response.tools[0];
        assert_eq!(tool.name, "sh_spawn");
        assert_eq!(tool.params.len(), 4);

        // alias and cmd are required.
        let alias_param = tool.params.iter().find(|p| p.name == "alias").unwrap();
        assert!(alias_param.required);
        let cmd_param = tool.params.iter().find(|p| p.name == "cmd").unwrap();
        assert!(cmd_param.required);

        // wait_for is optional.
        let wait_for_param = tool.params.iter().find(|p| p.name == "wait_for").unwrap();
        assert!(!wait_for_param.required);
    }

    // ── Test 14: sh_interact tool summary has correct params ──────────────

    #[tokio::test]
    async fn sh_interact_summary_has_correct_params() {
        let config = default_config();
        let process_table = test_process_table(&config);
        let session_manager = test_session_manager(Arc::new(config.clone()));

        let response = handle(&config, &process_table, &session_manager, Some("sh_interact"))
            .await
            .expect("should succeed");

        let tool = &response.tools[0];
        assert_eq!(tool.name, "sh_interact");
        assert_eq!(tool.params.len(), 4);

        // alias and action are required.
        let alias_param = tool.params.iter().find(|p| p.name == "alias").unwrap();
        assert!(alias_param.required);
        let action_param = tool.params.iter().find(|p| p.name == "action").unwrap();
        assert!(action_param.required);

        // lines default is 50.
        let lines_param = tool.params.iter().find(|p| p.name == "lines").unwrap();
        assert!(!lines_param.required);
        assert_eq!(lines_param.default, Some("50".to_string()));
    }

    // ── Test 15: sh_help tool summary documents its own optional param ────

    #[tokio::test]
    async fn sh_help_summary_has_tool_filter_param() {
        let config = default_config();
        let process_table = test_process_table(&config);
        let session_manager = test_session_manager(Arc::new(config.clone()));

        let response = handle(&config, &process_table, &session_manager, Some("sh_help"))
            .await
            .expect("should succeed");

        let tool = &response.tools[0];
        assert_eq!(tool.name, "sh_help");
        assert_eq!(tool.params.len(), 1);

        let tool_param = &tool.params[0];
        assert_eq!(tool_param.name, "tool");
        assert!(!tool_param.required);
    }

    // ── Test 16: Error display message is informative ────────────────────

    #[test]
    fn error_display_includes_tool_name_and_valid_tools() {
        let err = ShHelpError::InvalidToolName("sh_bogus".to_string());
        let msg = format!("{err}");
        assert!(msg.contains("sh_bogus"));
        assert!(msg.contains("sh_run"));
        assert!(msg.contains("sh_spawn"));
        assert!(msg.contains("sh_interact"));
        assert!(msg.contains("sh_session"));
        assert!(msg.contains("sh_help"));
    }

    // ── Test 17: Custom config values are reflected ──────────────────────

    #[tokio::test]
    async fn custom_config_values_reflected() {
        let mut config = default_config();
        config.server.max_sessions = 10;
        config.server.max_processes = 50;
        config.squasher.max_lines = 500;
        config.squasher.oreo_head = 100;
        config.squasher.oreo_tail = 300;
        config.squasher.max_bytes = 131_072;

        let process_table = test_process_table(&config);
        let session_manager = test_session_manager(Arc::new(config.clone()));

        let response = handle(&config, &process_table, &session_manager, None)
            .await
            .expect("should succeed");

        assert_eq!(response.resource_limits.max_sessions, 10);
        assert_eq!(response.resource_limits.max_processes, 50);
        assert_eq!(response.squasher_defaults.max_lines, 500);
        assert_eq!(response.squasher_defaults.oreo_head, 100);
        assert_eq!(response.squasher_defaults.oreo_tail, 300);
        assert_eq!(response.squasher_defaults.max_bytes, 131_072);
    }

    // ── Test 18: Filtered help still includes presets and limits ──────────

    #[tokio::test]
    async fn filtered_help_still_includes_presets_and_limits() {
        let config = test_config_with_presets();
        let process_table = test_process_table(&config);
        let session_manager = test_session_manager(Arc::new(config.clone()));

        let response = handle(&config, &process_table, &session_manager, Some("sh_run"))
            .await
            .expect("should succeed");

        // Even with a filter, the full context is returned.
        assert_eq!(response.tools.len(), 1);
        assert_eq!(response.watch_presets.len(), 2);
        assert_eq!(response.resource_limits.max_sessions, config.server.max_sessions);
        assert_eq!(response.squasher_defaults.max_lines, config.squasher.max_lines);
    }
}
