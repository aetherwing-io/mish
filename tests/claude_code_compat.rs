//! Claude Code Compatibility Tests
//!
//! Tests that `mish serve` works correctly under the exact conditions
//! Claude Code creates when spawning stdio MCP servers:
//!
//! 1. **Restricted environment** — the MCP SDK's `getDefaultEnvironment()`
//!    only passes HOME, LOGNAME, PATH, SHELL, TERM, USER. All other env
//!    vars are stripped.
//!
//! 2. **Protocol version negotiation** — current Claude Code (SDK 1.27.1)
//!    sends `protocolVersion: "2025-11-25"` and validates the server
//!    response against a supported set.
//!
//! 3. **Timing** — Claude Code sends initialize immediately after spawn,
//!    with a 30-second outer timeout.
//!
//! Source: @modelcontextprotocol/sdk `dist/esm/client/stdio.js`
//!   - `getDefaultEnvironment()` → whitelist
//!   - `DEFAULT_INHERITED_ENV_VARS` → [HOME, LOGNAME, PATH, SHELL, TERM, USER]
//!   - `spawn(command, args, { env, stdio: ['pipe','pipe',stderr], shell: false })`
//!
//! Run:
//!     cargo test --test claude_code_compat -- --nocapture

use serial_test::serial;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TIMEOUT_MS: u16 = 15_000;

/// The exact env vars the MCP SDK passes to child processes on macOS/Linux.
/// Source: @modelcontextprotocol/sdk `DEFAULT_INHERITED_ENV_VARS`
const MCP_SDK_WHITELIST: &[&str] = &["HOME", "LOGNAME", "PATH", "SHELL", "TERM", "USER"];

/// Protocol versions the MCP SDK considers valid in server responses.
/// Source: @modelcontextprotocol/sdk `SUPPORTED_PROTOCOL_VERSIONS`
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];

/// The protocol version current Claude Code (SDK 1.27.1) sends.
const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";

// =========================================================================
// Test server with configurable environment
// =========================================================================

struct CompatServer {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout_reader: BufReader<std::process::ChildStdout>,
    stderr_buf: Arc<Mutex<String>>,
    #[allow(dead_code)]
    started_at: Instant,
}

impl CompatServer {
    /// Spawn `mish serve` with a custom set of environment variables.
    /// If `env` is None, inherits full parent environment (control test).
    /// If `env` is Some(map), uses ONLY those vars (no inheritance).
    fn spawn_with_env(env: Option<HashMap<String, String>>) -> Self {
        let bin = env!("CARGO_BIN_EXE_mish");
        let started_at = Instant::now();

        let mut cmd = Command::new(bin);
        cmd.arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        match env {
            Some(vars) => {
                // Clear inherited env entirely, then set only our vars
                cmd.env_clear();
                for (k, v) in &vars {
                    cmd.env(k, v);
                }
            }
            None => {
                // Inherit full parent env (control)
            }
        }

        let mut child = cmd.spawn().expect("failed to start mish serve");

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let stdout_reader = BufReader::new(stdout);

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

    /// Build the MCP SDK whitelist env from current process env.
    /// This is exactly what `getDefaultEnvironment()` does.
    fn mcp_sdk_env() -> HashMap<String, String> {
        let mut env = HashMap::new();
        for key in MCP_SDK_WHITELIST {
            if let Ok(val) = std::env::var(key) {
                env.insert(key.to_string(), val);
            }
        }
        env
    }

    fn send(&mut self, json: &str) {
        let stdin = self.stdin.as_mut().expect("stdin closed");
        writeln!(stdin, "{}", json).expect("write to stdin");
        stdin.flush().expect("flush stdin");
    }

    fn read_line_timeout(&mut self, timeout_ms: u16) -> Option<String> {
        let fd = self.stdout_reader.get_ref().as_raw_fd();
        let borrowed = unsafe { std::os::unix::io::BorrowedFd::borrow_raw(fd) };
        let mut pfd = [nix::poll::PollFd::new(borrowed, nix::poll::PollFlags::POLLIN)];

        match nix::poll::poll(&mut pfd, timeout_ms) {
            Ok(0) => None,
            Ok(_) => {
                let mut line = String::new();
                match self.stdout_reader.read_line(&mut line) {
                    Ok(0) => None,
                    Ok(_) => Some(line),
                    Err(_) => None,
                }
            }
            Err(_) => None,
        }
    }

    fn stderr(&self) -> String {
        self.stderr_buf.lock().unwrap().clone()
    }

    fn shutdown(mut self) {
        self.stdin.take();
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

impl Drop for CompatServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// =========================================================================
// Helpers
// =========================================================================

fn init_request(protocol_version: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"{}","capabilities":{{}},"clientInfo":{{"name":"claude-code","version":"1.0.0"}}}}}}"#,
        protocol_version
    )
}

const INITIALIZED_NOTIF: &str =
    r#"{"jsonrpc":"2.0","id":null,"method":"notifications/initialized"}"#;
const TOOLS_LIST_REQ: &str = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;

fn parse_response(line: &str) -> serde_json::Value {
    serde_json::from_str(line.trim()).expect("should parse as JSON-RPC")
}

// =========================================================================
// Tests: Restricted Environment
// =========================================================================

/// Test 1: MCP SDK whitelist env — init succeeds.
///
/// Spawn mish with ONLY the 6 whitelist vars the MCP SDK passes.
/// The server must initialize successfully.
#[test]
#[serial(pty)]
fn test_restricted_env_init_succeeds() {
    let env = CompatServer::mcp_sdk_env();
    let mut server = CompatServer::spawn_with_env(Some(env));

    server.send(&init_request("2024-11-05"));
    let resp = server
        .read_line_timeout(TIMEOUT_MS)
        .expect("should get init response with restricted env");

    let parsed = parse_response(&resp);
    assert_eq!(parsed["result"]["serverInfo"]["name"], "mish");
    assert!(
        parsed["error"].is_null(),
        "init should succeed with restricted env, got error: {}",
        parsed["error"]
    );

    server.shutdown();
}

/// Test 2: MCP SDK whitelist env — full lifecycle.
///
/// init → notification → tools/list must all work with restricted env.
#[test]
#[serial(pty)]
fn test_restricted_env_full_lifecycle() {
    let env = CompatServer::mcp_sdk_env();
    let mut server = CompatServer::spawn_with_env(Some(env));

    // Initialize
    server.send(&init_request("2024-11-05"));
    let r1 = server.read_line_timeout(TIMEOUT_MS).expect("init response");
    let p1 = parse_response(&r1);
    assert_eq!(p1["result"]["serverInfo"]["name"], "mish");

    // Notification
    server.send(INITIALIZED_NOTIF);
    std::thread::sleep(Duration::from_millis(100));

    // tools/list
    server.send(TOOLS_LIST_REQ);
    let r2 = server
        .read_line_timeout(TIMEOUT_MS)
        .expect("tools/list response");
    let p2 = parse_response(&r2);
    let tools = p2["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 5, "should have 5 tools");

    server.shutdown();
}

/// Test 3: MCP SDK whitelist env — stderr silent.
///
/// With restricted env, startup must not produce stderr output.
/// Missing env vars like XDG_RUNTIME_DIR should be handled gracefully.
#[test]
#[serial(pty)]
fn test_restricted_env_stderr_silent() {
    let env = CompatServer::mcp_sdk_env();
    let mut server = CompatServer::spawn_with_env(Some(env));

    // Wait for startup
    std::thread::sleep(Duration::from_millis(1000));

    // Init to verify server is alive
    server.send(&init_request("2024-11-05"));
    let _ = server.read_line_timeout(TIMEOUT_MS).expect("init response");

    let stderr = server.stderr();
    assert!(
        stderr.is_empty(),
        "stderr must be empty with restricted env, got:\n{}",
        &stderr[..stderr.len().min(500)]
    );

    server.shutdown();
}

/// Test 4: Minimal env — only PATH.
///
/// Extreme test: spawn with ONLY PATH set. The server should still
/// start (it needs PATH to find bash, but HOME/USER/etc. are optional).
#[test]
#[serial(pty)]
fn test_minimal_env_path_only() {
    let mut env = HashMap::new();
    if let Ok(path) = std::env::var("PATH") {
        env.insert("PATH".to_string(), path);
    }
    // SHELL is needed for session creation, add it
    env.insert("SHELL".to_string(), "/bin/bash".to_string());

    let mut server = CompatServer::spawn_with_env(Some(env));

    server.send(&init_request("2024-11-05"));
    let resp = server
        .read_line_timeout(TIMEOUT_MS)
        .expect("should get init response with minimal env");

    let parsed = parse_response(&resp);
    assert_eq!(
        parsed["result"]["serverInfo"]["name"], "mish",
        "server should identify as mish even with minimal env"
    );

    server.shutdown();
}

// =========================================================================
// Tests: Protocol Version Negotiation
// =========================================================================

/// Test 5: Server responds with a supported protocol version.
///
/// The MCP SDK validates the response's protocolVersion against
/// SUPPORTED_PROTOCOL_VERSIONS. Mish must respond with one of them.
#[test]
#[serial(pty)]
fn test_protocol_version_in_supported_set() {
    let mut server = CompatServer::spawn_with_env(None);

    server.send(&init_request(LATEST_PROTOCOL_VERSION));
    let resp = server.read_line_timeout(TIMEOUT_MS).expect("init response");
    let parsed = parse_response(&resp);

    let server_version = parsed["result"]["protocolVersion"]
        .as_str()
        .expect("protocolVersion should be a string");

    assert!(
        SUPPORTED_PROTOCOL_VERSIONS.contains(&server_version),
        "server responded with protocolVersion '{}' which is NOT in SDK supported set: {:?}",
        server_version,
        SUPPORTED_PROTOCOL_VERSIONS
    );
}

/// Test 6: Server responds correctly to latest protocol version.
///
/// Claude Code (SDK 1.27.1) sends "2025-11-25". The server must respond
/// without error.
#[test]
#[serial(pty)]
fn test_latest_protocol_version_accepted() {
    let mut server = CompatServer::spawn_with_env(None);

    server.send(&init_request("2025-11-25"));
    let resp = server.read_line_timeout(TIMEOUT_MS).expect("init response");
    let parsed = parse_response(&resp);

    assert!(
        parsed["error"].is_null(),
        "server should accept protocolVersion 2025-11-25, got error: {}",
        parsed["error"]
    );
    assert_eq!(parsed["result"]["serverInfo"]["name"], "mish");
}

/// Test 7: Server responds correctly to each supported version.
///
/// Test all 5 protocol versions the SDK supports.
#[test]
#[serial(pty)]
fn test_all_supported_protocol_versions() {
    for version in SUPPORTED_PROTOCOL_VERSIONS {
        let mut server = CompatServer::spawn_with_env(None);

        server.send(&init_request(version));
        let resp = server
            .read_line_timeout(TIMEOUT_MS)
            .unwrap_or_else(|| panic!("no response for protocolVersion {}", version));
        let parsed = parse_response(&resp);

        assert!(
            parsed["error"].is_null(),
            "server should accept protocolVersion '{}', got error: {}",
            version,
            parsed["error"]
        );
        assert_eq!(
            parsed["result"]["serverInfo"]["name"], "mish",
            "server should identify as mish for version {}",
            version
        );

        server.shutdown();
    }
}

/// Test 8: Init response has all required fields for SDK validation.
///
/// The SDK validates the response against InitializeResultSchema (Zod):
/// - protocolVersion: string (required)
/// - capabilities: object (required)
/// - serverInfo: { name: string, version: string } (required)
#[test]
#[serial(pty)]
fn test_init_response_schema_compliance() {
    let mut server = CompatServer::spawn_with_env(None);

    server.send(&init_request(LATEST_PROTOCOL_VERSION));
    let resp = server.read_line_timeout(TIMEOUT_MS).expect("init response");
    let parsed = parse_response(&resp);

    let result = &parsed["result"];

    // protocolVersion — required string
    assert!(
        result["protocolVersion"].is_string(),
        "protocolVersion must be a string, got: {}",
        result["protocolVersion"]
    );

    // capabilities — required object
    assert!(
        result["capabilities"].is_object(),
        "capabilities must be an object, got: {}",
        result["capabilities"]
    );

    // serverInfo — required object with name and version
    assert!(
        result["serverInfo"].is_object(),
        "serverInfo must be an object, got: {}",
        result["serverInfo"]
    );
    assert!(
        result["serverInfo"]["name"].is_string(),
        "serverInfo.name must be a string"
    );
    assert!(
        result["serverInfo"]["version"].is_string(),
        "serverInfo.version must be a string"
    );

    // camelCase field names (not snake_case!)
    assert!(
        result.get("protocolVersion").is_some(),
        "must use camelCase 'protocolVersion', not snake_case"
    );
    assert!(
        result.get("serverInfo").is_some(),
        "must use camelCase 'serverInfo', not snake_case"
    );
    assert!(
        result.get("protocol_version").is_none(),
        "must NOT use snake_case 'protocol_version'"
    );
    assert!(
        result.get("server_info").is_none(),
        "must NOT use snake_case 'server_info'"
    );

    server.shutdown();
}

// =========================================================================
// Tests: Timing
// =========================================================================

/// Test 9: Immediate init (zero delay after spawn).
///
/// Claude Code sends initialize immediately after the child process
/// emits the 'spawn' event. The server must handle this without
/// dropping or corrupting the request.
#[test]
#[serial(pty)]
fn test_immediate_init_restricted_env() {
    let env = CompatServer::mcp_sdk_env();
    let mut server = CompatServer::spawn_with_env(Some(env));

    // Send immediately — no sleep
    server.send(&init_request(LATEST_PROTOCOL_VERSION));

    let resp = server
        .read_line_timeout(TIMEOUT_MS)
        .expect("should respond to immediate init");
    let parsed = parse_response(&resp);
    assert_eq!(parsed["result"]["serverInfo"]["name"], "mish");

    server.shutdown();
}

/// Test 10: Init response time is under 5 seconds.
///
/// Claude Code has a 30-second outer timeout. If mish takes too long
/// to respond to initialize (e.g., slow grammar loading), the connection
/// is killed. We test for < 5s to catch performance regressions early.
#[test]
#[serial(pty)]
fn test_init_response_time_under_5s() {
    let env = CompatServer::mcp_sdk_env();
    let mut server = CompatServer::spawn_with_env(Some(env));

    let start = Instant::now();
    server.send(&init_request(LATEST_PROTOCOL_VERSION));
    let resp = server
        .read_line_timeout(TIMEOUT_MS)
        .expect("should get init response");
    let elapsed = start.elapsed();

    let parsed = parse_response(&resp);
    assert_eq!(parsed["result"]["serverInfo"]["name"], "mish");

    assert!(
        elapsed < Duration::from_secs(5),
        "init response took {:?} — must be under 5s (Claude Code timeout is 30s)",
        elapsed
    );

    eprintln!(
        "  init response time: {:.0}ms",
        elapsed.as_millis()
    );

    server.shutdown();
}

// =========================================================================
// Tests: Combined (the real Claude Code simulation)
// =========================================================================

/// Test 11: Full Claude Code simulation.
///
/// Restricted env + latest protocol version + immediate init + full
/// lifecycle. This is the closest we can get to reproducing what
/// Claude Code actually does.
#[test]
#[serial(pty)]
fn test_full_claude_code_simulation() {
    let env = CompatServer::mcp_sdk_env();
    let mut server = CompatServer::spawn_with_env(Some(env));

    // Step 1: Send initialize immediately (like Claude Code does)
    let start = Instant::now();
    server.send(&init_request(LATEST_PROTOCOL_VERSION));

    let r1 = server
        .read_line_timeout(TIMEOUT_MS)
        .expect("init response in Claude Code simulation");
    let p1 = parse_response(&r1);

    // Validate init response matches SDK expectations
    assert_eq!(p1["result"]["serverInfo"]["name"], "mish");
    let proto = p1["result"]["protocolVersion"].as_str().unwrap();
    assert!(
        SUPPORTED_PROTOCOL_VERSIONS.contains(&proto),
        "protocolVersion '{}' not in SDK supported set",
        proto
    );

    // Step 2: Send notifications/initialized
    server.send(INITIALIZED_NOTIF);
    std::thread::sleep(Duration::from_millis(50));

    // Step 3: tools/list
    server.send(TOOLS_LIST_REQ);
    let r2 = server
        .read_line_timeout(TIMEOUT_MS)
        .expect("tools/list response");
    let p2 = parse_response(&r2);
    let tools = p2["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 5);

    let elapsed = start.elapsed();
    eprintln!("  full Claude Code simulation: {:.0}ms", elapsed.as_millis());

    // Verify stderr was clean through the whole lifecycle
    let stderr = server.stderr();
    let relevant_stderr: Vec<&str> = stderr
        .lines()
        .filter(|l| !l.contains("shutdown") && !l.contains("SIGTERM"))
        .collect();
    assert!(
        relevant_stderr.is_empty(),
        "stderr should be clean during Claude Code simulation:\n{}",
        relevant_stderr.join("\n")
    );

    server.shutdown();
}

/// Test 12: Matrix summary.
#[test]
#[serial(pty)]
fn test_claude_code_compat_matrix() {
    eprintln!("\n{}", "=".repeat(60));
    eprintln!("  Claude Code Compatibility Matrix");
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

    // Env tests
    check!("Restricted env: init succeeds", {
        let env = CompatServer::mcp_sdk_env();
        let mut s = CompatServer::spawn_with_env(Some(env));
        s.send(&init_request("2024-11-05"));
        let r = s.read_line_timeout(TIMEOUT_MS).unwrap();
        let p = parse_response(&r);
        assert_eq!(p["result"]["serverInfo"]["name"], "mish");
        s.shutdown();
    });

    check!("Restricted env: full lifecycle", {
        let env = CompatServer::mcp_sdk_env();
        let mut s = CompatServer::spawn_with_env(Some(env));
        s.send(&init_request("2024-11-05"));
        let _ = s.read_line_timeout(TIMEOUT_MS).unwrap();
        s.send(INITIALIZED_NOTIF);
        std::thread::sleep(Duration::from_millis(100));
        s.send(TOOLS_LIST_REQ);
        let r = s.read_line_timeout(TIMEOUT_MS).unwrap();
        let p = parse_response(&r);
        assert_eq!(p["result"]["tools"].as_array().unwrap().len(), 5);
        s.shutdown();
    });

    check!("Restricted env: stderr silent", {
        let env = CompatServer::mcp_sdk_env();
        let s = CompatServer::spawn_with_env(Some(env));
        std::thread::sleep(Duration::from_millis(1000));
        assert!(s.stderr().is_empty(), "stderr: {:?}", s.stderr());
        s.shutdown();
    });

    // Protocol version tests
    check!("Protocol version in supported set", {
        let mut s = CompatServer::spawn_with_env(None);
        s.send(&init_request(LATEST_PROTOCOL_VERSION));
        let r = s.read_line_timeout(TIMEOUT_MS).unwrap();
        let p = parse_response(&r);
        let v = p["result"]["protocolVersion"].as_str().unwrap();
        assert!(SUPPORTED_PROTOCOL_VERSIONS.contains(&v), "unsupported: {}", v);
        s.shutdown();
    });

    check!("Latest version (2025-11-25) accepted", {
        let mut s = CompatServer::spawn_with_env(None);
        s.send(&init_request("2025-11-25"));
        let r = s.read_line_timeout(TIMEOUT_MS).unwrap();
        let p = parse_response(&r);
        assert!(p["error"].is_null());
        s.shutdown();
    });

    check!("Init response schema compliant", {
        let mut s = CompatServer::spawn_with_env(None);
        s.send(&init_request(LATEST_PROTOCOL_VERSION));
        let r = s.read_line_timeout(TIMEOUT_MS).unwrap();
        let p = parse_response(&r);
        let result = &p["result"];
        assert!(result["protocolVersion"].is_string());
        assert!(result["capabilities"].is_object());
        assert!(result["serverInfo"]["name"].is_string());
        assert!(result["serverInfo"]["version"].is_string());
        // camelCase, not snake_case
        assert!(result.get("protocol_version").is_none());
        assert!(result.get("server_info").is_none());
        s.shutdown();
    });

    // Timing tests
    check!("Immediate init (zero delay)", {
        let env = CompatServer::mcp_sdk_env();
        let mut s = CompatServer::spawn_with_env(Some(env));
        s.send(&init_request(LATEST_PROTOCOL_VERSION));
        let r = s.read_line_timeout(TIMEOUT_MS).unwrap();
        assert_eq!(parse_response(&r)["result"]["serverInfo"]["name"], "mish");
        s.shutdown();
    });

    check!("Init under 5s (Claude Code timeout is 30s)", {
        let env = CompatServer::mcp_sdk_env();
        let mut s = CompatServer::spawn_with_env(Some(env));
        let start = Instant::now();
        s.send(&init_request(LATEST_PROTOCOL_VERSION));
        let _ = s.read_line_timeout(TIMEOUT_MS).unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
        s.shutdown();
    });

    // Full simulation
    check!("Full Claude Code simulation", {
        let env = CompatServer::mcp_sdk_env();
        let mut s = CompatServer::spawn_with_env(Some(env));
        s.send(&init_request(LATEST_PROTOCOL_VERSION));
        let r1 = s.read_line_timeout(TIMEOUT_MS).unwrap();
        let p1 = parse_response(&r1);
        assert_eq!(p1["result"]["serverInfo"]["name"], "mish");
        s.send(INITIALIZED_NOTIF);
        std::thread::sleep(Duration::from_millis(50));
        s.send(TOOLS_LIST_REQ);
        let r2 = s.read_line_timeout(TIMEOUT_MS).unwrap();
        let p2 = parse_response(&r2);
        assert_eq!(p2["result"]["tools"].as_array().unwrap().len(), 5);
        let stderr = s.stderr();
        let relevant: Vec<&str> = stderr.lines()
            .filter(|l| !l.contains("shutdown") && !l.contains("SIGTERM"))
            .collect();
        assert!(relevant.is_empty(), "stderr: {:?}", relevant);
        s.shutdown();
    });

    let total = passed + failed;
    eprintln!();
    eprintln!("  Results: {passed}/{total} passed, {failed}/{total} failed");
    eprintln!();

    assert_eq!(failed, 0, "{failed}/{total} compatibility checks failed");
}
