//! sh_spawn tool — background process management for MCP server mode.
//!
//! Starts a command in a session's shell as a background process, registers it
//! in the process table with a unique alias, and optionally waits for a regex
//! match in the output before returning. The process continues running in the
//! background after the tool returns.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use std::sync::Arc;

use regex::Regex;
use crate::config::MishConfig;
use crate::core::pty::PtyCapture;
use crate::interpreter::{self, DedicatedPtyProcess, ManagedInterpreter, ManagedProcess, InterpreterSession};
use crate::mcp::types::{
    ShSpawnParams, ShSpawnResponse,
    ERR_INVALID_PARAMS, ERR_COMMAND_BLOCKED, ERR_SHELL_ERROR,
};
use crate::safety;
use crate::process::spool::OutputSpool;
use crate::process::table::{ProcessTable, ProcessTableError};
use crate::session::manager::SessionManager;
use crate::squasher::vte_strip;

use crate::policy::scope::extract_scope;

use super::ToolError;

/// Polling interval when checking spool for wait_for matches.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Number of tail lines to include when wait_for times out.
const OUTPUT_TAIL_LINES: usize = 20;

/// Default session name when none is provided.
const DEFAULT_SESSION: &str = "main";

/// Global counter for auto-generated aliases.
static ALIAS_COUNTER: AtomicU32 = AtomicU32::new(1);

// ---------------------------------------------------------------------------
// Timeout resolution
// ---------------------------------------------------------------------------

/// Resolve timeout using the precedence: explicit > per-scope > config default.
fn resolve_spawn_timeout(explicit: Option<u64>, cmd: &str, config: &MishConfig) -> Duration {
    if let Some(secs) = explicit {
        return Duration::from_secs(secs);
    }

    let scope = extract_scope(cmd);
    if let Some(scope_timeout) = config.timeout_defaults.scope.get(scope) {
        return Duration::from_secs(*scope_timeout);
    }

    Duration::from_secs(config.timeout_defaults.default)
}

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
// SpawnSetup — returned by setup(), consumed by wait_for_match()
// ---------------------------------------------------------------------------

/// Intermediate state after spawning but before waiting.
/// Holds the spool Arc so the caller can release the ProcessTable lock.
pub struct SpawnSetup {
    pub alias: String,
    pub pid: u32,
    pub session: String,
    pub spool: Arc<OutputSpool>,
    pub wait_for: Option<String>,
    pub timeout: Duration,
    /// Managed process for draining output (interpreters and dedicated PTYs).
    /// None for regular background processes (those use session manager).
    pub managed: Option<Arc<ManagedProcess>>,
}

// ---------------------------------------------------------------------------
// Setup (requires ProcessTable lock)
// ---------------------------------------------------------------------------

/// Phase 1: Validate params, spawn background job, register in process table.
/// Returns SpawnSetup so the caller can release the table lock before waiting.
pub async fn setup(
    params: ShSpawnParams,
    session_manager: &SessionManager,
    process_table: &mut ProcessTable,
    config: &MishConfig,
) -> Result<SpawnSetup, ToolError> {
    let alias = params.alias.clone();
    let timeout = resolve_spawn_timeout(params.timeout, &params.cmd, config);

    // Validate alias is not empty.
    if alias.is_empty() {
        return Err(ToolError::new(ERR_INVALID_PARAMS, "alias must not be empty"));
    }

    // Safety deny-list check.
    if let Some(reason) = safety::check_deny_list(&params.cmd) {
        return Err(ToolError::new(
            ERR_COMMAND_BLOCKED,
            format!("command blocked by safety deny-list: {reason}"),
        ));
    }

    // Pre-check alias uniqueness before doing any work.
    // Allow reuse of aliases in terminal states (Killed/Completed/Failed/TimedOut)
    // so the kill-and-restart recovery pattern works.
    if process_table.alias_exists(&alias) {
        if let Some(entry) = process_table.get(&alias) {
            if !entry.state.is_terminal() {
                return Err(ToolError::from_process_table_error(
                    &ProcessTableError::AliasInUse,
                ));
            }
        }
    }

    // Dedicated PTY: spawn in its own PTY as foreground process.
    // For TUI/interactive apps (claude, htop, vim) that need their own terminal.
    if params.dedicated_pty.unwrap_or(false) {
        return setup_dedicated_pty(params, process_table, timeout).await;
    }

    // REPL detection: if the command is a bare interpreter invocation,
    // spawn it in a dedicated PTY via InterpreterSession instead of
    // backgrounding it in the shell session.
    if interpreter::is_repl_command(&params.cmd) {
        return setup_repl(params, process_table, timeout).await;
    }

    let session_name = DEFAULT_SESSION;

    // Ensure default session exists (auto-create if needed).
    session_manager
        .ensure_default_session()
        .await
        .map_err(ToolError::from_session_error)?;

    // Send command as background job to the session shell.
    // Single line: cmd backgrounds, then echo captures $! (PID of last bg job).
    // Single quotes around the marker avoid bash history expansion on `!`.
    // Must be one line so only one PROMPT_COMMAND boundary fires — if split
    // across lines, execute() returns after the first boundary (from `cmd &`)
    // before the echo runs, and PID extraction fails.
    let bg_cmd = format!("{} & echo 'MISH_BG_PID:'$!", params.cmd);
    let result = session_manager
        .execute_in_session(session_name, &bg_cmd, timeout)
        .await
        .map_err(|e| ToolError::new(e.error_code(), e.to_string()))?;

    // Extract PID from output. If extraction fails, the command is running
    // but untrackable — surface the error rather than registering with PID 0
    // (which would make killpg target our own process group).
    let pid = extract_bg_pid(&result.output).ok_or_else(|| {
        ToolError::new(
            ERR_SHELL_ERROR,
            format!(
                "background process started but PID extraction failed. Output: {}",
                result.output.chars().take(200).collect::<String>()
            ),
        )
    })?;

    // Register in process table.
    process_table
        .register(&alias, session_name, pid, None)
        .map_err(|e| ToolError::from_process_table_error(&e))?;

    // Clone spool Arc before we return (caller will drop table lock).
    let spool = process_table
        .get(&alias)
        .map(|e| e.spool.clone())
        .expect("just registered");

    // Write initial output to the process spool.
    let clean_output = clean_bg_output(&result.output);
    if !clean_output.is_empty() {
        spool.write(clean_output.as_bytes());
    }

    Ok(SpawnSetup {
        alias,
        pid,
        session: session_name.to_string(),
        spool,
        wait_for: params.wait_for,
        timeout,
        managed: None,
    })
}

// ---------------------------------------------------------------------------
// Dedicated PTY setup (raw PTY for TUI/interactive apps)
// ---------------------------------------------------------------------------

/// Spawn a process in a dedicated PTY with raw I/O (no sentinels).
/// For TUI apps, other agents, and interactive tools.
async fn setup_dedicated_pty(
    params: ShSpawnParams,
    process_table: &mut ProcessTable,
    timeout: Duration,
) -> Result<SpawnSetup, ToolError> {
    let alias = params.alias.clone();
    let cmd = params.cmd.clone();

    // Spawn in a blocking task (PTY allocation is blocking).
    let mut pty = tokio::task::spawn_blocking(move || {
        // If the command contains shell metacharacters, wrap in /bin/sh -c
        // Always wrap in /bin/sh -c to handle shell syntax, PATH lookup, etc.
        // This mirrors how InterpreterSession::spawn works.
        let args = vec!["/bin/sh".to_string(), "-c".to_string(), cmd];

        PtyCapture::spawn(&args)
    })
    .await
    .map_err(|e| ToolError::new(ERR_SHELL_ERROR, format!("spawn_blocking join error: {e}")))?
    .map_err(|e| ToolError::new(ERR_SHELL_ERROR, format!("dedicated PTY spawn failed: {e}")))?;

    // Dedicated PTY children survive mish exit (BUG-004).
    pty.set_detach_on_drop(true);

    let pid = pty.pid().as_raw() as u32;

    // Register in process table with session="dedicated".
    process_table
        .register(&alias, "dedicated", pid, None)
        .map_err(|e| ToolError::from_process_table_error(&e))?;

    // Clone spool Arc from the registered entry.
    let spool = process_table
        .get(&alias)
        .map(|e| e.spool.clone())
        .expect("just registered");

    // Create DedicatedPtyProcess, wrap in ManagedProcess, attach to entry.
    let managed = Arc::new(ManagedProcess::Dedicated(
        DedicatedPtyProcess::new(pty, spool.clone()),
    ));
    process_table.set_interpreter(&alias, managed.clone());

    // Store app profile for send_and_wait.
    if let Some(ref profile) = params.profile {
        if let Some(entry) = process_table.entries_mut().get_mut(&alias) {
            entry.profile = Some(profile.clone());
        }
    }

    Ok(SpawnSetup {
        alias,
        pid,
        session: "dedicated".to_string(),
        spool,
        wait_for: params.wait_for,
        timeout,
        managed: Some(managed),
    })
}

// ---------------------------------------------------------------------------
// REPL setup (dedicated PTY via InterpreterSession)
// ---------------------------------------------------------------------------

/// Spawn a REPL interpreter in a dedicated PTY and register it in the process table.
async fn setup_repl(
    params: ShSpawnParams,
    process_table: &mut ProcessTable,
    timeout: Duration,
) -> Result<SpawnSetup, ToolError> {
    let alias = params.alias.clone();
    let cmd = params.cmd.clone();

    // Spawn the interpreter in a blocking task (PTY allocation is blocking).
    let interpreter_session = tokio::task::spawn_blocking(move || {
        InterpreterSession::spawn(&cmd)
    })
    .await
    .map_err(|e| ToolError::new(ERR_SHELL_ERROR, format!("spawn_blocking join error: {e}")))?
    .map_err(|e| ToolError::new(ERR_SHELL_ERROR, format!("interpreter spawn failed: {e}")))?;

    let pid = interpreter_session.pid();

    // Register in process table with session="interpreter".
    process_table
        .register(&alias, "interpreter", pid, None)
        .map_err(|e| ToolError::from_process_table_error(&e))?;

    // Clone spool Arc from the registered entry.
    let spool = process_table
        .get(&alias)
        .map(|e| e.spool.clone())
        .expect("just registered");

    // Create ManagedInterpreter, wrap in ManagedProcess, and attach to the process entry.
    let managed = Arc::new(ManagedProcess::Interpreter(
        ManagedInterpreter::new(interpreter_session, spool.clone()),
    ));
    process_table.set_interpreter(&alias, managed.clone());

    // Store app profile for send_and_wait.
    if let Some(ref profile) = params.profile {
        if let Some(entry) = process_table.entries_mut().get_mut(&alias) {
            entry.profile = Some(profile.clone());
        }
    }

    Ok(SpawnSetup {
        alias,
        pid,
        session: "interpreter".to_string(),
        spool,
        wait_for: params.wait_for,
        timeout,
        managed: Some(managed),
    })
}

// ---------------------------------------------------------------------------
// Wait (does NOT require ProcessTable lock)
// ---------------------------------------------------------------------------

/// Phase 2: Poll the spool for a wait_for regex match.
/// Operates on Arc<OutputSpool> — no ProcessTable lock needed.
pub async fn wait_for_match(
    setup: &SpawnSetup,
    session_manager: &SessionManager,
) -> ShSpawnResponse {
    let Some(ref wait_pattern) = setup.wait_for else {
        // No wait_for — return immediately.
        return ShSpawnResponse {
            alias: setup.alias.clone(),
            pid: setup.pid,
            session: setup.session.clone(),
            state: "running".to_string(),
            wait_matched: false,
            match_line: None,
            duration_to_match_ms: None,
            output_tail: None,
            reason: None,
        };
    };

    let regex = match Regex::new(&format!("(?i){}", wait_pattern)) {
        Ok(r) => r,
        Err(e) => {
            return ShSpawnResponse {
                alias: setup.alias.clone(),
                pid: setup.pid,
                session: setup.session.clone(),
                state: "running".to_string(),
                wait_matched: false,
                match_line: None,
                duration_to_match_ms: None,
                output_tail: None,
                reason: Some(format!("invalid wait_for regex '{}': {}", wait_pattern, e)),
            };
        }
    };

    let start = Instant::now();

    // Check existing spool output first.
    {
        let existing = String::from_utf8_lossy(&setup.spool.read_all()).to_string();
        if let Some(matched) = find_match_line(&existing, &regex) {
            return ShSpawnResponse {
                alias: setup.alias.clone(),
                pid: setup.pid,
                session: setup.session.clone(),
                state: "running".to_string(),
                wait_matched: true,
                match_line: Some(matched),
                duration_to_match_ms: Some(start.elapsed().as_millis() as u64),
                output_tail: None,
                reason: None,
            };
        }
    }

    // Poll for new output.
    loop {
        if start.elapsed() >= setup.timeout {
            let raw = setup.spool.read_all();
            let text = String::from_utf8_lossy(&raw);
            let stripped = vte_strip::strip_ansi(&text);
            let lines: Vec<&str> = stripped.lines().collect();
            let tail_start = lines.len().saturating_sub(OUTPUT_TAIL_LINES);
            let output_tail = lines[tail_start..].join("\n");

            return ShSpawnResponse {
                alias: setup.alias.clone(),
                pid: setup.pid,
                session: setup.session.clone(),
                state: "running".to_string(),
                wait_matched: false,
                match_line: None,
                duration_to_match_ms: None,
                output_tail: Some(output_tail),
                reason: Some(format!(
                    "wait_for regex did not match within {}s timeout",
                    setup.timeout.as_secs()
                )),
            };
        }

        tokio::time::sleep(WAIT_POLL_INTERVAL).await;

        // Read new output into spool.
        // For managed processes (interpreters, dedicated PTYs), drain from the PTY.
        // For regular background processes, read from the session manager.
        if let Some(ref managed) = setup.managed {
            let _ = managed.drain_to_spool().await;
        } else {
            let mut buf = vec![0u8; 4096];
            match session_manager
                .read_from_session(&setup.session, &mut buf)
                .await
            {
                Ok(n) if n > 0 => {
                    setup.spool.write(&buf[..n]);
                }
                _ => {}
            }
        }

        // Check spool for match.
        let all_output = String::from_utf8_lossy(&setup.spool.read_all()).to_string();
        if let Some(matched) = find_match_line(&all_output, &regex) {
            return ShSpawnResponse {
                alias: setup.alias.clone(),
                pid: setup.pid,
                session: setup.session.clone(),
                state: "running".to_string(),
                wait_matched: true,
                match_line: Some(matched),
                duration_to_match_ms: Some(start.elapsed().as_millis() as u64),
                output_tail: None,
                reason: None,
            };
        }
    }
}

// ---------------------------------------------------------------------------
// Original handle() — delegates to setup() + wait_for_match()
// ---------------------------------------------------------------------------

/// Handle an sh_spawn tool call.
///
/// NOTE: This function holds the ProcessTable reference for the entire call.
/// For concurrent access, use setup() + wait_for_match() separately and
/// release the table lock between them.
pub async fn handle(
    params: ShSpawnParams,
    session_manager: &SessionManager,
    process_table: &mut ProcessTable,
    config: &MishConfig,
) -> Result<serde_json::Value, ToolError> {
    // Validate wait_for regex early (before setup) to return ToolError.
    if let Some(ref wait_pattern) = params.wait_for {
        Regex::new(&format!("(?i){}", wait_pattern)).map_err(|e| {
            ToolError::new(
                ERR_INVALID_PARAMS,
                format!("invalid wait_for regex '{}': {}", wait_pattern, e),
            )
        })?;
    }

    let spawn_setup = setup(params, session_manager, process_table, config).await?;
    let response = wait_for_match(&spawn_setup, session_manager).await;
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

/// Find the first line matching the regex and return it (ANSI-stripped).
pub(crate) fn find_match_line(output: &str, regex: &Regex) -> Option<String> {
    for line in output.lines() {
        let clean = vte_strip::strip_ansi(line);
        if regex.is_match(&clean) {
            return Some(clean);
        }
    }
    None
}

/// Get the last N lines from a process's spool output (ANSI-stripped).
#[cfg(test)]
fn get_output_tail(table: &ProcessTable, alias: &str, max_lines: usize) -> String {
    if let Some(entry) = table.get(alias) {
        let raw = entry.spool.read_all();
        let text = String::from_utf8_lossy(&raw);
        let stripped = vte_strip::strip_ansi(&text);
        let lines: Vec<&str> = stripped.lines().collect();
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

    /// Shared session helper for integration tests.
    /// Reduces boilerplate — each test still gets its own session (serial gate handles isolation).
    async fn shared_session() -> (SessionManager, MishConfig) {
        let config = test_config();
        let session_config = test_session_config();
        let mgr = SessionManager::new(session_config);
        mgr.create_session("main", Some("/bin/bash"))
            .await
            .expect("shared session creation");
        (mgr, config)
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
    fn find_match_line_strips_ansi_from_output() {
        let regex = Regex::new("(?i)listening on").unwrap();
        // Simulate ANSI-colored output: \x1b[32m = green, \x1b[0m = reset
        let output = "Starting...\n\x1b[32mListening on port 3000\x1b[0m\nReady";
        let matched = find_match_line(output, &regex);
        assert_eq!(
            matched,
            Some("Listening on port 3000".to_string()),
            "match_line should not contain ANSI escape sequences"
        );
        // Verify no escape characters leaked through.
        assert!(
            !matched.as_ref().unwrap().contains('\x1b'),
            "match_line must not contain raw ANSI escapes"
        );
    }

    #[test]
    fn find_match_line_matches_text_inside_ansi() {
        let regex = Regex::new("(?i)error").unwrap();
        // The word "error" is wrapped in ANSI red: \x1b[31m...\x1b[0m
        let output = "\x1b[31merror: something failed\x1b[0m";
        let matched = find_match_line(output, &regex);
        assert!(matched.is_some(), "should match 'error' inside ANSI codes");
        assert!(!matched.as_ref().unwrap().contains('\x1b'));
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

    #[test]
    fn get_output_tail_strips_ansi() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);
        table.register("test", "main", 100, None).unwrap();

        let entry = table.get("test").unwrap();
        // Write output containing ANSI color codes.
        entry.spool.write(b"\x1b[32mline1\x1b[0m\n\x1b[31mline2\x1b[0m\n\x1b[33mline3\x1b[0m");

        let tail = get_output_tail(&table, "test", 3);
        assert_eq!(tail, "line1\nline2\nline3", "ANSI codes should be stripped");
        assert!(!tail.contains('\x1b'), "output must not contain raw ANSI escapes");
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
        let (mgr, config) = shared_session().await;
        let mut table = ProcessTable::new(&config);

        let params = ShSpawnParams {
            alias: "bg1".to_string(),
            cmd: "echo hello_from_bg".to_string(),
            wait_for: None,
            timeout: Some(5),
            dedicated_pty: None,
            profile: None,
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
        let (mgr, config) = shared_session().await;
        let mut table = ProcessTable::new(&config);

        // Spawn first process.
        let params1 = ShSpawnParams {
            alias: "dup".to_string(),
            cmd: "echo first".to_string(),
            wait_for: None,
            timeout: Some(5),
            dedicated_pty: None,
            profile: None,
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
            dedicated_pty: None,
            profile: None,
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
        let (mgr, config) = shared_session().await;
        let mut table = ProcessTable::new(&config);

        // The command outputs "ready to serve" which should match "ready".
        // Since echo returns immediately, the match should be in the initial output.
        let params = ShSpawnParams {
            alias: "server".to_string(),
            cmd: "echo 'server is ready to serve'".to_string(),
            wait_for: Some("ready".to_string()),
            timeout: Some(5),
            dedicated_pty: None,
            profile: None,
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
    #[serial(pty)]
    async fn spawn_empty_alias_rejected() {
        let (mgr, config) = shared_session().await;
        let mut table = ProcessTable::new(&config);

        let params = ShSpawnParams {
            alias: "".to_string(),
            cmd: "echo hello".to_string(),
            wait_for: None,
            timeout: Some(5),
            dedicated_pty: None,
            profile: None,
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
        let (mgr, config) = shared_session().await;
        let mut table = ProcessTable::new(&config);

        let params = ShSpawnParams {
            alias: "bad_regex".to_string(),
            cmd: "echo hello".to_string(),
            wait_for: Some("[invalid".to_string()),
            timeout: Some(5),
            dedicated_pty: None,
            profile: None,
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
        let (mgr, config) = shared_session().await;
        let mut table = ProcessTable::new(&config);

        // Command outputs "hello" but we wait for "never_matches".
        // Use a very short timeout (1 second) to keep the test fast.
        let params = ShSpawnParams {
            alias: "waiter".to_string(),
            cmd: "echo hello_world".to_string(),
            wait_for: Some("never_matches_this".to_string()),
            timeout: Some(1),
            dedicated_pty: None,
            profile: None,
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
        let (mgr, config) = shared_session().await;
        let mut table = ProcessTable::new(&config);

        let params = ShSpawnParams {
            alias: "danger".to_string(),
            cmd: "rm -rf /".to_string(),
            wait_for: None,
            timeout: Some(5),
            dedicated_pty: None,
            profile: None,
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
        let (mgr, config) = shared_session().await;
        let mut table = ProcessTable::new(&config);

        let params = ShSpawnParams {
            alias: "format".to_string(),
            cmd: "mkfs.ext4 /dev/sda".to_string(),
            wait_for: None,
            timeout: Some(5),
            dedicated_pty: None,
            profile: None,
        };

        let result = handle(params, &mgr, &mut table, &config).await;
        assert!(result.is_err(), "mkfs should be blocked");
        let err = result.unwrap_err();
        assert_eq!(err.code, -32005);

        mgr.close_all().await;
    }

    // ── Per-scope timeout resolution ─────────────────────────────────

    #[test]
    fn resolve_spawn_timeout_explicit_overrides_scope() {
        let config = test_config();
        // npm has scope timeout of 300, but explicit 60 wins
        let timeout = super::resolve_spawn_timeout(Some(60), "npm install", &config);
        assert_eq!(timeout, Duration::from_secs(60));
    }

    #[test]
    fn resolve_spawn_timeout_per_scope_npm() {
        let config = test_config();
        let timeout = super::resolve_spawn_timeout(None, "npm install", &config);
        assert_eq!(timeout, Duration::from_secs(300));
    }

    #[test]
    fn resolve_spawn_timeout_per_scope_cargo() {
        let config = test_config();
        let timeout = super::resolve_spawn_timeout(None, "cargo build", &config);
        assert_eq!(timeout, Duration::from_secs(600));
    }

    #[test]
    fn resolve_spawn_timeout_unknown_uses_default() {
        let config = test_config();
        let timeout = super::resolve_spawn_timeout(None, "echo hello", &config);
        assert_eq!(timeout, Duration::from_secs(config.timeout_defaults.default));
    }

    #[test]
    fn resolve_spawn_timeout_path_command_extracts_basename() {
        let config = test_config();
        let timeout = super::resolve_spawn_timeout(None, "/usr/bin/npm start", &config);
        assert_eq!(timeout, Duration::from_secs(300));
    }
}
