//! PTY Environment Diagnostic Test Harness
//!
//! Systematically isolates which environmental condition causes PTY failures
//! when `mish serve` is spawned as a stdio subprocess (e.g., by Claude Code).
//!
//! Each test applies specific environmental restrictions via `pre_exec` and
//! verifies that `sh_run(echo diag_ok)` succeeds.
//!
//! Run all diagnostics:
//!     cargo test --test pty_env_diagnostic -- --nocapture
//!
//! Run matrix summary only:
//!     cargo test --test pty_env_diagnostic test_pty_diag_07_matrix_summary -- --nocapture
//!
//! Run a specific restriction:
//!     cargo test --test pty_env_diagnostic test_pty_diag_01_setsid -- --nocapture

use serial_test::serial;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// =========================================================================
// Environmental restriction variants
// =========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvRestriction {
    /// `libc::setsid()` — detaches from controlling terminal
    Setsid,
    /// Close all file descriptors > 2
    CloseExtraFds,
    /// Remove TERM environment variable
    NoTermEnv,
    /// `libc::setpgid(0, 0)` — new process group
    NewProcessGroup,
    /// `libc::signal(SIGHUP, SIG_IGN)` — ignore hangup
    IgnoreSighup,
}

impl fmt::Display for EnvRestriction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EnvRestriction::Setsid => write!(f, "setsid()"),
            EnvRestriction::CloseExtraFds => write!(f, "close fds > 2"),
            EnvRestriction::NoTermEnv => write!(f, "no TERM env"),
            EnvRestriction::NewProcessGroup => write!(f, "new process group"),
            EnvRestriction::IgnoreSighup => write!(f, "ignore SIGHUP"),
        }
    }
}

// =========================================================================
// Diagnostic error type
// =========================================================================

struct DiagError {
    message: String,
    elapsed: Duration,
    stderr_output: String,
    exit_status: Option<std::process::ExitStatus>,
}

impl fmt::Display for DiagError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "--- PTY DIAGNOSTIC FAILURE ---")?;
        writeln!(f, "Elapsed: {:.3}s", self.elapsed.as_secs_f64())?;
        writeln!(f, "Error: {}", self.message)?;
        if !self.stderr_output.is_empty() {
            writeln!(f, "Stderr:")?;
            for line in self.stderr_output.lines().take(20) {
                writeln!(f, "  {line}")?;
            }
        }
        if let Some(status) = &self.exit_status {
            writeln!(f, "Exit status: {status}")?;
        } else {
            writeln!(f, "Exit status: None (still running)")?;
        }
        write!(f, "---")
    }
}

type DiagResult<T> = Result<T, DiagError>;

// =========================================================================
// Diagnostic server — adapted from MishServer with pre_exec builder
// =========================================================================

const DIAG_TIMEOUT_MS: u16 = 15_000;

struct DiagnosticServer {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    reader: BufReader<std::process::ChildStdout>,
    stderr_buf: Arc<Mutex<String>>,
    started_at: Instant,
}

impl DiagnosticServer {
    /// Spawn `mish serve` with the given environmental restrictions applied.
    fn start(restrictions: &[EnvRestriction]) -> Self {
        let bin = env!("CARGO_BIN_EXE_mish");
        let mut cmd = Command::new(bin);
        cmd.arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("SHELL", "/bin/bash");

        // Apply env-level restrictions (on the Command, before spawn)
        for r in restrictions {
            if *r == EnvRestriction::NoTermEnv {
                cmd.env_remove("TERM");
            }
        }

        // Collect restrictions that need pre_exec (runs between fork and exec)
        let pre_exec_flags: Vec<EnvRestriction> = restrictions
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    EnvRestriction::Setsid
                        | EnvRestriction::CloseExtraFds
                        | EnvRestriction::NewProcessGroup
                        | EnvRestriction::IgnoreSighup
                )
            })
            .copied()
            .collect();

        if !pre_exec_flags.is_empty() {
            // SAFETY: All calls within pre_exec are async-signal-safe libc functions.
            // This runs in the child process after fork(), before exec().
            unsafe {
                cmd.pre_exec(move || {
                    for r in &pre_exec_flags {
                        match r {
                            EnvRestriction::Setsid => {
                                libc::setsid();
                            }
                            EnvRestriction::CloseExtraFds => {
                                // FDs 0/1/2 are already dup2'd to pipes by Command.
                                // Close everything else to simulate a clean environment.
                                for fd in 3..256 {
                                    libc::close(fd);
                                }
                            }
                            EnvRestriction::NewProcessGroup => {
                                libc::setpgid(0, 0);
                            }
                            EnvRestriction::IgnoreSighup => {
                                libc::signal(libc::SIGHUP, libc::SIG_IGN);
                            }
                            _ => {}
                        }
                    }
                    Ok(())
                });
            }
        }

        let started_at = Instant::now();
        let mut child = cmd.spawn().expect("failed to start mish serve");

        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");
        let reader = BufReader::new(stdout);

        // Spawn background thread to drain stderr continuously
        let stderr_buf = Arc::new(Mutex::new(String::new()));
        let buf_clone = stderr_buf.clone();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stderr);
            let mut line = String::new();
            while r.read_line(&mut line).unwrap_or(0) > 0 {
                buf_clone.lock().unwrap().push_str(&line);
                line.clear();
            }
        });

        Self {
            child,
            stdin: Some(stdin),
            reader,
            stderr_buf,
            started_at,
        }
    }

    fn stdin(&mut self) -> &mut std::process::ChildStdin {
        self.stdin.as_mut().expect("stdin already closed")
    }

    /// Send a JSON-RPC request and read the response with timeout.
    fn request(&mut self, json: &str) -> DiagResult<serde_json::Value> {
        // Write request — separate borrow from make_error
        let write_result = writeln!(self.stdin(), "{}", json);
        if let Err(e) = write_result {
            return Err(self.make_error(format!("write to stdin failed: {e}")));
        }
        let flush_result = self.stdin().flush();
        if let Err(e) = flush_result {
            return Err(self.make_error(format!("flush stdin failed: {e}")));
        }

        // Poll stdout with timeout
        let fd = self.reader.get_ref().as_raw_fd();
        let borrowed = unsafe { std::os::unix::io::BorrowedFd::borrow_raw(fd) };
        let mut pfd = [nix::poll::PollFd::new(borrowed, nix::poll::PollFlags::POLLIN)];

        match nix::poll::poll(&mut pfd, DIAG_TIMEOUT_MS) {
            Ok(0) => {
                return Err(self.make_error(format!(
                    "timeout ({}s) waiting for response to: {}",
                    DIAG_TIMEOUT_MS / 1000,
                    &json[..json.len().min(120)]
                )));
            }
            Ok(_) => {}
            Err(nix::Error::EINTR) => {}
            Err(e) => {
                return Err(self.make_error(format!("poll error: {e}")));
            }
        }

        let mut line = String::new();
        let read_result = self.reader.read_line(&mut line);
        if let Err(e) = read_result {
            return Err(self.make_error(format!("read error: {e}")));
        }

        if line.trim().is_empty() {
            return Err(self.make_error("server closed stdout (empty response)".to_string()));
        }

        serde_json::from_str(line.trim())
            .map_err(|e| self.make_error(format!("JSON parse error: {e}\nRaw: {}", line.trim())))
    }

    /// Send a JSON-RPC notification (no response expected).
    fn notify(&mut self, json: &str) -> DiagResult<()> {
        let write_result = writeln!(self.stdin(), "{}", json);
        if let Err(e) = write_result {
            return Err(self.make_error(format!("write notification failed: {e}")));
        }
        let flush_result = self.stdin().flush();
        if let Err(e) = flush_result {
            return Err(self.make_error(format!("flush notification failed: {e}")));
        }
        std::thread::sleep(Duration::from_millis(50));
        Ok(())
    }

    /// Run the MCP initialization handshake (initialize + notifications/initialized).
    fn init(&mut self) -> DiagResult<()> {
        let resp = self.request(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"pty-diagnostic"}}}"#,
        )?;

        // Verify server identity (camelCase: serverInfo)
        let server_name = resp["result"]["serverInfo"]["name"].as_str();
        if server_name != Some("mish") {
            return Err(self.make_error(format!(
                "unexpected init response (expected serverInfo.name=\"mish\"): {resp}"
            )));
        }

        self.notify(
            r#"{"jsonrpc":"2.0","id":null,"method":"notifications/initialized"}"#,
        )
    }

    /// Execute `echo diag_ok` via sh_run and return the output string.
    fn run_echo_diagnostic(&mut self) -> DiagResult<String> {
        let resp = self.request(
            r#"{"jsonrpc":"2.0","id":100,"method":"tools/call","params":{"name":"sh_run","arguments":{"cmd":"echo diag_ok","timeout":10}}}"#,
        )?;

        if resp["error"].is_object() {
            return Err(self.make_error(format!("sh_run returned error: {resp}")));
        }

        // Response is content-wrapped: result.content[0].text → JSON payload
        if let Some(text) = resp["result"]["content"][0]["text"].as_str() {
            if let Ok(payload) = serde_json::from_str::<serde_json::Value>(text) {
                if let Some(output) = payload["result"]["output"].as_str() {
                    return Ok(output.to_string());
                }
            }
        }

        // Fallback: try direct result access (older response format)
        if let Some(output) = resp["result"]["result"]["output"].as_str() {
            return Ok(output.to_string());
        }

        Err(self.make_error(format!(
            "could not extract output from sh_run response: {resp}"
        )))
    }

    /// Close stdin (triggers EOF → graceful shutdown) and wait for exit.
    fn shutdown(mut self) -> Option<std::process::ExitStatus> {
        self.stdin.take(); // Drop stdin → EOF
        let start = Instant::now();
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) if start.elapsed() > Duration::from_secs(10) => {
                    let _ = self.child.kill();
                    return self.child.wait().ok();
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => return None,
            }
        }
    }

    /// Build a DiagError with current diagnostic context.
    fn make_error(&mut self, message: String) -> DiagError {
        let stderr_output = self.stderr_buf.lock().unwrap().clone();
        let exit_status = self.child.try_wait().ok().flatten();
        DiagError {
            message,
            elapsed: self.started_at.elapsed(),
            stderr_output,
            exit_status,
        }
    }
}

impl Drop for DiagnosticServer {
    fn drop(&mut self) {
        // Best-effort kill if the test didn't call shutdown().
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// =========================================================================
// Diagnostic runner
// =========================================================================

/// Spawn mish with restrictions, init, run echo, return output.
fn run_diagnostic(restrictions: &[EnvRestriction]) -> DiagResult<String> {
    let mut server = DiagnosticServer::start(restrictions);
    server.init()?;
    let output = server.run_echo_diagnostic()?;
    server.shutdown();
    Ok(output)
}

// =========================================================================
// Shared helpers for alternative spawn paths
// =========================================================================

/// Send a JSON-RPC request over generic writer/reader and parse the response.
fn jsonrpc_request<W: Write, R: Read + AsRawFd>(
    writer: &mut W,
    reader: &mut BufReader<R>,
    json: &str,
) -> Result<serde_json::Value, String> {
    writeln!(writer, "{}", json).map_err(|e| format!("write: {e}"))?;
    writer.flush().map_err(|e| format!("flush: {e}"))?;

    let fd = reader.get_ref().as_raw_fd();
    let borrowed = unsafe { std::os::unix::io::BorrowedFd::borrow_raw(fd) };
    let mut pfd = [nix::poll::PollFd::new(borrowed, nix::poll::PollFlags::POLLIN)];

    match nix::poll::poll(&mut pfd, DIAG_TIMEOUT_MS) {
        Ok(0) => return Err(format!("timeout ({}s)", DIAG_TIMEOUT_MS / 1000)),
        Ok(_) => {}
        Err(nix::Error::EINTR) => {}
        Err(e) => return Err(format!("poll: {e}")),
    }

    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| format!("read: {e}"))?;
    if line.trim().is_empty() {
        return Err("server closed stdout (EOF)".to_string());
    }
    serde_json::from_str(line.trim()).map_err(|e| format!("parse: {e}\nraw: {}", line.trim()))
}

/// Extract the output string from an sh_run tools/call response.
/// Handles both content-wrapped and direct response formats.
fn extract_sh_run_output(resp: &serde_json::Value) -> Option<String> {
    // Content-wrapped: result.content[0].text → JSON with result.output
    if let Some(text) = resp["result"]["content"][0]["text"].as_str() {
        if let Ok(payload) = serde_json::from_str::<serde_json::Value>(text) {
            if let Some(output) = payload["result"]["output"].as_str() {
                return Some(output.to_string());
            }
        }
    }
    // Direct: result.result.output
    resp["result"]["result"]["output"].as_str().map(String::from)
}

// =========================================================================
// Alternative spawn paths: unix socketpair + bun
// =========================================================================

/// Create a unix socketpair (AF_UNIX, SOCK_STREAM) matching Bun's pipe impl.
fn make_socketpair() -> (OwnedFd, OwnedFd) {
    let mut fds = [0i32; 2];
    let ret = unsafe {
        libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr())
    };
    assert_eq!(ret, 0, "socketpair() failed: {}", std::io::Error::last_os_error());
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

/// Run diagnostic using unix socketpairs instead of POSIX pipes for stdio.
/// This matches Bun's spawn behavior — Bun uses AF_UNIX SOCK_STREAM socket
/// pairs, not pipe() — isolating whether the fd type is the differentiator.
fn run_diagnostic_socketpair() -> DiagResult<String> {
    let (parent_w, child_r) = make_socketpair();       // stdin:  parent writes → child reads
    let (child_w_out, parent_r_out) = make_socketpair(); // stdout: child writes → parent reads
    let (child_w_err, parent_r_err) = make_socketpair(); // stderr: child writes → parent reads

    let bin = env!("CARGO_BIN_EXE_mish");
    let started_at = Instant::now();

    let mut child = Command::new(bin)
        .arg("serve")
        .stdin(Stdio::from(child_r))
        .stdout(Stdio::from(child_w_out))
        .stderr(Stdio::from(child_w_err))
        .env("SHELL", "/bin/bash")
        .spawn()
        .expect("failed to start mish serve with socketpairs");

    // Convert parent-side OwnedFd → File for standard I/O traits
    let mut stdin_f: std::fs::File = parent_w.into();
    let stdout_f: std::fs::File = parent_r_out.into();
    let stderr_f: std::fs::File = parent_r_err.into();

    let mut reader = BufReader::new(stdout_f);

    // Drain stderr in background
    let stderr_buf = Arc::new(Mutex::new(String::new()));
    let buf_clone = stderr_buf.clone();
    std::thread::spawn(move || {
        let mut r = BufReader::new(stderr_f);
        let mut line = String::new();
        while r.read_line(&mut line).unwrap_or(0) > 0 {
            buf_clone.lock().unwrap().push_str(&line);
            line.clear();
        }
    });

    let wrap_err = |msg: String| DiagError {
        message: msg,
        elapsed: started_at.elapsed(),
        stderr_output: stderr_buf.lock().unwrap().clone(),
        exit_status: None,
    };

    // Initialize
    let resp = jsonrpc_request(
        &mut stdin_f,
        &mut reader,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"socketpair-diagnostic"}}}"#,
    ).map_err(|e| wrap_err(format!("init: {e}")))?;

    if resp["result"]["serverInfo"]["name"].as_str() != Some("mish") {
        return Err(wrap_err(format!("bad init response: {resp}")));
    }

    // Notification
    writeln!(stdin_f, r#"{{"jsonrpc":"2.0","id":null,"method":"notifications/initialized"}}"#)
        .map_err(|e| wrap_err(format!("notification write: {e}")))?;
    stdin_f.flush().map_err(|e| wrap_err(format!("notification flush: {e}")))?;
    std::thread::sleep(Duration::from_millis(50));

    // sh_run echo diag_ok
    let resp = jsonrpc_request(
        &mut stdin_f,
        &mut reader,
        r#"{"jsonrpc":"2.0","id":100,"method":"tools/call","params":{"name":"sh_run","arguments":{"cmd":"echo diag_ok","timeout":10}}}"#,
    ).map_err(|e| wrap_err(format!("sh_run: {e}")))?;

    if resp["error"].is_object() {
        return Err(wrap_err(format!("sh_run error: {resp}")));
    }

    let output = extract_sh_run_output(&resp)
        .ok_or_else(|| wrap_err(format!("could not extract output: {resp}")))?;

    // Cleanup
    drop(stdin_f);
    let _ = child.kill();
    let _ = child.wait();

    Ok(output)
}

/// Run diagnostic by spawning mish through Bun — the exact runtime Claude Code uses.
/// Bun creates AF_UNIX socket pairs for stdio AND runs in its own event loop,
/// capturing the full Claude Code spawn environment.
fn run_diagnostic_bun() -> DiagResult<String> {
    // Check bun availability
    let bun_check = Command::new("bun").arg("--version").output();
    if bun_check.as_ref().map(|o| o.status.success()).unwrap_or(false) == false {
        return Err(DiagError {
            message: "bun not found in PATH (install: https://bun.sh)".to_string(),
            elapsed: Duration::ZERO,
            stderr_output: String::new(),
            exit_status: None,
        });
    }

    let bin = env!("CARGO_BIN_EXE_mish");
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/bun_mcp_diagnostic.js");
    let started_at = Instant::now();

    let output = Command::new("bun")
        .arg("run")
        .arg(script)
        .env("MISH_BIN", bin)
        .env("SHELL", "/bin/bash")
        .output()
        .map_err(|e| DiagError {
            message: format!("failed to run bun: {e}"),
            elapsed: started_at.elapsed(),
            stderr_output: String::new(),
            exit_status: None,
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        return Err(DiagError {
            message: format!("bun exited with {}", output.status),
            elapsed: started_at.elapsed(),
            stderr_output: format!("bun stderr:\n{stderr}"),
            exit_status: Some(output.status),
        });
    }

    // Parse the JSON result emitted by the bun script
    let result: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|e| DiagError {
            message: format!("failed to parse bun output: {e}\nraw: {}", &stdout[..stdout.len().min(500)]),
            elapsed: started_at.elapsed(),
            stderr_output: format!("bun stderr:\n{stderr}"),
            exit_status: Some(output.status),
        })?;

    if result["pass"].as_bool() == Some(true) {
        Ok(result["output"].as_str().unwrap_or("").to_string())
    } else {
        let err_msg = result["error"].as_str().unwrap_or("unknown");
        let mish_stderr = result["stderr"].as_str().unwrap_or("");
        Err(DiagError {
            message: format!("bun spawn failed: {err_msg}"),
            elapsed: Duration::from_millis(result["elapsed_ms"].as_u64().unwrap_or(0)),
            stderr_output: format!("mish stderr:\n{mish_stderr}\nbun stderr:\n{stderr}"),
            exit_status: Some(output.status),
        })
    }
}

// =========================================================================
// Tests: 00–06 restrictions, 07 matrix, 08 socketpair, 09 bun
// =========================================================================

#[test]
#[serial(pty)]
fn test_pty_diag_00_baseline() {
    let result = run_diagnostic(&[]);
    match result {
        Ok(output) => {
            assert!(
                output.contains("diag_ok"),
                "Baseline output should contain 'diag_ok', got: {output}"
            );
            eprintln!("[PASS] 00 - Baseline: output contains diag_ok");
        }
        Err(e) => panic!("Baseline FAILED (control — should never happen):\n{e}"),
    }
}

#[test]
#[serial(pty)]
fn test_pty_diag_01_setsid() {
    let result = run_diagnostic(&[EnvRestriction::Setsid]);
    match result {
        Ok(output) => {
            assert!(
                output.contains("diag_ok"),
                "setsid output should contain 'diag_ok', got: {output}"
            );
            eprintln!("[PASS] 01 - setsid(): PTY works after setsid");
        }
        Err(e) => panic!("setsid() FAILED — primary suspect confirmed:\n{e}"),
    }
}

#[test]
#[serial(pty)]
fn test_pty_diag_02_close_extra_fds() {
    let result = run_diagnostic(&[EnvRestriction::CloseExtraFds]);
    match result {
        Ok(output) => {
            assert!(
                output.contains("diag_ok"),
                "close-fds output should contain 'diag_ok', got: {output}"
            );
            eprintln!("[PASS] 02 - Close extra fds: PTY works without inherited fds");
        }
        Err(e) => panic!("Close extra fds FAILED — may break tokio internals:\n{e}"),
    }
}

#[test]
#[serial(pty)]
fn test_pty_diag_03_no_term_env() {
    let result = run_diagnostic(&[EnvRestriction::NoTermEnv]);
    match result {
        Ok(output) => {
            assert!(
                output.contains("diag_ok"),
                "no-TERM output should contain 'diag_ok', got: {output}"
            );
            eprintln!("[PASS] 03 - No TERM env: PTY works without TERM");
        }
        Err(e) => panic!("No TERM env FAILED — shell init may depend on TERM:\n{e}"),
    }
}

#[test]
#[serial(pty)]
fn test_pty_diag_04_new_process_group() {
    let result = run_diagnostic(&[EnvRestriction::NewProcessGroup]);
    match result {
        Ok(output) => {
            assert!(
                output.contains("diag_ok"),
                "new-pgid output should contain 'diag_ok', got: {output}"
            );
            eprintln!("[PASS] 04 - New process group: PTY works with setpgid");
        }
        Err(e) => panic!("New process group FAILED — affects killpg targeting:\n{e}"),
    }
}

#[test]
#[serial(pty)]
fn test_pty_diag_05_ignore_sighup() {
    let result = run_diagnostic(&[EnvRestriction::IgnoreSighup]);
    match result {
        Ok(output) => {
            assert!(
                output.contains("diag_ok"),
                "ignore-SIGHUP output should contain 'diag_ok', got: {output}"
            );
            eprintln!("[PASS] 05 - Ignore SIGHUP: PTY works with SIGHUP ignored");
        }
        Err(e) => panic!("Ignore SIGHUP FAILED — child may not clean up:\n{e}"),
    }
}

#[test]
#[serial(pty)]
fn test_pty_diag_06_all_combined() {
    let restrictions = &[
        EnvRestriction::Setsid,
        EnvRestriction::CloseExtraFds,
        EnvRestriction::NoTermEnv,
        EnvRestriction::NewProcessGroup,
        EnvRestriction::IgnoreSighup,
    ];
    let result = run_diagnostic(restrictions);
    match result {
        Ok(output) => {
            assert!(
                output.contains("diag_ok"),
                "all-combined output should contain 'diag_ok', got: {output}"
            );
            eprintln!("[PASS] 06 - All combined: full Claude Code simulation passes");
        }
        Err(e) => panic!("All combined FAILED — full Claude Code environment simulation:\n{e}"),
    }
}

#[test]
#[serial(pty)]
fn test_pty_diag_08_unix_socketpair() {
    let result = run_diagnostic_socketpair();
    match result {
        Ok(output) => {
            assert!(
                output.contains("diag_ok"),
                "socketpair output should contain 'diag_ok', got: {output}"
            );
            eprintln!("[PASS] 08 - Unix socketpair: PTY works with AF_UNIX SOCK_STREAM stdio");
        }
        Err(e) => panic!(
            "Unix socketpair FAILED — Bun's socket-based pipes may be the cause:\n{e}"
        ),
    }
}

#[test]
#[serial(pty)]
fn test_pty_diag_09_bun_spawn() {
    let result = run_diagnostic_bun();
    match result {
        Ok(output) => {
            assert!(
                output.contains("diag_ok"),
                "bun output should contain 'diag_ok', got: {output}"
            );
            eprintln!("[PASS] 09 - Bun spawn: PTY works under Bun's process management");
        }
        Err(e) => {
            if e.message.contains("bun not found") {
                eprintln!("[SKIP] 09 - Bun spawn: bun not installed");
                return;
            }
            panic!("Bun spawn FAILED — reproduces Claude Code environment:\n{e}");
        }
    }
}

#[test]
#[serial(pty)]
fn test_pty_diag_07_matrix_summary() {
    // Restriction-based variants (use run_diagnostic)
    let restriction_variants: &[(&str, &[EnvRestriction])] = &[
        ("00 - Baseline (pipes)", &[]),
        ("01 - setsid()", &[EnvRestriction::Setsid]),
        ("02 - Close fds > 2", &[EnvRestriction::CloseExtraFds]),
        ("03 - No TERM env", &[EnvRestriction::NoTermEnv]),
        ("04 - New process group", &[EnvRestriction::NewProcessGroup]),
        ("05 - Ignore SIGHUP", &[EnvRestriction::IgnoreSighup]),
        ("06 - All restrictions", &[
            EnvRestriction::Setsid,
            EnvRestriction::CloseExtraFds,
            EnvRestriction::NoTermEnv,
            EnvRestriction::NewProcessGroup,
            EnvRestriction::IgnoreSighup,
        ]),
    ];

    eprintln!("\n{}", "=".repeat(60));
    eprintln!("  PTY Environment Diagnostic Matrix");
    eprintln!("{}\n", "=".repeat(60));

    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut baseline_ok = false;

    // Helper to report results
    let mut report = |label: &str, result: &DiagResult<String>| {
        match result {
            Ok(output) if output.contains("diag_ok") => {
                eprintln!("  [PASS] {label}");
                passed += 1;
                if label.starts_with("00") {
                    baseline_ok = true;
                }
            }
            Ok(output) => {
                eprintln!(
                    "  [FAIL] {label}: output missing 'diag_ok' (got {} bytes)",
                    output.len()
                );
                failed += 1;
            }
            Err(e) if e.message.contains("not found") => {
                eprintln!("  [SKIP] {label}: {}", e.message);
                skipped += 1;
            }
            Err(e) => {
                eprintln!("  [FAIL] {label}");
                eprintln!("         error: {}", e.message);
                eprintln!("         elapsed: {:.3}s", e.elapsed.as_secs_f64());
                if !e.stderr_output.is_empty() {
                    for line in e.stderr_output.lines().take(3) {
                        eprintln!("         stderr: {line}");
                    }
                }
                if let Some(status) = &e.exit_status {
                    eprintln!("         exit: {status}");
                }
                failed += 1;
            }
        }
    };

    // Run restriction-based tests
    for (label, restrictions) in restriction_variants {
        let result = run_diagnostic(restrictions);
        report(label, &result);
    }

    // Run socketpair test
    let sp_result = run_diagnostic_socketpair();
    report("08 - Unix socketpair (AF_UNIX)", &sp_result);

    // Run bun spawn test
    let bun_result = run_diagnostic_bun();
    report("09 - Bun.spawn (Claude Code runtime)", &bun_result);

    let total = passed + failed;
    eprintln!();
    if skipped > 0 {
        eprintln!("  Results: {passed}/{total} passed, {failed}/{total} failed, {skipped} skipped");
    } else {
        eprintln!("  Results: {passed}/{total} passed, {failed}/{total} failed");
    }
    eprintln!();

    // Baseline MUST pass — it's the control test.
    assert!(
        baseline_ok,
        "CRITICAL: Baseline test failed — the test harness itself is broken, \
         not an environment issue. Check that `mish serve` works standalone."
    );
}
