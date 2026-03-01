//! `sh_session` MCP tool — session lifecycle management.
//!
//! Provides create, list, and close actions for managing shell sessions.
//! Wraps `SessionManager` for all session operations.

use serde::Deserialize;
use serde_json::json;

use crate::mcp::types::{
    self as mcp_types, ShSessionListResponse,
};
use crate::process::table::ProcessTable;
use crate::session::manager::{SessionError, SessionManager};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Tool-level error returned from `handle`.
#[derive(Debug)]
pub struct ToolError {
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for ToolError {}

impl From<SessionError> for ToolError {
    fn from(e: SessionError) -> Self {
        ToolError {
            code: e.error_code(),
            message: e.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Extended params (superset of mcp::types::ShSessionParams)
// ---------------------------------------------------------------------------

/// Parameters for the sh_session tool.
///
/// Extends the base `ShSessionParams` with `name` and `shell` fields needed
/// for create and close actions.
#[derive(Debug, Clone, Deserialize)]
pub struct ShSessionParams {
    pub action: String,
    pub name: Option<String>,
    pub shell: Option<String>,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const ERR_INVALID_PARAMS: i32 = -32602;
const ERR_UNKNOWN_ACTION: i32 = -32602;

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

/// Handle an `sh_session` tool call.
///
/// Dispatches to create, list, or close based on the `action` parameter.
/// Returns a JSON value containing the result plus a process digest.
pub async fn handle(
    params: ShSessionParams,
    session_manager: &SessionManager,
    process_table: &ProcessTable,
) -> Result<serde_json::Value, ToolError> {
    let result = match params.action.as_str() {
        "create" => handle_create(params, session_manager).await?,
        "list" => handle_list(session_manager, process_table).await?,
        "close" => handle_close(params, session_manager).await?,
        other => {
            return Err(ToolError {
                code: ERR_UNKNOWN_ACTION,
                message: format!("unknown action: {other}; expected create, list, or close"),
            });
        }
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
    let name = params.name.as_deref().ok_or_else(|| ToolError {
        code: ERR_INVALID_PARAMS,
        message: "name is required for create action".to_string(),
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

/// Handle `action: "close"` — close a named session and kill its processes.
async fn handle_close(
    params: ShSessionParams,
    session_manager: &SessionManager,
) -> Result<serde_json::Value, ToolError> {
    let name = params.name.as_deref().ok_or_else(|| ToolError {
        code: ERR_INVALID_PARAMS,
        message: "name is required for close action".to_string(),
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

    // ------------------------------------------------------------------
    // Test 1: Create session
    // ------------------------------------------------------------------

    #[tokio::test]
    #[serial(pty)]
    async fn test_create_session() {
        let mgr = SessionManager::new(test_config());
        let pt = test_process_table();

        let params = ShSessionParams {
            action: "create".to_string(),
            name: Some("test-session".to_string()),
            shell: Some("/bin/bash".to_string()),
        };

        let result = handle(params, &mgr, &pt).await;
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
        let mgr = SessionManager::new(test_config());
        let pt = test_process_table();

        // Create a session first.
        mgr.create_session("main", Some(bash_path()))
            .await
            .expect("create main");

        let params = ShSessionParams {
            action: "list".to_string(),
            name: None,
            shell: None,
        };

        let result = handle(params, &mgr, &pt).await;
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
        let mgr = SessionManager::new(test_config());
        let pt = test_process_table();

        mgr.create_session("ephemeral", Some(bash_path()))
            .await
            .expect("create");

        let params = ShSessionParams {
            action: "close".to_string(),
            name: Some("ephemeral".to_string()),
            shell: None,
        };

        let result = handle(params, &mgr, &pt).await;
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
            action: "close".to_string(),
            name: Some("ghost".to_string()),
            shell: None,
        };

        let result = handle(params, &mgr, &pt).await;
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
            action: "create".to_string(),
            name: Some("second".to_string()),
            shell: Some("/bin/bash".to_string()),
        };

        let result = handle(params, &mgr, &pt).await;
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
        let mgr = SessionManager::new(test_config());
        let pt = test_process_table();

        mgr.create_session("dupe", Some(bash_path()))
            .await
            .expect("create dupe");

        let params = ShSessionParams {
            action: "create".to_string(),
            name: Some("dupe".to_string()),
            shell: Some("/bin/bash".to_string()),
        };

        let result = handle(params, &mgr, &pt).await;
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

    #[tokio::test]
    async fn test_unknown_action() {
        let mgr = SessionManager::new(test_config());
        let pt = test_process_table();

        let params = ShSessionParams {
            action: "destroy".to_string(),
            name: None,
            shell: None,
        };

        let result = handle(params, &mgr, &pt).await;
        assert!(result.is_err(), "unknown action should fail");

        let err = result.unwrap_err();
        assert_eq!(err.code, ERR_UNKNOWN_ACTION);
        assert!(
            err.message.contains("unknown action"),
            "error message should mention 'unknown action', got: {}",
            err.message
        );
    }

    // ------------------------------------------------------------------
    // Test 8: Create without name (error)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn test_create_missing_name() {
        let mgr = SessionManager::new(test_config());
        let pt = test_process_table();

        let params = ShSessionParams {
            action: "create".to_string(),
            name: None,
            shell: Some("/bin/bash".to_string()),
        };

        let result = handle(params, &mgr, &pt).await;
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
            action: "close".to_string(),
            name: None,
            shell: None,
        };

        let result = handle(params, &mgr, &pt).await;
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
            action: "list".to_string(),
            name: None,
            shell: None,
        };

        let result = handle(params, &mgr, &pt).await;
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
            action: "list".to_string(),
            name: None,
            shell: None,
        };

        let result = handle(params, &mgr, &pt).await;
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
        assert_eq!(params.action, "create");
        assert_eq!(params.name, Some("dev".to_string()));
        assert_eq!(params.shell, Some("/bin/zsh".to_string()));
    }

    #[test]
    fn test_params_deserialize_list() {
        let json = r#"{"action": "list"}"#;
        let params: ShSessionParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.action, "list");
        assert!(params.name.is_none());
        assert!(params.shell.is_none());
    }

    #[test]
    fn test_params_deserialize_close() {
        let json = r#"{"action": "close", "name": "dev"}"#;
        let params: ShSessionParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.action, "close");
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
            action: "create".to_string(),
            name: Some("lifecycle".to_string()),
            shell: Some("/bin/bash".to_string()),
        };
        let create_result = handle(create_params, &mgr, &pt).await;
        assert!(create_result.is_ok(), "create should succeed");
        assert_eq!(create_result.unwrap()["session"], "lifecycle");

        // List - should contain the session
        let list_params = ShSessionParams {
            action: "list".to_string(),
            name: None,
            shell: None,
        };
        let list_result = handle(list_params, &mgr, &pt).await;
        assert!(list_result.is_ok(), "list should succeed");
        let list_val = list_result.unwrap();
        let sessions = list_val["sessions"]
            .as_array()
            .expect("sessions array");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["session"], "lifecycle");

        // Close
        let close_params = ShSessionParams {
            action: "close".to_string(),
            name: Some("lifecycle".to_string()),
            shell: None,
        };
        let close_result = handle(close_params, &mgr, &pt).await;
        assert!(close_result.is_ok(), "close should succeed");
        assert_eq!(close_result.unwrap()["closed"], true);

        // List again - should be empty
        let list_params2 = ShSessionParams {
            action: "list".to_string(),
            name: None,
            shell: None,
        };
        let list_result2 = handle(list_params2, &mgr, &pt).await;
        assert!(list_result2.is_ok());
        let list_val2 = list_result2.unwrap();
        let sessions2 = list_val2["sessions"]
            .as_array()
            .expect("sessions array");
        assert!(sessions2.is_empty());
    }
}
