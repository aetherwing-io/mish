//! sh_spawn tool — background process management for MCP server mode.
//!
//! Starts a command in a session's shell as a background process, registers it
//! in the process table with a unique alias, and optionally waits for a regex
//! match in the output before returning. The process continues running in the
//! background after the tool returns.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use regex::Regex;
use crate::config::MishConfig;
use crate::mcp::types::{ShSpawnParams, ShSpawnResponse};
use crate::safety;
use crate::process::table::{ProcessTable, ProcessTableError};
use crate::session::manager::SessionManager;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default timeout for wait_for matching (seconds).
const DEFAULT_TIMEOUT_SEC: u64 = 300;

/// Polling interval when checking spool for wait_for matches.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Number of tail lines to include when wait_for times out.
const OUTPUT_TAIL_LINES: usize = 20;

/// Default session name when none is provided.
const DEFAULT_SESSION: &str = "main";

/// Global counter for auto-generated aliases.
static ALIAS_COUNTER: AtomicU32 = AtomicU32::new(1);

// ---------------------------------------------------------------------------
// ToolError
// ---------------------------------------------------------------------------

/// Error type for tool operations.
#[derive(Debug)]
pub struct ToolError {
    pub code: i32,
    pub message: String,
}

impl ToolError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// Create from a ProcessTableError.
    pub fn from_process_table_error(e: &ProcessTableError) -> Self {
        Self {
            code: e.error_code(),
            message: e.to_string(),
        }
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "tool error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for ToolError {}

// ---------------------------------------------------------------------------
// Alias generation
// ---------------------------------------------------------------------------

/// Generate a unique alias like "proc-1", "proc-2", etc.
pub fn generate_alias() -> String {
    let n = ALIAS_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("proc-{n}")
}

/// Reset alias counter (for testing only).
#[cfg(test)]
pub fn reset_alias_counter() {
    ALIAS_COUNTER.store(1, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

/// Handle an sh_spawn tool call.
///
/// Workflow:
/// 1. Resolve session name (default: "main")
/// 2. Validate/generate alias
/// 3. Check alias uniqueness in ProcessTable (error -32004 on conflict)
/// 4. Send command to session shell as a background job (`cmd &`)
/// 5. Register process in ProcessTable
/// 6. If wait_for provided: poll output spool until regex matches or timeout
/// 7. Return spawn response with process digest
pub async fn handle(
    params: ShSpawnParams,
    session_manager: &SessionManager,
    process_table: &mut ProcessTable,
    _config: &MishConfig,
) -> Result<serde_json::Value, ToolError> {
    let session_name = DEFAULT_SESSION;
    let alias = params.alias.clone();
    let timeout_sec = params.timeout.unwrap_or(DEFAULT_TIMEOUT_SEC);
    let timeout = Duration::from_secs(timeout_sec);

    // Validate alias is not empty.
    if alias.is_empty() {
        return Err(ToolError::new(-32602, "alias must not be empty"));
    }

    // Safety deny-list check.
    if let Some(reason) = safety::check_deny_list(&params.cmd) {
        return Err(ToolError::new(
            -32005,
            format!("command blocked by safety deny-list: {reason}"),
        ));
    }

    // Pre-check alias uniqueness before doing any work.
    if process_table.alias_exists(&alias) {
        return Err(ToolError::from_process_table_error(
            &ProcessTableError::AliasInUse,
        ));
    }

    // Verify session exists.
    let _session = session_manager
        .get_session(session_name)
        .await
        .ok_or_else(|| ToolError::new(-32002, format!("session not found: {session_name}")))?;

    // Send command as background job to the session shell.
    // We append ` &` and `echo` the background PID so we can capture it.
    // The command format: `<cmd> & echo "MISH_BG_PID:$!"`
    let bg_cmd = format!("{} &\necho \"MISH_BG_PID:$!\"", params.cmd);
    let result = session_manager
        .execute_in_session(session_name, &bg_cmd, timeout)
        .await
        .map_err(|e| ToolError::new(e.error_code(), e.to_string()))?;

    // Extract PID from output. Look for "MISH_BG_PID:<pid>".
    let pid = extract_bg_pid(&result.output).unwrap_or(0);

    // Register in process table.
    process_table
        .register(&alias, session_name, pid, None)
        .map_err(|e| ToolError::from_process_table_error(&e))?;

    // Write initial output to the process spool.
    if let Some(entry) = process_table.get(&alias) {
        // Strip the MISH_BG_PID line from output before storing.
        let clean_output = clean_bg_output(&result.output);
        if !clean_output.is_empty() {
            entry.spool.write(clean_output.as_bytes());
        }
    }

    // If wait_for is specified, poll the spool for a regex match.
    if let Some(ref wait_pattern) = params.wait_for {
        let regex = Regex::new(&format!("(?i){}", wait_pattern)).map_err(|e| {
            ToolError::new(
                -32602,
                format!("invalid wait_for regex '{}': {}", wait_pattern, e),
            )
        })?;

        let start = Instant::now();

        // First check existing output (already in spool).
        if let Some(entry) = process_table.get(&alias) {
            let existing = String::from_utf8_lossy(&entry.spool.read_all()).to_string();
            if let Some(matched) = find_match_line(&existing, &regex) {
                let duration_ms = start.elapsed().as_millis() as u64;
                let response = ShSpawnResponse {
                    alias: alias.clone(),
                    pid,
                    session: session_name.to_string(),
                    state: "running".to_string(),
                    wait_matched: true,
                    match_line: Some(matched),
                    duration_to_match_ms: Some(duration_ms),
                    output_tail: None,
                    reason: None,
                };
                return Ok(serde_json::to_value(&response).unwrap());
            }
        }

        // Poll for new output by reading from the session.
        loop {
            if start.elapsed() >= timeout {
                // Timeout — return with wait_matched: false.
                let output_tail = get_output_tail(process_table, &alias, OUTPUT_TAIL_LINES);
                let response = ShSpawnResponse {
                    alias: alias.clone(),
                    pid,
                    session: session_name.to_string(),
                    state: "running".to_string(),
                    wait_matched: false,
                    match_line: None,
                    duration_to_match_ms: None,
                    output_tail: Some(output_tail),
                    reason: Some(format!(
                        "wait_for regex did not match within {}s timeout",
                        timeout_sec
                    )),
                };
                return Ok(serde_json::to_value(&response).unwrap());
            }

            // Small sleep before polling.
            tokio::time::sleep(WAIT_POLL_INTERVAL).await;

            // Read any new output from the session.
            let mut buf = vec![0u8; 4096];
            match session_manager
                .read_from_session(session_name, &mut buf)
                .await
            {
                Ok(n) if n > 0 => {
                    if let Some(entry) = process_table.get(&alias) {
                        entry.spool.write(&buf[..n]);
                    }
                }
                _ => {}
            }

            // Check spool for match.
            if let Some(entry) = process_table.get(&alias) {
                let all_output = String::from_utf8_lossy(&entry.spool.read_all()).to_string();
                if let Some(matched) = find_match_line(&all_output, &regex) {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    let response = ShSpawnResponse {
                        alias: alias.clone(),
                        pid,
                        session: session_name.to_string(),
                        state: "running".to_string(),
                        wait_matched: true,
                        match_line: Some(matched),
                        duration_to_match_ms: Some(duration_ms),
                        output_tail: None,
                        reason: None,
                    };
                    return Ok(serde_json::to_value(&response).unwrap());
                }
            }
        }
    }

    // No wait_for — return immediately.
    let response = ShSpawnResponse {
        alias: alias.clone(),
        pid,
        session: session_name.to_string(),
        state: "running".to_string(),
        wait_matched: false,
        match_line: None,
        duration_to_match_ms: None,
        output_tail: None,
        reason: None,
    };

    Ok(serde_json::to_value(&response).unwrap())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the background PID from output containing "MISH_BG_PID:<pid>".
fn extract_bg_pid(output: &str) -> Option<u32> {
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(pid_str) = trimmed.strip_prefix("MISH_BG_PID:") {
            if let Ok(pid) = pid_str.trim().parse::<u32>() {
                return Some(pid);
            }
        }
    }
    None
}

/// Remove the MISH_BG_PID marker line from output.
fn clean_bg_output(output: &str) -> String {
    output
        .lines()
        .filter(|line| !line.trim().starts_with("MISH_BG_PID:"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Find the first line matching the regex and return it.
fn find_match_line(output: &str, regex: &Regex) -> Option<String> {
    for line in output.lines() {
        if regex.is_match(line) {
            return Some(line.to_string());
        }
    }
    None
}

/// Get the last N lines from a process's spool output.
fn get_output_tail(table: &ProcessTable, alias: &str, max_lines: usize) -> String {
    if let Some(entry) = table.get(alias) {
        let raw = entry.spool.read_all();
        let text = String::from_utf8_lossy(&raw);
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(max_lines);
        lines[start..].join("\n")
    } else {
        String::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MishConfig;
    use crate::process::table::ProcessTable;
    use serial_test::serial;
    use std::sync::Arc;

    fn test_config() -> MishConfig {
        let mut config = MishConfig::default();
        config.server.max_processes = 20;
        config.server.max_spool_bytes_total = 52_428_800;
        config.squasher.spool_bytes = 1024;
        config
    }

    fn small_config(max_processes: usize) -> MishConfig {
        let mut config = MishConfig::default();
        config.server.max_processes = max_processes;
        config.server.max_spool_bytes_total = 52_428_800;
        config.squasher.spool_bytes = 1024;
        config
    }

    fn test_session_config() -> Arc<MishConfig> {
        Arc::new(MishConfig::default())
    }

    // ── Unit tests for helpers ────────────────────────────────────────

    #[test]
    fn extract_bg_pid_from_output() {
        let output = "some output\nMISH_BG_PID:12345\nmore output";
        assert_eq!(extract_bg_pid(output), Some(12345));
    }

    #[test]
    fn extract_bg_pid_missing() {
        let output = "just regular output\nno pid here";
        assert_eq!(extract_bg_pid(output), None);
    }

    #[test]
    fn extract_bg_pid_with_whitespace() {
        let output = "MISH_BG_PID: 42 ";
        assert_eq!(extract_bg_pid(output), Some(42));
    }

    #[test]
    fn clean_bg_output_removes_marker() {
        let output = "line1\nMISH_BG_PID:123\nline3";
        assert_eq!(clean_bg_output(output), "line1\nline3");
    }

    #[test]
    fn clean_bg_output_no_marker() {
        let output = "line1\nline2\nline3";
        assert_eq!(clean_bg_output(output), "line1\nline2\nline3");
    }

    #[test]
    fn find_match_line_found() {
        let regex = Regex::new("(?i)listening on").unwrap();
        let output = "Starting server...\nListening on port 3000\nReady";
        assert_eq!(
            find_match_line(output, &regex),
            Some("Listening on port 3000".to_string())
        );
    }

    #[test]
    fn find_match_line_not_found() {
        let regex = Regex::new("(?i)error").unwrap();
        let output = "all good\nno problems here";
        assert_eq!(find_match_line(output, &regex), None);
    }

    #[test]
    fn get_output_tail_returns_last_n_lines() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);
        table.register("test", "main", 100, None).unwrap();

        let entry = table.get("test").unwrap();
        entry.spool.write(b"line1\nline2\nline3\nline4\nline5");

        let tail = get_output_tail(&table, "test", 3);
        assert_eq!(tail, "line3\nline4\nline5");
    }

    #[test]
    fn get_output_tail_fewer_lines_than_max() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);
        table.register("test", "main", 100, None).unwrap();

        let entry = table.get("test").unwrap();
        entry.spool.write(b"line1\nline2");

        let tail = get_output_tail(&table, "test", 10);
        assert_eq!(tail, "line1\nline2");
    }

    #[test]
    fn get_output_tail_unknown_alias() {
        let config = test_config();
        let table = ProcessTable::new(&config);
        let tail = get_output_tail(&table, "nonexistent", 10);
        assert_eq!(tail, "");
    }

    // ── Alias auto-generation ─────────────────────────────────────────

    #[test]
    fn alias_auto_generation_sequential() {
        reset_alias_counter();
        let a1 = generate_alias();
        let a2 = generate_alias();
        let a3 = generate_alias();
        assert_eq!(a1, "proc-1");
        assert_eq!(a2, "proc-2");
        assert_eq!(a3, "proc-3");
    }

    // ── Process table integration ─────────────────────────────────────

    #[test]
    fn alias_conflict_returns_correct_error_code() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);
        table.register("server", "main", 100, None).unwrap();

        // Trying to register the same alias should fail.
        let result = table.register("server", "main", 200, None);
        assert_eq!(result, Err(ProcessTableError::AliasInUse));
        assert_eq!(ProcessTableError::AliasInUse.error_code(), -32004);
    }

    #[test]
    fn process_limit_returns_correct_error_code() {
        let config = small_config(1);
        let mut table = ProcessTable::new(&config);
        table.register("proc1", "main", 100, None).unwrap();

        let result = table.register("proc2", "main", 200, None);
        assert_eq!(result, Err(ProcessTableError::ProcessLimitReached));
        assert_eq!(ProcessTableError::ProcessLimitReached.error_code(), -32007);
    }

    #[test]
    fn process_registered_in_table_after_register() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);
        table.register("myalias", "main", 42, None).unwrap();

        assert!(table.alias_exists("myalias"));
        let entry = table.get("myalias").unwrap();
        assert_eq!(entry.alias, "myalias");
        assert_eq!(entry.session, "main");
        assert_eq!(entry.pid, 42);
        assert_eq!(
            entry.state,
            crate::process::state::ProcessState::Running
        );
    }

    // ── ToolError ─────────────────────────────────────────────────────

    #[test]
    fn tool_error_from_process_table_error() {
        let err = ToolError::from_process_table_error(&ProcessTableError::AliasInUse);
        assert_eq!(err.code, -32004);
        assert!(err.message.contains("alias"));
    }

    #[test]
    fn tool_error_display() {
        let err = ToolError::new(-32004, "process alias already in use");
        let msg = format!("{err}");
        assert!(msg.contains("-32004"));
        assert!(msg.contains("already in use"));
    }

    // ── Integration tests (require real shell) ────────────────────────

    #[tokio::test]
    #[serial(pty)]
    async fn spawn_basic_command() {
        let config = test_config();
        let session_config = test_session_config();
        let mgr = SessionManager::new(session_config);
        mgr.create_session("main", Some("/bin/bash"))
            .await
            .expect("create session");

        let mut table = ProcessTable::new(&config);

        let params = ShSpawnParams {
            alias: "bg1".to_string(),
            cmd: "echo hello_from_bg".to_string(),
            wait_for: None,
            timeout: Some(5),
        };

        let result = handle(params, &mgr, &mut table, &config).await;
        assert!(result.is_ok(), "handle should succeed: {:?}", result.err());

        let val = result.unwrap();
        assert_eq!(val["alias"], "bg1");
        assert_eq!(val["session"], "main");
        assert_eq!(val["state"], "running");
        // wait_matched should be false since no wait_for
        assert_eq!(val["wait_matched"], false);

        // Process should be in the table.
        assert!(table.alias_exists("bg1"));

        mgr.close_all().await;
    }

    #[tokio::test]
    #[serial(pty)]
    async fn spawn_alias_conflict_error() {
        let config = test_config();
        let session_config = test_session_config();
        let mgr = SessionManager::new(session_config);
        mgr.create_session("main", Some("/bin/bash"))
            .await
            .expect("create session");

        let mut table = ProcessTable::new(&config);

        // Spawn first process.
        let params1 = ShSpawnParams {
            alias: "dup".to_string(),
            cmd: "echo first".to_string(),
            wait_for: None,
            timeout: Some(5),
        };
        handle(params1, &mgr, &mut table, &config)
            .await
            .expect("first spawn");

        // Spawn with same alias should fail.
        let params2 = ShSpawnParams {
            alias: "dup".to_string(),
            cmd: "echo second".to_string(),
            wait_for: None,
            timeout: Some(5),
        };
        let result = handle(params2, &mgr, &mut table, &config).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, -32004);

        mgr.close_all().await;
    }

    #[tokio::test]
    #[serial(pty)]
    async fn spawn_wait_for_matching() {
        let config = test_config();
        let session_config = test_session_config();
        let mgr = SessionManager::new(session_config);
        mgr.create_session("main", Some("/bin/bash"))
            .await
            .expect("create session");

        let mut table = ProcessTable::new(&config);

        // The command outputs "ready to serve" which should match "ready".
        // Since echo returns immediately, the match should be in the initial output.
        let params = ShSpawnParams {
            alias: "server".to_string(),
            cmd: "echo 'server is ready to serve'".to_string(),
            wait_for: Some("ready".to_string()),
            timeout: Some(5),
        };

        let result = handle(params, &mgr, &mut table, &config).await;
        assert!(result.is_ok(), "handle should succeed: {:?}", result.err());

        let val = result.unwrap();
        assert_eq!(val["alias"], "server");
        assert_eq!(val["wait_matched"], true);
        assert!(val["match_line"].is_string());
        let match_line = val["match_line"].as_str().unwrap();
        assert!(
            match_line.contains("ready"),
            "match_line should contain 'ready', got: {match_line}"
        );
        assert!(val["duration_to_match_ms"].is_number());

        mgr.close_all().await;
    }

    #[tokio::test]
    async fn spawn_session_not_found() {
        let config = test_config();
        let session_config = test_session_config();
        let mgr = SessionManager::new(session_config);
        // Don't create the "main" session.

        let mut table = ProcessTable::new(&config);

        let params = ShSpawnParams {
            alias: "bg".to_string(),
            cmd: "echo hello".to_string(),
            wait_for: None,
            timeout: Some(5),
        };

        let result = handle(params, &mgr, &mut table, &config).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, -32002);
    }

    #[tokio::test]
    #[serial(pty)]
    async fn spawn_empty_alias_rejected() {
        let config = test_config();
        let session_config = test_session_config();
        let mgr = SessionManager::new(session_config);
        mgr.create_session("main", Some("/bin/bash"))
            .await
            .expect("create session");

        let mut table = ProcessTable::new(&config);

        let params = ShSpawnParams {
            alias: "".to_string(),
            cmd: "echo hello".to_string(),
            wait_for: None,
            timeout: Some(5),
        };

        let result = handle(params, &mgr, &mut table, &config).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, -32602);

        mgr.close_all().await;
    }

    #[tokio::test]
    #[serial(pty)]
    async fn spawn_invalid_wait_for_regex() {
        let config = test_config();
        let session_config = test_session_config();
        let mgr = SessionManager::new(session_config);
        mgr.create_session("main", Some("/bin/bash"))
            .await
            .expect("create session");

        let mut table = ProcessTable::new(&config);

        let params = ShSpawnParams {
            alias: "bad_regex".to_string(),
            cmd: "echo hello".to_string(),
            wait_for: Some("[invalid".to_string()),
            timeout: Some(5),
        };

        let result = handle(params, &mgr, &mut table, &config).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, -32602);
        assert!(err.message.contains("invalid wait_for regex"));

        mgr.close_all().await;
    }

    #[tokio::test]
    #[serial(pty)]
    async fn spawn_wait_for_timeout() {
        let config = test_config();
        let session_config = test_session_config();
        let mgr = SessionManager::new(session_config);
        mgr.create_session("main", Some("/bin/bash"))
            .await
            .expect("create session");

        let mut table = ProcessTable::new(&config);

        // Command outputs "hello" but we wait for "never_matches".
        // Use a very short timeout (1 second) to keep the test fast.
        let params = ShSpawnParams {
            alias: "waiter".to_string(),
            cmd: "echo hello_world".to_string(),
            wait_for: Some("never_matches_this".to_string()),
            timeout: Some(1),
        };

        let result = handle(params, &mgr, &mut table, &config).await;
        assert!(result.is_ok(), "handle should succeed: {:?}", result.err());

        let val = result.unwrap();
        assert_eq!(val["alias"], "waiter");
        assert_eq!(val["wait_matched"], false);
        assert!(val["reason"].is_string());
        let reason = val["reason"].as_str().unwrap();
        assert!(
            reason.contains("did not match"),
            "reason should explain timeout, got: {reason}"
        );

        mgr.close_all().await;
    }

    // ── Deny-list integration ────────────────────────────────────────

    #[tokio::test]
    #[serial(pty)]
    async fn spawn_deny_list_blocks_rm_rf_root() {
        let config = test_config();
        let session_config = test_session_config();
        let mgr = SessionManager::new(session_config);
        mgr.create_session("main", Some("/bin/bash"))
            .await
            .expect("create session");

        let mut table = ProcessTable::new(&config);

        let params = ShSpawnParams {
            alias: "danger".to_string(),
            cmd: "rm -rf /".to_string(),
            wait_for: None,
            timeout: Some(5),
        };

        let result = handle(params, &mgr, &mut table, &config).await;
        assert!(result.is_err(), "rm -rf / should be blocked");
        let err = result.unwrap_err();
        assert_eq!(err.code, -32005);
        assert!(err.message.contains("deny-list"));

        // Process should NOT be in the table.
        assert!(!table.alias_exists("danger"));

        mgr.close_all().await;
    }

    #[tokio::test]
    #[serial(pty)]
    async fn spawn_deny_list_blocks_mkfs() {
        let config = test_config();
        let session_config = test_session_config();
        let mgr = SessionManager::new(session_config);
        mgr.create_session("main", Some("/bin/bash"))
            .await
            .expect("create session");

        let mut table = ProcessTable::new(&config);

        let params = ShSpawnParams {
            alias: "format".to_string(),
            cmd: "mkfs.ext4 /dev/sda".to_string(),
            wait_for: None,
            timeout: Some(5),
        };

        let result = handle(params, &mgr, &mut table, &config).await;
        assert!(result.is_err(), "mkfs should be blocked");
        let err = result.unwrap_err();
        assert_eq!(err.code, -32005);

        mgr.close_all().await;
    }
}
