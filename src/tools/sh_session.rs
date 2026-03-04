//! `sh_session` MCP tool — session lifecycle management.
//!
//! Provides create, list, and close actions for managing shell sessions.
//! Wraps `SessionManager` for all session operations.

use serde::Deserialize;
use serde_json::json;

use crate::audit::logger::read_session_entries;
use crate::config::AuditConfig;
use crate::mcp::types::{
    self as mcp_types, SessionAction, ShSessionListResponse,
};
use crate::process::table::ProcessTable;
use crate::session::manager::SessionManager;
use super::ToolError;

// ---------------------------------------------------------------------------
// Extended params (superset of mcp::types::ShSessionParams)
// ---------------------------------------------------------------------------

/// Parameters for the sh_session tool.
///
/// Extends the base `ShSessionParams` with `name` and `shell` fields needed
/// for create and close actions. `last` and `format` are for the audit action.
#[derive(Debug, Clone, Deserialize)]
pub struct ShSessionParams {
    pub action: SessionAction,
    pub name: Option<String>,
    pub shell: Option<String>,
    /// For audit action: return only the last N command records.
    pub last: Option<usize>,
    /// For audit action: "summary" returns session-end-style aggregate.
    pub format: Option<String>,
}

/// Context for reading audit logs.
pub struct AuditContext<'a> {
    pub config: &'a AuditConfig,
    pub session_id: &'a str,
}

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

/// Handle an `sh_session` tool call.
///
/// Dispatches to create, list, close, or audit based on the `action` parameter.
/// Returns a JSON value containing the result plus a process digest.
pub async fn handle(
    params: ShSessionParams,
    session_manager: &SessionManager,
    process_table: &ProcessTable,
    audit_ctx: Option<&AuditContext<'_>>,
) -> Result<serde_json::Value, ToolError> {
    let result = match params.action {
        SessionAction::Create => handle_create(params, session_manager).await?,
        SessionAction::List => handle_list(session_manager, process_table).await?,
        SessionAction::Close => handle_close(params, session_manager).await?,
        SessionAction::Audit => handle_audit(&params, audit_ctx).await?,
    };

    Ok(result)
}

// ---------------------------------------------------------------------------
// Action handlers
// ---------------------------------------------------------------------------

/// Handle `action: "create"` — create a new named session.
async fn handle_create(
    params: ShSessionParams,
    session_manager: &SessionManager,
) -> Result<serde_json::Value, ToolError> {
    let name = params.name.as_deref().ok_or_else(|| {
        ToolError::invalid_params("name is required for create action")
    })?;

    let shell_path = params.shell.as_deref();
    let info = session_manager.create_session(name, shell_path).await?;

    // Determine the shell path used. If the user provided one, use that.
    // Otherwise, use $SHELL or /bin/sh (matching SessionManager's logic).
    let shell = params
        .shell
        .unwrap_or_else(|| {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
        });

    Ok(json!({
        "session": info.name,
        "shell": shell,
        "cwd": info.cwd,
        "pid": info.pid,
        "ready": info.ready,
    }))
}

/// Handle `action: "list"` — list all sessions with stats.
async fn handle_list(
    session_manager: &SessionManager,
    process_table: &ProcessTable,
) -> Result<serde_json::Value, ToolError> {
    let sessions = session_manager.list_sessions().await;

    // Build the response using the MCP types SessionInfo.
    // Note: SessionManager's SessionInfo doesn't track the shell path,
    // so we use $SHELL or /bin/sh as the default. For the total active
    // processes, we use the ProcessTable's active_count (since we can't
    // iterate per-session without modifying ProcessTable).
    let default_shell =
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

    let total_active = process_table.active_count();
    let session_count = sessions.len();

    let session_infos: Vec<mcp_types::SessionInfo> = sessions
        .into_iter()
        .map(|s| {
            // Distribute active processes: in MVP (single session), all
            // active processes belong to the main session. For multiple
            // sessions, this is an approximation until ProcessTable
            // supports per-session counts.
            let active = if session_count == 1 {
                total_active
            } else {
                0
            };

            mcp_types::SessionInfo {
                session: s.name,
                shell: default_shell.clone(),
                cwd: s.cwd,
                active_processes: active,
            }
        })
        .collect();

    let resp = ShSessionListResponse {
        sessions: session_infos,
    };

    Ok(serde_json::to_value(&resp).expect("ShSessionListResponse serialization"))
}

/// Handle `action: "audit"` — read audit entries for the current session.
///
/// Returns JSONL entries from the session audit log.
/// - Default: all CommandRecord entries
/// - `last=N`: last N CommandRecord entries
/// - `format="summary"`: session-end-style aggregate of all CommandRecords
async fn handle_audit(
    params: &ShSessionParams,
    audit_ctx: Option<&AuditContext<'_>>,
) -> Result<serde_json::Value, ToolError> {
    let ctx = audit_ctx.ok_or_else(|| {
        ToolError::internal("audit context not available")
    })?;

    let all_entries = read_session_entries(ctx.config, ctx.session_id);

    // Filter to CommandRecord entries only
    let records: Vec<&serde_json::Value> = all_entries
        .iter()
        .filter(|e| e.get("event").and_then(|ev| ev.get("type")).and_then(|t| t.as_str()) == Some("CommandRecord"))
        .collect();

    match params.format.as_deref() {
        Some("summary") => {
            // Compute live aggregate (same shape as SessionEnd)
            let total_commands = records.len() as u64;
            let mut total_raw_bytes: u64 = 0;
            let mut total_squashed_bytes: u64 = 0;
            let mut grammars: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut total_wall_ms: u64 = 0;

            for r in &records {
                let ev = &r["event"];
                total_raw_bytes += ev["raw_bytes"].as_u64().unwrap_or(0);
                total_squashed_bytes += ev["squashed_bytes"].as_u64().unwrap_or(0);
                total_wall_ms += ev["wall_ms"].as_u64().unwrap_or(0);
                if let Some(g) = ev["grammar"].as_str() {
                    grammars.insert(g.to_string());
                }
            }

            let aggregate_ratio = if total_squashed_bytes == 0 {
                0.0
            } else {
                total_raw_bytes as f64 / total_squashed_bytes as f64
            };

            let mut grammars_vec: Vec<String> = grammars.into_iter().collect();
            grammars_vec.sort();

            Ok(json!({
                "total_commands": total_commands,
                "total_raw_bytes": total_raw_bytes,
                "total_squashed_bytes": total_squashed_bytes,
                "aggregate_ratio": aggregate_ratio,
                "grammars_used": grammars_vec,
                "total_wall_ms": total_wall_ms,
            }))
        }
        _ => {
            // Return entries, optionally limited to last N
            let to_return: Vec<&serde_json::Value> = if let Some(n) = params.last {
                let skip = records.len().saturating_sub(n);
                records.into_iter().skip(skip).collect()
            } else {
                records
            };

            Ok(json!({
                "entries": to_return,
                "count": to_return.len(),
            }))
        }
    }
}

/// Handle `action: "close"` — close a named session and kill its processes.
async fn handle_close(
    params: ShSessionParams,
    session_manager: &SessionManager,
) -> Result<serde_json::Value, ToolError> {
    let name = params.name.as_deref().ok_or_else(|| {
        ToolError::invalid_params("name is required for close action")
    })?;

    session_manager.close_session(name).await?;

    Ok(json!({
        "session": name,
        "closed": true,
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{default_config, MishConfig};
    use crate::mcp::types::{SessionAction, ERR_INVALID_PARAMS};
    use crate::session::manager::SessionError;
    use serial_test::serial;
    use std::sync::Arc;

    fn bash_path() -> &'static str {
        "/bin/bash"
    }

    fn test_config() -> Arc<MishConfig> {
        Arc::new(default_config())
    }

    fn test_config_with_max(max_sessions: usize) -> Arc<MishConfig> {
        let mut config = default_config();
        config.server.max_sessions = max_sessions;
        Arc::new(config)
    }

    fn test_process_table() -> ProcessTable {
        let config = default_config();
        ProcessTable::new(&config)
    }

    /// Shared helper: creates a SessionManager with one named session.
    /// Reduces boilerplate for tests that need a pre-existing session.
    async fn shared_mgr_with_session(name: &str) -> SessionManager {
        let mgr = SessionManager::new(test_config());
        mgr.create_session(name, Some(bash_path()))
            .await
            .expect("shared session");
        mgr
    }

    // ------------------------------------------------------------------
    // Test 1: Create session
    // ------------------------------------------------------------------

    #[tokio::test]
    #[serial(pty)]
    async fn test_create_session() {
        let mgr = SessionManager::new(test_config());
        let pt = test_process_table();

        let params = ShSessionParams {
            action: SessionAction::Create,
            name: Some("test-session".to_string()),
            shell: Some("/bin/bash".to_string()),
            last: None,
            format: None,
        };

        let result = handle(params, &mgr, &pt, None).await;
        assert!(result.is_ok(), "create should succeed: {result:?}");

        let val = result.unwrap();
        assert_eq!(val["session"], "test-session");
        assert_eq!(val["shell"], "/bin/bash");
        assert!(val["pid"].as_u64().unwrap() > 0);
        assert_eq!(val["ready"], true);
        assert!(val["cwd"].as_str().is_some());

        mgr.close_all().await;
    }

    // ------------------------------------------------------------------
    // Test 2: List sessions
    // ------------------------------------------------------------------

    #[tokio::test]
    #[serial(pty)]
    async fn test_list_sessions() {
        let mgr = shared_mgr_with_session("main").await;
        let pt = test_process_table();

        let params = ShSessionParams {
            action: SessionAction::List,
            name: None,
            shell: None,
            last: None,
            format: None,
        };

        let result = handle(params, &mgr, &pt, None).await;
        assert!(result.is_ok(), "list should succeed: {result:?}");

        let val = result.unwrap();
        let sessions = val["sessions"].as_array().expect("sessions array");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["session"], "main");
        assert!(sessions[0]["cwd"].as_str().is_some());
        assert!(sessions[0]["shell"].as_str().is_some());
        assert_eq!(sessions[0]["active_processes"], 0);

        mgr.close_all().await;
    }

    // ------------------------------------------------------------------
    // Test 3: Close session
    // ------------------------------------------------------------------

    #[tokio::test]
    #[serial(pty)]
    async fn test_close_session() {
        let mgr = shared_mgr_with_session("ephemeral").await;
        let pt = test_process_table();

        let params = ShSessionParams {
            action: SessionAction::Close,
            name: Some("ephemeral".to_string()),
            shell: None,
            last: None,
            format: None,
        };

        let result = handle(params, &mgr, &pt, None).await;
        assert!(result.is_ok(), "close should succeed: {result:?}");

        let val = result.unwrap();
        assert_eq!(val["session"], "ephemeral");
        assert_eq!(val["closed"], true);

        // Verify it's gone.
        assert!(mgr.get_session("ephemeral").await.is_none());
    }

    // ------------------------------------------------------------------
    // Test 4: Close nonexistent session (error)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn test_close_nonexistent_session() {
        let mgr = SessionManager::new(test_config());
        let pt = test_process_table();

        let params = ShSessionParams {
            action: SessionAction::Close,
            name: Some("ghost".to_string()),
            shell: None,
            last: None,
            format: None,
        };

        let result = handle(params, &mgr, &pt, None).await;
        assert!(result.is_err(), "close nonexistent should fail");

        let err = result.unwrap_err();
        assert_eq!(err.code, -32002, "error code should be -32002 (not found)");
        assert!(
            err.message.contains("not found"),
            "error message should mention 'not found', got: {}",
            err.message
        );
    }

    // ------------------------------------------------------------------
    // Test 5: Create with limit reached (error)
    // ------------------------------------------------------------------

    #[tokio::test]
    #[serial(pty)]
    async fn test_create_limit_reached() {
        let mgr = SessionManager::new(test_config_with_max(1));
        let pt = test_process_table();

        // Fill the limit.
        mgr.create_session("first", Some(bash_path()))
            .await
            .expect("create first");

        let params = ShSessionParams {
            action: SessionAction::Create,
            name: Some("second".to_string()),
            shell: Some("/bin/bash".to_string()),
            last: None,
            format: None,
        };

        let result = handle(params, &mgr, &pt, None).await;
        assert!(result.is_err(), "create beyond limit should fail");

        let err = result.unwrap_err();
        assert_eq!(err.code, -32006, "error code should be -32006 (limit reached)");
        assert!(
            err.message.contains("limit"),
            "error message should mention 'limit', got: {}",
            err.message
        );

        mgr.close_all().await;
    }

    // ------------------------------------------------------------------
    // Test 6: Create duplicate name (error)
    // ------------------------------------------------------------------

    #[tokio::test]
    #[serial(pty)]
    async fn test_create_duplicate_name() {
        let mgr = shared_mgr_with_session("dupe").await;
        let pt = test_process_table();

        let params = ShSessionParams {
            action: SessionAction::Create,
            name: Some("dupe".to_string()),
            shell: Some("/bin/bash".to_string()),
            last: None,
            format: None,
        };

        let result = handle(params, &mgr, &pt, None).await;
        assert!(result.is_err(), "duplicate name should fail");

        let err = result.unwrap_err();
        // AlreadyExists maps to -32006 (ERR_LIMIT_REACHED) per SessionError::error_code()
        assert_eq!(err.code, -32006, "error code should be -32006");
        assert!(
            err.message.contains("already exists"),
            "error message should mention 'already exists', got: {}",
            err.message
        );

        mgr.close_all().await;
    }

    // ------------------------------------------------------------------
    // Test 7: Unknown action (error)
    // ------------------------------------------------------------------

    #[test]
    fn test_unknown_action_rejected_by_serde() {
        // With the SessionAction enum, unknown action strings are rejected
        // at deserialization time by serde, not by the handler.
        let json_str = r#"{"action": "destroy"}"#;
        let result: Result<ShSessionParams, _> = serde_json::from_str(json_str);
        assert!(result.is_err(), "unknown action should fail deserialization");
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("destroy") || err_msg.contains("unknown variant"),
            "error should mention the bad action, got: {err_msg}");
    }

    // ------------------------------------------------------------------
    // Test 8: Create without name (error)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn test_create_missing_name() {
        let mgr = SessionManager::new(test_config());
        let pt = test_process_table();

        let params = ShSessionParams {
            action: SessionAction::Create,
            name: None,
            shell: Some("/bin/bash".to_string()),
            last: None,
            format: None,
        };

        let result = handle(params, &mgr, &pt, None).await;
        assert!(result.is_err(), "create without name should fail");

        let err = result.unwrap_err();
        assert_eq!(err.code, ERR_INVALID_PARAMS);
        assert!(
            err.message.contains("name is required"),
            "error message should mention 'name is required', got: {}",
            err.message
        );
    }

    // ------------------------------------------------------------------
    // Test 9: Close without name (error)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn test_close_missing_name() {
        let mgr = SessionManager::new(test_config());
        let pt = test_process_table();

        let params = ShSessionParams {
            action: SessionAction::Close,
            name: None,
            shell: None,
            last: None,
            format: None,
        };

        let result = handle(params, &mgr, &pt, None).await;
        assert!(result.is_err(), "close without name should fail");

        let err = result.unwrap_err();
        assert_eq!(err.code, ERR_INVALID_PARAMS);
        assert!(
            err.message.contains("name is required"),
            "error message should mention 'name is required', got: {}",
            err.message
        );
    }

    // ------------------------------------------------------------------
    // Test 10: List with multiple sessions
    // ------------------------------------------------------------------

    #[tokio::test]
    #[serial(pty)]
    async fn test_list_multiple_sessions() {
        let mgr = SessionManager::new(test_config());
        let pt = test_process_table();

        mgr.create_session("alpha", Some(bash_path()))
            .await
            .expect("create alpha");
        mgr.create_session("beta", Some(bash_path()))
            .await
            .expect("create beta");

        let params = ShSessionParams {
            action: SessionAction::List,
            name: None,
            shell: None,
            last: None,
            format: None,
        };

        let result = handle(params, &mgr, &pt, None).await;
        assert!(result.is_ok(), "list should succeed: {result:?}");

        let val = result.unwrap();
        let sessions = val["sessions"].as_array().expect("sessions array");
        assert_eq!(sessions.len(), 2);

        // Sessions are sorted by name (SessionManager::list_sessions sorts).
        assert_eq!(sessions[0]["session"], "alpha");
        assert_eq!(sessions[1]["session"], "beta");

        mgr.close_all().await;
    }

    // ------------------------------------------------------------------
    // Test 11: List empty (no sessions)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn test_list_empty() {
        let mgr = SessionManager::new(test_config());
        let pt = test_process_table();

        let params = ShSessionParams {
            action: SessionAction::List,
            name: None,
            shell: None,
            last: None,
            format: None,
        };

        let result = handle(params, &mgr, &pt, None).await;
        assert!(result.is_ok(), "list should succeed: {result:?}");

        let val = result.unwrap();
        let sessions = val["sessions"].as_array().expect("sessions array");
        assert!(sessions.is_empty());
    }

    // ------------------------------------------------------------------
    // Test 12: Params deserialization
    // ------------------------------------------------------------------

    #[test]
    fn test_params_deserialize_create() {
        let json = r#"{"action": "create", "name": "dev", "shell": "/bin/zsh"}"#;
        let params: ShSessionParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.action, SessionAction::Create);
        assert_eq!(params.name, Some("dev".to_string()));
        assert_eq!(params.shell, Some("/bin/zsh".to_string()));
    }

    #[test]
    fn test_params_deserialize_list() {
        let json = r#"{"action": "list"}"#;
        let params: ShSessionParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.action, SessionAction::List);
        assert!(params.name.is_none());
        assert!(params.shell.is_none());
    }

    #[test]
    fn test_params_deserialize_close() {
        let json = r#"{"action": "close", "name": "dev"}"#;
        let params: ShSessionParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.action, SessionAction::Close);
        assert_eq!(params.name, Some("dev".to_string()));
        assert!(params.shell.is_none());
    }

    // ------------------------------------------------------------------
    // Test 13: ToolError display
    // ------------------------------------------------------------------

    #[test]
    fn test_tool_error_display() {
        let err = ToolError {
            code: -32002,
            message: "session not found: ghost".to_string(),
        };
        let display = format!("{err}");
        assert!(display.contains("-32002"));
        assert!(display.contains("session not found"));
    }

    // ------------------------------------------------------------------
    // Test 14: SessionError conversion to ToolError
    // ------------------------------------------------------------------

    #[test]
    fn test_session_error_to_tool_error() {
        let se = SessionError::NotFound("test".to_string());
        let te: ToolError = se.into();
        assert_eq!(te.code, -32002);
        assert!(te.message.contains("not found"));

        let se = SessionError::LimitReached {
            current: 5,
            max: 5,
        };
        let te: ToolError = se.into();
        assert_eq!(te.code, -32006);
        assert!(te.message.contains("limit"));

        let se = SessionError::AlreadyExists("x".to_string());
        let te: ToolError = se.into();
        assert_eq!(te.code, -32006);
        assert!(te.message.contains("already exists"));
    }

    // ------------------------------------------------------------------
    // Test 15: Create then list then close (full lifecycle)
    // ------------------------------------------------------------------

    #[tokio::test]
    #[serial(pty)]
    async fn test_full_lifecycle() {
        let mgr = SessionManager::new(test_config());
        let pt = test_process_table();

        // Create
        let create_params = ShSessionParams {
            action: SessionAction::Create,
            name: Some("lifecycle".to_string()),
            shell: Some("/bin/bash".to_string()),
            last: None,
            format: None,
        };
        let create_result = handle(create_params, &mgr, &pt, None).await;
        assert!(create_result.is_ok(), "create should succeed");
        assert_eq!(create_result.unwrap()["session"], "lifecycle");

        // List - should contain the session
        let list_params = ShSessionParams {
            action: SessionAction::List,
            name: None,
            shell: None,
            last: None,
            format: None,
        };
        let list_result = handle(list_params, &mgr, &pt, None).await;
        assert!(list_result.is_ok(), "list should succeed");
        let list_val = list_result.unwrap();
        let sessions = list_val["sessions"]
            .as_array()
            .expect("sessions array");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["session"], "lifecycle");

        // Close
        let close_params = ShSessionParams {
            action: SessionAction::Close,
            name: Some("lifecycle".to_string()),
            shell: None,
            last: None,
            format: None,
        };
        let close_result = handle(close_params, &mgr, &pt, None).await;
        assert!(close_result.is_ok(), "close should succeed");
        assert_eq!(close_result.unwrap()["closed"], true);

        // List again - should be empty
        let list_params2 = ShSessionParams {
            action: SessionAction::List,
            name: None,
            shell: None,
            last: None,
            format: None,
        };
        let list_result2 = handle(list_params2, &mgr, &pt, None).await;
        assert!(list_result2.is_ok());
        let list_val2 = list_result2.unwrap();
        let sessions2 = list_val2["sessions"]
            .as_array()
            .expect("sessions array");
        assert!(sessions2.is_empty());
    }

    // ------------------------------------------------------------------
    // Audit access tests
    // ------------------------------------------------------------------

    // Test 16: Audit action returns all JSONL entries
    #[tokio::test]
    async fn test_audit_returns_entries() {
        use crate::audit::logger::{AuditEntry, AuditEvent, AuditLogger};
        use crate::config::AuditConfig;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let cfg = AuditConfig {
            log_path: dir.path().join("audit.log").to_string_lossy().to_string(),
            log_level: "trace".into(),
            log_commands: true,
            log_policy_decisions: true,
            log_handoff_events: true,
            raw_retention: "none".into(),
        };
        let session_id = "test-audit-sess";
        let mut logger = AuditLogger::new(&cfg, session_id).unwrap();

        // Write some CommandRecord entries
        logger.log(AuditEntry::new(
            session_id.into(), "sh_run".into(), Some("ls".into()),
            AuditEvent::CommandRecord {
                category: "condense".into(),
                grammar: Some("ls".into()),
                exit_code: 0,
                wall_ms: 50,
                raw_bytes: 1000,
                squashed_bytes: 200,
                compression_ratio: 5.0,
                safety_action: "allow".into(),
                raw_output_sha256: None,
            },
        ));
        logger.log(AuditEntry::new(
            session_id.into(), "sh_run".into(), Some("make".into()),
            AuditEvent::CommandRecord {
                category: "condense".into(),
                grammar: Some("make".into()),
                exit_code: 0,
                wall_ms: 3000,
                raw_bytes: 5000,
                squashed_bytes: 500,
                compression_ratio: 10.0,
                safety_action: "allow".into(),
                raw_output_sha256: None,
            },
        ));
        logger.flush();

        let mgr = SessionManager::new(test_config());
        let pt = test_process_table();
        let audit_ctx = AuditContext {
            config: &cfg,
            session_id,
        };

        let params = ShSessionParams {
            action: SessionAction::Audit,
            name: None,
            shell: None,
            last: None,
            format: None,
        };

        let result = handle(params, &mgr, &pt, Some(&audit_ctx)).await.unwrap();
        let entries = result["entries"].as_array().expect("entries array");
        assert_eq!(entries.len(), 2, "expected 2 CommandRecord entries, got: {entries:?}");
    }

    // Test 17: Audit action with last=1 returns only the last entry
    #[tokio::test]
    async fn test_audit_last_n() {
        use crate::audit::logger::{AuditEntry, AuditEvent, AuditLogger};
        use crate::config::AuditConfig;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let cfg = AuditConfig {
            log_path: dir.path().join("audit.log").to_string_lossy().to_string(),
            log_level: "trace".into(),
            log_commands: true,
            log_policy_decisions: true,
            log_handoff_events: true,
            raw_retention: "none".into(),
        };
        let session_id = "test-audit-last";
        let mut logger = AuditLogger::new(&cfg, session_id).unwrap();

        for i in 0..5 {
            logger.log(AuditEntry::new(
                session_id.into(), "sh_run".into(), Some(format!("cmd{i}")),
                AuditEvent::CommandRecord {
                    category: "condense".into(),
                    grammar: None,
                    exit_code: 0,
                    wall_ms: 100 * (i + 1),
                    raw_bytes: 1000,
                    squashed_bytes: 200,
                    compression_ratio: 5.0,
                    safety_action: "allow".into(),
                    raw_output_sha256: None,
                },
            ));
        }
        logger.flush();

        let mgr = SessionManager::new(test_config());
        let pt = test_process_table();
        let audit_ctx = AuditContext { config: &cfg, session_id };

        let params = ShSessionParams {
            action: SessionAction::Audit,
            name: None, shell: None,
            last: Some(2),
            format: None,
        };

        let result = handle(params, &mgr, &pt, Some(&audit_ctx)).await.unwrap();
        let entries = result["entries"].as_array().expect("entries array");
        assert_eq!(entries.len(), 2, "expected last 2 entries");
        // Should be the last two (cmd3, cmd4)
        assert_eq!(entries[0]["command"], "cmd3");
        assert_eq!(entries[1]["command"], "cmd4");
    }

    // Test 18: Audit format=summary returns aggregate
    #[tokio::test]
    async fn test_audit_format_summary() {
        use crate::audit::logger::{AuditEntry, AuditEvent, AuditLogger};
        use crate::config::AuditConfig;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let cfg = AuditConfig {
            log_path: dir.path().join("audit.log").to_string_lossy().to_string(),
            log_level: "trace".into(),
            log_commands: true,
            log_policy_decisions: true,
            log_handoff_events: true,
            raw_retention: "none".into(),
        };
        let session_id = "test-audit-summary";
        let mut logger = AuditLogger::new(&cfg, session_id).unwrap();

        logger.log(AuditEntry::new(
            session_id.into(), "sh_run".into(), Some("ls".into()),
            AuditEvent::CommandRecord {
                category: "condense".into(),
                grammar: Some("ls".into()),
                exit_code: 0,
                wall_ms: 50,
                raw_bytes: 1000,
                squashed_bytes: 200,
                compression_ratio: 5.0,
                safety_action: "allow".into(),
                raw_output_sha256: None,
            },
        ));
        logger.log(AuditEntry::new(
            session_id.into(), "sh_run".into(), Some("make".into()),
            AuditEvent::CommandRecord {
                category: "condense".into(),
                grammar: Some("make".into()),
                exit_code: 0,
                wall_ms: 3000,
                raw_bytes: 5000,
                squashed_bytes: 500,
                compression_ratio: 10.0,
                safety_action: "allow".into(),
                raw_output_sha256: None,
            },
        ));
        logger.flush();

        let mgr = SessionManager::new(test_config());
        let pt = test_process_table();
        let audit_ctx = AuditContext { config: &cfg, session_id };

        let params = ShSessionParams {
            action: SessionAction::Audit,
            name: None, shell: None,
            last: None,
            format: Some("summary".into()),
        };

        let result = handle(params, &mgr, &pt, Some(&audit_ctx)).await.unwrap();
        assert_eq!(result["total_commands"], 2);
        assert_eq!(result["total_raw_bytes"], 6000);
        assert_eq!(result["total_squashed_bytes"], 700);
        let grammars = result["grammars_used"].as_array().unwrap();
        assert!(grammars.contains(&json!("ls")));
        assert!(grammars.contains(&json!("make")));
    }

    // Test 19: Audit without audit context returns error
    #[tokio::test]
    async fn test_audit_no_context_error() {
        let mgr = SessionManager::new(test_config());
        let pt = test_process_table();

        let params = ShSessionParams {
            action: SessionAction::Audit,
            name: None, shell: None,
            last: None,
            format: None,
        };

        let result = handle(params, &mgr, &pt, None).await;
        assert!(result.is_err(), "audit without context should fail");
    }

    // Test 20: Audit params deserialization
    #[test]
    fn test_params_deserialize_audit() {
        let json = r#"{"action": "audit", "last": 3}"#;
        let params: ShSessionParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.action, SessionAction::Audit);
        assert_eq!(params.last, Some(3));
        assert!(params.format.is_none());
    }

    #[test]
    fn test_params_deserialize_audit_summary() {
        let json = r#"{"action": "audit", "format": "summary"}"#;
        let params: ShSessionParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.action, SessionAction::Audit);
        assert!(params.last.is_none());
        assert_eq!(params.format, Some("summary".to_string()));
    }
}
