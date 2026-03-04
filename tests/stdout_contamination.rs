//! Stdout Contamination Tests
//!
//! The MCP SDK's ReadBuffer expects ONLY valid JSON-RPC on stdout from
//! the moment the server process starts. Any stray bytes — debug prints,
//! warnings, tracing output, or even a single newline — cause the SDK to
//! treat it as a protocol violation and kill the connection.
//!
//! These tests validate that `mish serve` produces zero stdout output
//! before the first JSON-RPC exchange, zero stderr during normal startup,
//! and that every byte on stdout is valid JSON-RPC.
//!
//! Run:
//!     cargo test --test stdout_contamination -- --nocapture

use serial_test::serial;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TIMEOUT_MS: u16 = 10_000;

// =========================================================================
// Test server helper
// =========================================================================

struct TestServer {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout_reader: BufReader<std::process::ChildStdout>,
    stderr_buf: Arc<Mutex<String>>,
    #[allow(dead_code)]
    started_at: Instant,
}

impl TestServer {
    fn spawn() -> Self {
        let bin = env!("CARGO_BIN_EXE_mish");
        let started_at = Instant::now();

        let mut child = Command::new(bin)
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("SHELL", "/bin/bash")
            .spawn()
            .expect("failed to start mish serve");

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let stdout_reader = BufReader::new(stdout);

        // Drain stderr in background
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
            stdout_reader,
            stderr_buf,
            started_at,
        }
    }

    /// Send a line to stdin and flush.
    fn send(&mut self, json: &str) {
        let stdin = self.stdin.as_mut().expect("stdin closed");
        writeln!(stdin, "{}", json).expect("write to stdin");
        stdin.flush().expect("flush stdin");
    }

    /// Read one line from stdout with a timeout.
    /// Returns None on timeout, Some(line) on data.
    fn read_line_timeout(&mut self, timeout_ms: u16) -> Option<String> {
        let fd = self.stdout_reader.get_ref().as_raw_fd();
        let borrowed = unsafe { std::os::unix::io::BorrowedFd::borrow_raw(fd) };
        let mut pfd = [nix::poll::PollFd::new(borrowed, nix::poll::PollFlags::POLLIN)];

        match nix::poll::poll(&mut pfd, timeout_ms) {
            Ok(0) => None, // timeout
            Ok(_) => {
                let mut line = String::new();
                match self.stdout_reader.read_line(&mut line) {
                    Ok(0) => None, // EOF
                    Ok(_) => Some(line),
                    Err(_) => None,
                }
            }
            Err(_) => None,
        }
    }

    /// Read ALL available bytes from stdout without blocking.
    /// Uses a short poll timeout to drain any buffered output.
    fn drain_stdout_nonblocking(&mut self) -> String {
        let mut collected = String::new();
        while let Some(line) = self.read_line_timeout(100) {
            collected.push_str(&line);
        }
        collected
    }

    /// Get current stderr contents.
    fn stderr(&self) -> String {
        self.stderr_buf.lock().unwrap().clone()
    }

    /// Shut down by closing stdin and waiting.
    fn shutdown(mut self) {
        self.stdin.take(); // Drop stdin → EOF
        let start = Instant::now();
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if start.elapsed() > Duration::from_secs(10) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => return,
            }
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// =========================================================================
// JSON-RPC helpers
// =========================================================================

const INITIALIZE_REQ: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"contamination-test"}}}"#;
const INITIALIZED_NOTIF: &str = r#"{"jsonrpc":"2.0","id":null,"method":"notifications/initialized"}"#;
const TOOLS_LIST_REQ: &str = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;

fn is_valid_jsonrpc(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(v) => v["jsonrpc"].as_str() == Some("2.0"),
        Err(_) => false,
    }
}

// =========================================================================
// Tests
// =========================================================================

/// Test 1: No stdout bytes before first request.
///
/// After spawning `mish serve`, stdout must be completely silent until
/// we send the first JSON-RPC request. Any bytes here would corrupt the
/// MCP SDK's ReadBuffer.
#[test]
#[serial(pty)]
fn test_no_stdout_before_first_request() {
    let mut server = TestServer::spawn();

    // Wait for the server to fully initialize (config loading, PID file, etc.)
    std::thread::sleep(Duration::from_millis(500));

    // Try to read from stdout — should get nothing (timeout)
    let pre_init_output = server.drain_stdout_nonblocking();

    assert!(
        pre_init_output.is_empty(),
        "stdout must be empty before first request, but got {} bytes: {:?}",
        pre_init_output.len(),
        &pre_init_output[..pre_init_output.len().min(200)]
    );

    server.shutdown();
}

/// Test 2: No stdout bytes between spawn and initialize response.
///
/// The first bytes on stdout must be the JSON-RPC response to our
/// initialize request — nothing before it.
#[test]
#[serial(pty)]
fn test_first_stdout_bytes_are_init_response() {
    let mut server = TestServer::spawn();

    // Wait for startup to complete
    std::thread::sleep(Duration::from_millis(300));

    // Send initialize — this is the first request
    server.send(INITIALIZE_REQ);

    // The first line on stdout should be the initialize response
    let first_line = server
        .read_line_timeout(TIMEOUT_MS)
        .expect("should get initialize response");

    // It must be valid JSON-RPC
    assert!(
        is_valid_jsonrpc(&first_line),
        "first stdout output must be valid JSON-RPC, got: {:?}",
        &first_line[..first_line.len().min(200)]
    );

    // It must be the initialize response (id: 1)
    let parsed: serde_json::Value =
        serde_json::from_str(first_line.trim()).expect("should parse as JSON");
    assert_eq!(
        parsed["id"], 1,
        "first response must have id:1 (initialize), got: {}",
        parsed["id"]
    );
    assert_eq!(
        parsed["result"]["serverInfo"]["name"], "mish",
        "init response must identify as mish"
    );

    server.shutdown();
}

/// Test 3: Every stdout line is valid JSON-RPC.
///
/// Over a full init + tools/list lifecycle, every single line on stdout
/// must be parseable JSON with jsonrpc:"2.0". No warnings, no debug
/// output, no empty lines.
#[test]
#[serial(pty)]
fn test_every_stdout_line_is_valid_jsonrpc() {
    let mut server = TestServer::spawn();
    std::thread::sleep(Duration::from_millis(300));

    // Send: initialize, notification, tools/list
    server.send(INITIALIZE_REQ);
    let line1 = server
        .read_line_timeout(TIMEOUT_MS)
        .expect("init response");

    server.send(INITIALIZED_NOTIF);
    // Notifications produce no response — wait briefly to confirm
    std::thread::sleep(Duration::from_millis(100));

    server.send(TOOLS_LIST_REQ);
    let line2 = server
        .read_line_timeout(TIMEOUT_MS)
        .expect("tools/list response");

    // Close stdin to trigger shutdown
    server.stdin.take();
    std::thread::sleep(Duration::from_millis(500));

    // Drain any remaining stdout
    let remaining = server.drain_stdout_nonblocking();

    // Validate every line
    let mut all_lines = vec![
        ("initialize response", line1),
        ("tools/list response", line2),
    ];
    for line in remaining.lines() {
        if !line.trim().is_empty() {
            all_lines.push(("post-shutdown", line.to_string()));
        }
    }

    for (label, line) in &all_lines {
        assert!(
            is_valid_jsonrpc(line),
            "[{label}] stdout line is not valid JSON-RPC: {:?}",
            &line[..line.len().min(200)]
        );
    }
}

/// Test 4: Response count matches request count.
///
/// Notifications must not produce output. We send exactly 2 requests
/// (initialize + tools/list) and 1 notification, and must get exactly
/// 2 lines on stdout.
#[test]
#[serial(pty)]
fn test_response_count_matches_request_count() {
    let mut server = TestServer::spawn();
    std::thread::sleep(Duration::from_millis(300));

    // 2 requests + 1 notification = expect 2 responses
    server.send(INITIALIZE_REQ);
    let r1 = server.read_line_timeout(TIMEOUT_MS).expect("init response");

    server.send(INITIALIZED_NOTIF);
    std::thread::sleep(Duration::from_millis(100));

    server.send(TOOLS_LIST_REQ);
    let r2 = server
        .read_line_timeout(TIMEOUT_MS)
        .expect("tools/list response");

    // Close stdin
    server.stdin.take();
    std::thread::sleep(Duration::from_millis(500));

    // Drain remaining — should be empty (no extra output after shutdown)
    let remaining = server.drain_stdout_nonblocking();
    let extra_lines: Vec<&str> = remaining
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();

    assert!(
        extra_lines.is_empty(),
        "expected 0 extra stdout lines after 2 responses, got {}: {:?}",
        extra_lines.len(),
        extra_lines
    );

    // Verify the 2 responses have correct ids
    let p1: serde_json::Value = serde_json::from_str(r1.trim()).unwrap();
    let p2: serde_json::Value = serde_json::from_str(r2.trim()).unwrap();
    assert_eq!(p1["id"], 1);
    assert_eq!(p2["id"], 2);
}

/// Test 5: Stderr is silent during startup.
///
/// During the startup phase (before first request), stderr must produce
/// zero output. Any stderr during startup (eprintln! from config loading,
/// grammar warnings, stale PID warnings) can cause Claude Code to flag
/// the server as problematic.
#[test]
#[serial(pty)]
fn test_stderr_silent_during_startup() {
    let server = TestServer::spawn();

    // Wait for full startup (config load, PID file, grammar parsing, etc.)
    std::thread::sleep(Duration::from_millis(1000));

    let startup_stderr = server.stderr();

    assert!(
        startup_stderr.is_empty(),
        "stderr must be empty during startup, but got {} bytes:\n{}",
        startup_stderr.len(),
        &startup_stderr[..startup_stderr.len().min(500)]
    );

    server.shutdown();
}

/// Test 6: Stderr is silent during normal MCP lifecycle.
///
/// Over a full init → notification → tools/list lifecycle (no errors
/// triggered), stderr must remain empty.
#[test]
#[serial(pty)]
fn test_stderr_silent_during_normal_lifecycle() {
    let mut server = TestServer::spawn();
    std::thread::sleep(Duration::from_millis(300));

    // Normal lifecycle
    server.send(INITIALIZE_REQ);
    let _ = server.read_line_timeout(TIMEOUT_MS).expect("init response");

    server.send(INITIALIZED_NOTIF);
    std::thread::sleep(Duration::from_millis(100));

    server.send(TOOLS_LIST_REQ);
    let _ = server
        .read_line_timeout(TIMEOUT_MS)
        .expect("tools/list response");

    // Check stderr before shutdown
    let lifecycle_stderr = server.stderr();

    // Filter out shutdown-related messages (those are acceptable during teardown)
    let non_shutdown_stderr: String = lifecycle_stderr
        .lines()
        .filter(|l| !l.contains("shutdown") && !l.contains("SIGTERM") && !l.contains("SIGINT"))
        .collect::<Vec<&str>>()
        .join("\n");

    assert!(
        non_shutdown_stderr.trim().is_empty(),
        "stderr must be empty during normal lifecycle (excluding shutdown), but got:\n{}",
        &non_shutdown_stderr[..non_shutdown_stderr.len().min(500)]
    );

    server.shutdown();
}

/// Test 7: Rapid init — no stdout leaks under fast connection.
///
/// Claude Code connects and sends initialize within milliseconds of
/// spawning the server. Test that even with zero delay between spawn
/// and first request, no stray bytes appear before the response.
#[test]
#[serial(pty)]
fn test_rapid_init_no_stdout_leak() {
    let mut server = TestServer::spawn();

    // Send initialize immediately — no sleep
    server.send(INITIALIZE_REQ);

    let response = server
        .read_line_timeout(TIMEOUT_MS)
        .expect("should get response even with rapid init");

    // The response must be valid JSON-RPC
    assert!(
        is_valid_jsonrpc(&response),
        "rapid init response must be valid JSON-RPC: {:?}",
        &response[..response.len().min(200)]
    );

    let parsed: serde_json::Value = serde_json::from_str(response.trim()).unwrap();
    assert_eq!(parsed["id"], 1);
    assert_eq!(parsed["result"]["serverInfo"]["name"], "mish");

    server.shutdown();
}

/// Test 8: Multiple rapid requests — no interleaved garbage.
///
/// Send initialize + tools/list back-to-back (pipeline style, like
/// Claude Code does). Verify responses arrive in order with no garbage
/// bytes between them.
#[test]
#[serial(pty)]
fn test_pipelined_requests_no_interleaving() {
    let mut server = TestServer::spawn();

    // Pipeline: send both requests immediately
    server.send(INITIALIZE_REQ);
    server.send(TOOLS_LIST_REQ);

    // Read first response
    let r1 = server
        .read_line_timeout(TIMEOUT_MS)
        .expect("init response");
    let p1: serde_json::Value = serde_json::from_str(r1.trim()).expect("parse r1");

    // Read second response
    let r2 = server
        .read_line_timeout(TIMEOUT_MS)
        .expect("tools/list response");
    let p2: serde_json::Value = serde_json::from_str(r2.trim()).expect("parse r2");

    // Verify ordering: init (id:1) then tools/list (id:2)
    assert_eq!(p1["id"], 1, "first response should be init (id:1)");
    assert_eq!(p2["id"], 2, "second response should be tools/list (id:2)");

    // Both must be valid JSON-RPC
    assert!(is_valid_jsonrpc(&r1), "r1 not valid JSON-RPC");
    assert!(is_valid_jsonrpc(&r2), "r2 not valid JSON-RPC");

    server.shutdown();
}

/// Test 9: Matrix summary — run all contamination checks in one view.
#[test]
#[serial(pty)]
fn test_contamination_matrix() {
    eprintln!("\n{}", "=".repeat(60));
    eprintln!("  Stdout Contamination Matrix");
    eprintln!("{}\n", "=".repeat(60));

    let mut passed = 0usize;
    let mut failed = 0usize;

    macro_rules! check {
        ($label:expr, $body:expr) => {{
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $body));
            match result {
                Ok(()) => {
                    eprintln!("  [PASS] {}", $label);
                    passed += 1;
                }
                Err(e) => {
                    let msg = if let Some(s) = e.downcast_ref::<String>() {
                        s.clone()
                    } else if let Some(s) = e.downcast_ref::<&str>() {
                        s.to_string()
                    } else {
                        "unknown panic".to_string()
                    };
                    eprintln!("  [FAIL] {}", $label);
                    eprintln!("         {}", &msg[..msg.len().min(200)]);
                    failed += 1;
                }
            }
        }};
    }

    check!("No stdout before first request", {
        let mut s = TestServer::spawn();
        std::thread::sleep(Duration::from_millis(500));
        let pre = s.drain_stdout_nonblocking();
        assert!(pre.is_empty(), "got pre-init stdout: {:?}", pre);
        s.shutdown();
    });

    check!("First stdout is init response", {
        let mut s = TestServer::spawn();
        std::thread::sleep(Duration::from_millis(300));
        s.send(INITIALIZE_REQ);
        let line = s.read_line_timeout(TIMEOUT_MS).unwrap();
        assert!(is_valid_jsonrpc(&line));
        let p: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(p["id"], 1);
        s.shutdown();
    });

    check!("Every line is valid JSON-RPC", {
        let mut s = TestServer::spawn();
        std::thread::sleep(Duration::from_millis(300));
        s.send(INITIALIZE_REQ);
        let l1 = s.read_line_timeout(TIMEOUT_MS).unwrap();
        assert!(is_valid_jsonrpc(&l1));
        s.send(TOOLS_LIST_REQ);
        let l2 = s.read_line_timeout(TIMEOUT_MS).unwrap();
        assert!(is_valid_jsonrpc(&l2));
        s.shutdown();
    });

    check!("Correct response count (2 req + 1 notif = 2 resp)", {
        let mut s = TestServer::spawn();
        std::thread::sleep(Duration::from_millis(300));
        s.send(INITIALIZE_REQ);
        let _ = s.read_line_timeout(TIMEOUT_MS).unwrap();
        s.send(INITIALIZED_NOTIF);
        std::thread::sleep(Duration::from_millis(100));
        s.send(TOOLS_LIST_REQ);
        let _ = s.read_line_timeout(TIMEOUT_MS).unwrap();
        s.stdin.take();
        std::thread::sleep(Duration::from_millis(500));
        let extra = s.drain_stdout_nonblocking();
        let extra_count = extra.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(extra_count, 0, "got {} extra lines", extra_count);
    });

    check!("Stderr silent during startup", {
        let s = TestServer::spawn();
        std::thread::sleep(Duration::from_millis(1000));
        let stderr = s.stderr();
        assert!(stderr.is_empty(), "startup stderr: {:?}", stderr);
        s.shutdown();
    });

    check!("Stderr silent during lifecycle", {
        let mut s = TestServer::spawn();
        std::thread::sleep(Duration::from_millis(300));
        s.send(INITIALIZE_REQ);
        let _ = s.read_line_timeout(TIMEOUT_MS).unwrap();
        s.send(INITIALIZED_NOTIF);
        std::thread::sleep(Duration::from_millis(100));
        s.send(TOOLS_LIST_REQ);
        let _ = s.read_line_timeout(TIMEOUT_MS).unwrap();
        let stderr = s.stderr();
        let relevant: Vec<&str> = stderr
            .lines()
            .filter(|l| !l.contains("shutdown") && !l.contains("SIGTERM"))
            .collect();
        assert!(
            relevant.is_empty(),
            "lifecycle stderr: {:?}",
            relevant
        );
        s.shutdown();
    });

    check!("Rapid init (zero delay)", {
        let mut s = TestServer::spawn();
        s.send(INITIALIZE_REQ);
        let line = s.read_line_timeout(TIMEOUT_MS).unwrap();
        assert!(is_valid_jsonrpc(&line));
        s.shutdown();
    });

    check!("Pipelined requests (no interleaving)", {
        let mut s = TestServer::spawn();
        s.send(INITIALIZE_REQ);
        s.send(TOOLS_LIST_REQ);
        let r1 = s.read_line_timeout(TIMEOUT_MS).unwrap();
        let r2 = s.read_line_timeout(TIMEOUT_MS).unwrap();
        let p1: serde_json::Value = serde_json::from_str(r1.trim()).unwrap();
        let p2: serde_json::Value = serde_json::from_str(r2.trim()).unwrap();
        assert_eq!(p1["id"], 1);
        assert_eq!(p2["id"], 2);
    });

    let total = passed + failed;
    eprintln!();
    eprintln!("  Results: {passed}/{total} passed, {failed}/{total} failed");
    eprintln!();

    assert_eq!(failed, 0, "{failed}/{total} contamination checks failed");
}
