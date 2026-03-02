//! End-to-end integration tests for the mish MCP server (`mish serve`).
//!
//! Tests the compiled binary by spawning `mish serve` as a subprocess,
//! sending JSON-RPC requests via stdin, and verifying responses on stdout.
//!
//! Test categories:
//! 1. sh_session create/list/close lifecycle
//! 2. sh_run with real commands through category router
//! 3. Process table digest presence on every response
//! 4. Error codes for invalid requests
//! 5. Graceful shutdown on EOF
//! 6. Initialize response verification
//! 7. sh_spawn + sh_interact background process lifecycle

use serial_test::serial;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::process::{Command, Stdio};
use std::time::Duration;

// =========================================================================
// Test harness
// =========================================================================

/// A handle to a running `mish serve` subprocess.
struct MishServer {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    reader: BufReader<std::process::ChildStdout>,
}

impl MishServer {
    /// Spawn `mish serve` with piped stdin/stdout.
    fn start() -> Self {
        let bin = env!("CARGO_BIN_EXE_mish");
        let mut child = Command::new(bin)
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Force bash for deterministic boundary detection.
            // The "main" session uses $SHELL; zsh on macOS can differ
            // in precmd/PROMPT_COMMAND behavior across environments.
            .env("SHELL", "/bin/bash")
            .spawn()
            .expect("failed to start mish serve");

        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let reader = BufReader::new(stdout);

        Self {
            child,
            stdin: Some(stdin),
            reader,
        }
    }

    fn stdin(&mut self) -> &mut std::process::ChildStdin {
        self.stdin.as_mut().expect("stdin already closed")
    }

    /// Send a JSON-RPC request and read the response.
    fn request(&mut self, json: &str) -> serde_json::Value {
        writeln!(self.stdin(), "{}", json).expect("write to stdin");
        self.stdin().flush().expect("flush stdin");

        // Poll stdout with 30s timeout to prevent infinite hang
        let fd = self.reader.get_ref().as_raw_fd();
        let borrowed = unsafe { std::os::unix::io::BorrowedFd::borrow_raw(fd) };
        let mut pfd = [nix::poll::PollFd::new(borrowed, nix::poll::PollFlags::POLLIN)];
        match nix::poll::poll(&mut pfd, 30_000u16) {
            Ok(0) => panic!(
                "timeout (30s) waiting for server response to: {}",
                &json[..json.len().min(120)]
            ),
            Ok(_) => {}
            Err(nix::Error::EINTR) => {}
            Err(e) => panic!("poll error: {e}"),
        }

        let mut line = String::new();
        self.reader
            .read_line(&mut line)
            .expect("read response from stdout");
        assert!(!line.trim().is_empty(), "server closed stdout unexpectedly");
        serde_json::from_str(line.trim()).expect("parse JSON-RPC response")
    }

    /// Send a JSON-RPC notification (no response expected).
    fn notify(&mut self, json: &str) {
        writeln!(self.stdin(), "{}", json).expect("write to stdin");
        self.stdin().flush().expect("flush stdin");
        // Small delay to let the server process the notification.
        std::thread::sleep(Duration::from_millis(50));
    }

    /// Run the MCP initialization handshake (initialize + notifications/initialized).
    fn init(&mut self) {
        let resp = self.request(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"2024-11-05","capabilities":{},"client_info":{"name":"integration-test"}}}"#,
        );
        assert_eq!(resp["result"]["server_info"]["name"], "mish");

        self.notify(
            r#"{"jsonrpc":"2.0","id":null,"method":"notifications/initialized"}"#,
        );
    }

    /// Close stdin (triggers EOF → graceful shutdown) and wait for exit.
    fn shutdown(mut self) -> std::process::ExitStatus {
        self.stdin.take(); // Drop stdin → EOF
        self.child
            .wait_timeout(Duration::from_secs(10))
            .expect("wait for child")
            .expect("child should exit within 10s")
    }
}

impl Drop for MishServer {
    fn drop(&mut self) {
        // Best-effort kill if the test didn't call shutdown().
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Trait extension for wait_timeout on Child (not in stable std).
trait WaitTimeout {
    fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> std::io::Result<Option<std::process::ExitStatus>>;
}

impl WaitTimeout for std::process::Child {
    fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> std::io::Result<Option<std::process::ExitStatus>> {
        let start = std::time::Instant::now();
        loop {
            match self.try_wait()? {
                Some(status) => return Ok(Some(status)),
                None if start.elapsed() > timeout => return Ok(None),
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }
}

// =========================================================================
// 1. sh_session create/list/close lifecycle
// =========================================================================

#[test]
#[serial(pty)]
fn test_01_session_list_shows_main() {
    let mut server = MishServer::start();
    server.init();

    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"list"}}}"#,
    );

    assert!(resp["error"].is_null(), "Expected success, got: {resp}");
    let sessions = &resp["result"]["result"]["sessions"];
    assert!(sessions.is_array());
    // run_server() creates a "main" session on startup.
    let names: Vec<&str> = sessions
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"main"),
        "Expected 'main' session in {names:?}"
    );

    server.shutdown();
}

#[test]
#[serial(pty)]
fn test_02_session_create_list_close_lifecycle() {
    let mut server = MishServer::start();
    server.init();

    // Create a new session
    let create_resp = server.request(
        r#"{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"create","name":"test-session","shell":"/bin/bash"}}}"#,
    );
    assert!(
        create_resp["error"].is_null(),
        "create failed: {create_resp}"
    );
    assert_eq!(create_resp["result"]["result"]["session"], "test-session");
    assert_eq!(create_resp["result"]["result"]["ready"], true);

    // List should show both main and test-session
    let list_resp = server.request(
        r#"{"jsonrpc":"2.0","id":21,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"list"}}}"#,
    );
    let sessions = &list_resp["result"]["result"]["sessions"];
    let names: Vec<&str> = sessions
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"main"), "missing 'main' in {names:?}");
    assert!(
        names.contains(&"test-session"),
        "missing 'test-session' in {names:?}"
    );

    // Close the session
    let close_resp = server.request(
        r#"{"jsonrpc":"2.0","id":22,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"close","name":"test-session"}}}"#,
    );
    assert!(
        close_resp["error"].is_null(),
        "close failed: {close_resp}"
    );
    assert_eq!(close_resp["result"]["result"]["closed"], true);

    // List should no longer contain test-session
    let list2_resp = server.request(
        r#"{"jsonrpc":"2.0","id":23,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"list"}}}"#,
    );
    let sessions2 = &list2_resp["result"]["result"]["sessions"];
    let names2: Vec<&str> = sessions2
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session"].as_str().unwrap())
        .collect();
    assert!(
        !names2.contains(&"test-session"),
        "test-session should be gone, got: {names2:?}"
    );

    server.shutdown();
}

// =========================================================================
// 2. sh_run with real commands through category router
// =========================================================================

#[test]
#[serial(pty)]
fn test_03_sh_run_echo() {
    let mut server = MishServer::start();
    server.init();

    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":30,"method":"tools/call","params":{"name":"sh_run","arguments":{"cmd":"echo integration_test_output","timeout":10}}}"#,
    );

    assert!(resp["error"].is_null(), "sh_run failed: {resp}");
    let result = &resp["result"]["result"];
    assert_eq!(result["exit_code"], 0);
    assert!(
        result["output"]
            .as_str()
            .unwrap()
            .contains("integration_test_output"),
        "output should contain echo text, got: {}",
        result["output"]
    );

    server.shutdown();
}

#[test]
#[serial(pty)]
fn test_04_sh_run_response_structure() {
    let mut server = MishServer::start();
    server.init();

    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":40,"method":"tools/call","params":{"name":"sh_run","arguments":{"cmd":"echo structure_check","timeout":10}}}"#,
    );

    assert!(resp["error"].is_null(), "sh_run failed: {resp}");
    let result = &resp["result"]["result"];
    // Verify all expected fields exist
    assert!(result["exit_code"].is_number(), "exit_code should be number");
    assert!(result["duration_ms"].is_number(), "duration_ms should be number");
    assert!(result["cwd"].is_string(), "cwd should be string");
    assert!(result["category"].is_string(), "category should be string");
    assert!(result["output"].is_string(), "output should be string");
    assert!(result["lines"].is_object(), "lines should be object");
    assert!(result["lines"]["total"].is_number(), "lines.total should be number");
    assert!(result["lines"]["shown"].is_number(), "lines.shown should be number");

    server.shutdown();
}

#[test]
#[serial(pty)]
fn test_05_sh_run_category_in_response() {
    let mut server = MishServer::start();
    server.init();

    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":50,"method":"tools/call","params":{"name":"sh_run","arguments":{"cmd":"echo hello","timeout":10}}}"#,
    );

    assert!(resp["error"].is_null(), "sh_run failed: {resp}");
    let result = &resp["result"]["result"];
    // category should be present as a string (exact value depends on grammar config)
    assert!(
        result["category"].is_string(),
        "category should be a string, got: {}",
        result["category"]
    );

    server.shutdown();
}

#[test]
#[serial(pty)]
fn test_06_sh_run_line_counts() {
    let mut server = MishServer::start();
    server.init();

    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":60,"method":"tools/call","params":{"name":"sh_run","arguments":{"cmd":"echo line1; echo line2; echo line3","timeout":10}}}"#,
    );

    assert!(resp["error"].is_null(), "sh_run failed: {resp}");
    let result = &resp["result"]["result"];
    assert!(result["lines"]["total"].is_number());
    assert!(result["lines"]["shown"].is_number());
    assert!(result["lines"]["total"].as_u64().unwrap() >= 1);

    server.shutdown();
}

// =========================================================================
// 3. Process table digest presence on every response
// =========================================================================

#[test]
#[serial(pty)]
fn test_07_digest_on_tool_success() {
    let mut server = MishServer::start();
    server.init();

    // sh_help response should have processes array
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":70,"method":"tools/call","params":{"name":"sh_help","arguments":{}}}"#,
    );
    assert!(resp["error"].is_null(), "sh_help failed: {resp}");
    assert!(
        resp["result"]["processes"].is_array(),
        "processes digest missing from sh_help response: {resp}"
    );

    server.shutdown();
}

#[test]
#[serial(pty)]
fn test_08_digest_on_sh_run() {
    let mut server = MishServer::start();
    server.init();

    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":80,"method":"tools/call","params":{"name":"sh_run","arguments":{"cmd":"echo digest_check","timeout":5}}}"#,
    );
    assert!(resp["error"].is_null(), "sh_run failed: {resp}");
    assert!(
        resp["result"]["processes"].is_array(),
        "processes digest missing from sh_run response: {resp}"
    );

    server.shutdown();
}

#[test]
#[serial(pty)]
fn test_09_digest_on_error_response() {
    let mut server = MishServer::start();
    server.init();

    // Unknown tool should return error WITH process digest
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":90,"method":"tools/call","params":{"name":"nonexistent_tool","arguments":{}}}"#,
    );
    assert!(resp["error"].is_object(), "Expected error: {resp}");
    assert!(
        resp["error"]["data"]["processes"].is_array(),
        "processes digest missing from error response: {resp}"
    );

    server.shutdown();
}

#[test]
#[serial(pty)]
fn test_10_digest_on_session_operations() {
    let mut server = MishServer::start();
    server.init();

    // Create a session — response should have digest
    let create_resp = server.request(
        r#"{"jsonrpc":"2.0","id":100,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"create","name":"digest-test","shell":"/bin/bash"}}}"#,
    );
    assert!(create_resp["error"].is_null(), "create failed: {create_resp}");
    assert!(
        create_resp["result"]["processes"].is_array(),
        "processes digest missing from session create: {create_resp}"
    );

    // List sessions — response should have digest
    let list_resp = server.request(
        r#"{"jsonrpc":"2.0","id":101,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"list"}}}"#,
    );
    assert!(list_resp["error"].is_null(), "list failed: {list_resp}");
    assert!(
        list_resp["result"]["processes"].is_array(),
        "processes digest missing from session list: {list_resp}"
    );

    // Close session — response should have digest
    let close_resp = server.request(
        r#"{"jsonrpc":"2.0","id":102,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"close","name":"digest-test"}}}"#,
    );
    assert!(close_resp["error"].is_null(), "close failed: {close_resp}");
    assert!(
        close_resp["result"]["processes"].is_array(),
        "processes digest missing from session close: {close_resp}"
    );

    server.shutdown();
}

// =========================================================================
// 4. Error codes for invalid requests
// =========================================================================

#[test]
#[serial(pty)]
fn test_11_unknown_tool_error_code() {
    let mut server = MishServer::start();
    server.init();

    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":110,"method":"tools/call","params":{"name":"bogus_tool","arguments":{}}}"#,
    );
    assert_eq!(
        resp["error"]["code"], -32601,
        "unknown tool should return -32601 (method not found): {resp}"
    );

    server.shutdown();
}

#[test]
#[serial(pty)]
fn test_12_invalid_params_error_code() {
    let mut server = MishServer::start();
    server.init();

    // sh_run without required 'cmd' field
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":120,"method":"tools/call","params":{"name":"sh_run","arguments":{"not_a_field":true}}}"#,
    );
    assert_eq!(
        resp["error"]["code"], -32602,
        "invalid params should return -32602: {resp}"
    );

    server.shutdown();
}

#[test]
#[serial(pty)]
fn test_13_missing_params_error_code() {
    let mut server = MishServer::start();
    server.init();

    // tools/call without params.name
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":130,"method":"tools/call","params":{"arguments":{}}}"#,
    );
    assert_eq!(
        resp["error"]["code"], -32602,
        "missing 'name' should return -32602: {resp}"
    );

    server.shutdown();
}

#[test]
#[serial(pty)]
fn test_14_unknown_method_error_code() {
    let mut server = MishServer::start();
    server.init();

    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":140,"method":"bogus/method"}"#,
    );
    assert_eq!(
        resp["error"]["code"], -32601,
        "unknown method should return -32601: {resp}"
    );

    server.shutdown();
}

#[test]
#[serial(pty)]
fn test_15_tools_call_before_init_error() {
    let mut server = MishServer::start();

    // Send initialize but NOT notifications/initialized
    server.request(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"2024-11-05","capabilities":{},"client_info":{"name":"test"}}}"#,
    );

    // Try tools/call without the initialized notification
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":150,"method":"tools/call","params":{"name":"sh_help","arguments":{}}}"#,
    );
    assert_eq!(
        resp["error"]["code"], -32600,
        "tools/call before init should return -32600: {resp}"
    );

    server.shutdown();
}

#[test]
#[serial(pty)]
fn test_16_invalid_json_parse_error() {
    let mut server = MishServer::start();

    // Send invalid JSON — transport should respond with parse error
    let resp = server.request("this is not json at all");
    assert_eq!(
        resp["error"]["code"], -32700,
        "invalid JSON should return -32700 (parse error): {resp}"
    );

    server.shutdown();
}

#[test]
#[serial(pty)]
fn test_17_close_nonexistent_session_error() {
    let mut server = MishServer::start();
    server.init();

    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":170,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"close","name":"ghost"}}}"#,
    );
    assert!(resp["error"].is_object(), "Expected error: {resp}");
    assert_eq!(
        resp["error"]["code"], -32002,
        "close nonexistent should return -32002 (session not found): {resp}"
    );

    server.shutdown();
}

#[test]
#[serial(pty)]
fn test_18_interact_nonexistent_alias_error() {
    let mut server = MishServer::start();
    server.init();

    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":180,"method":"tools/call","params":{"name":"sh_interact","arguments":{"alias":"ghost","action":"status"}}}"#,
    );
    assert!(resp["error"].is_object(), "Expected error: {resp}");
    assert_eq!(
        resp["error"]["code"], -32003,
        "interact nonexistent alias should return -32003: {resp}"
    );

    server.shutdown();
}

#[test]
#[serial(pty)]
fn test_19_denied_command_error() {
    let mut server = MishServer::start();
    server.init();

    // rm -rf / should be denied by safety deny list
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":190,"method":"tools/call","params":{"name":"sh_run","arguments":{"cmd":"rm -rf /","timeout":5}}}"#,
    );
    assert!(
        resp["error"].is_object(),
        "dangerous command should be denied: {resp}"
    );
    assert_eq!(
        resp["error"]["code"], -32005,
        "denied command should return -32005 (command blocked): {resp}"
    );

    server.shutdown();
}

// =========================================================================
// 5. Graceful shutdown on EOF
// =========================================================================

#[test]
#[serial(pty)]
fn test_20_graceful_shutdown_on_eof() {
    let mut server = MishServer::start();
    server.init();

    // Run a quick command to prove server is functional
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":200,"method":"tools/call","params":{"name":"sh_help","arguments":{}}}"#,
    );
    assert!(resp["error"].is_null(), "sh_help should work: {resp}");

    // Close stdin → EOF → server should shut down cleanly
    let status = server.shutdown();
    assert!(
        status.success(),
        "mish serve should exit cleanly on EOF, got: {status}"
    );
}

#[test]
#[serial(pty)]
fn test_21_eof_without_any_requests() {
    let server = MishServer::start();

    // Immediately close stdin — server should still exit cleanly
    let status = server.shutdown();
    assert!(
        status.success(),
        "mish serve should exit cleanly on immediate EOF, got: {status}"
    );
}

// =========================================================================
// 6. Full MCP lifecycle (multi-step scenario)
// =========================================================================

#[test]
#[serial(pty)]
fn test_22_full_lifecycle() {
    let mut server = MishServer::start();
    server.init();

    // 1. tools/list — verify 5 tools
    let list_resp = server.request(
        r#"{"jsonrpc":"2.0","id":220,"method":"tools/list"}"#,
    );
    let tools = list_resp["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 5, "Expected 5 tools, got: {}", tools.len());
    let tool_names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(tool_names.contains(&"sh_run"));
    assert!(tool_names.contains(&"sh_spawn"));
    assert!(tool_names.contains(&"sh_interact"));
    assert!(tool_names.contains(&"sh_session"));
    assert!(tool_names.contains(&"sh_help"));

    // 2. sh_run — execute a real command
    let run_resp = server.request(
        r#"{"jsonrpc":"2.0","id":221,"method":"tools/call","params":{"name":"sh_run","arguments":{"cmd":"echo lifecycle_test","timeout":10}}}"#,
    );
    assert!(run_resp["error"].is_null());
    assert_eq!(run_resp["result"]["result"]["exit_code"], 0);
    assert!(
        run_resp["result"]["result"]["output"]
            .as_str()
            .unwrap()
            .contains("lifecycle_test")
    );

    // 3. sh_session — create, use, close a session
    let create_resp = server.request(
        r#"{"jsonrpc":"2.0","id":222,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"create","name":"lifecycle-sess","shell":"/bin/bash"}}}"#,
    );
    assert!(create_resp["error"].is_null());
    assert_eq!(create_resp["result"]["result"]["session"], "lifecycle-sess");

    let close_resp = server.request(
        r#"{"jsonrpc":"2.0","id":223,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"close","name":"lifecycle-sess"}}}"#,
    );
    assert!(close_resp["error"].is_null());
    assert_eq!(close_resp["result"]["result"]["closed"], true);

    // 4. sh_help — reference card with digest
    let help_resp = server.request(
        r#"{"jsonrpc":"2.0","id":224,"method":"tools/call","params":{"name":"sh_help","arguments":{}}}"#,
    );
    assert!(help_resp["error"].is_null());
    assert!(help_resp["result"]["result"]["tools"].is_array());
    assert!(help_resp["result"]["processes"].is_array());

    // 5. Graceful shutdown
    let status = server.shutdown();
    assert!(status.success());
}

// =========================================================================
// 7. Initialize response verification
// =========================================================================

#[test]
#[serial(pty)]
fn test_23_initialize_response_fields() {
    let mut server = MishServer::start();

    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":230,"method":"initialize","params":{"protocol_version":"2024-11-05","capabilities":{},"client_info":{"name":"test-client","version":"1.0"}}}"#,
    );

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 230);
    assert!(resp["error"].is_null());

    let result = &resp["result"];
    assert_eq!(result["protocol_version"], "2024-11-05");
    assert_eq!(result["server_info"]["name"], "mish");
    assert!(result["server_info"]["version"].is_string());
    assert_eq!(result["capabilities"]["tools"]["list_changed"], false);

    server.shutdown();
}

#[test]
#[serial(pty)]
fn test_24_request_id_preserved() {
    let mut server = MishServer::start();
    server.init();

    // String id
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":"abc-123","method":"tools/list"}"#,
    );
    assert_eq!(resp["id"], "abc-123");

    // Numeric id
    let resp2 = server.request(
        r#"{"jsonrpc":"2.0","id":99999,"method":"tools/list"}"#,
    );
    assert_eq!(resp2["id"], 99999);

    server.shutdown();
}

// =========================================================================
// 8. sh_run on custom session
// =========================================================================

#[test]
#[serial(pty)]
fn test_25_sh_run_on_custom_session() {
    let mut server = MishServer::start();
    server.init();

    // Create a session
    let create_resp = server.request(
        r#"{"jsonrpc":"2.0","id":250,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"create","name":"custom-run","shell":"/bin/bash"}}}"#,
    );
    assert!(create_resp["error"].is_null(), "create failed: {create_resp}");

    // Run a command on the custom session
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":251,"method":"tools/call","params":{"name":"sh_run","arguments":{"cmd":"echo custom_session_output","session":"custom-run","timeout":10}}}"#,
    );
    assert!(resp["error"].is_null(), "sh_run on custom session failed: {resp}");
    assert_eq!(resp["result"]["result"]["exit_code"], 0);
    assert!(
        resp["result"]["result"]["output"]
            .as_str()
            .unwrap()
            .contains("custom_session_output"),
        "output should contain echo text: {}",
        resp["result"]["result"]["output"]
    );

    // Clean up
    server.request(
        r#"{"jsonrpc":"2.0","id":252,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"close","name":"custom-run"}}}"#,
    );

    server.shutdown();
}

// =========================================================================
// 9. tools/list response verification
// =========================================================================

#[test]
#[serial(pty)]
fn test_26_tools_list_response_structure() {
    let mut server = MishServer::start();
    server.init();

    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":260,"method":"tools/list"}"#,
    );
    assert!(resp["error"].is_null(), "tools/list failed: {resp}");

    let tools = resp["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 5);

    // Each tool should have name, description, and inputSchema
    for tool in tools {
        assert!(tool["name"].is_string(), "tool missing name: {tool}");
        assert!(tool["description"].is_string(), "tool missing description: {tool}");
        assert!(tool["inputSchema"].is_object(), "tool missing inputSchema: {tool}");
    }

    server.shutdown();
}

// =========================================================================
// 10. Multiple commands in sequence
// =========================================================================

#[test]
#[serial(pty)]
fn test_27_multiple_sh_run_commands() {
    let mut server = MishServer::start();
    server.init();

    // Run several commands in sequence to verify server handles repeated requests
    for i in 0..5 {
        let id = 270 + i;
        let cmd = format!(
            r#"{{"jsonrpc":"2.0","id":{},"method":"tools/call","params":{{"name":"sh_run","arguments":{{"cmd":"echo seq_{}","timeout":5}}}}}}"#,
            id, i
        );
        let resp = server.request(&cmd);
        assert!(resp["error"].is_null(), "sh_run #{i} failed: {resp}");
        assert_eq!(resp["id"], id);
        assert_eq!(resp["result"]["result"]["exit_code"], 0);
    }

    server.shutdown();
}

// =========================================================================
// 11. sh_spawn + sh_interact background process lifecycle
// =========================================================================

#[test]
#[serial(pty)]
fn test_28_sh_spawn_basic() {
    let mut server = MishServer::start();
    server.init();

    // Spawn a background process
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":280,"method":"tools/call","params":{"name":"sh_spawn","arguments":{"alias":"bgtest","cmd":"sleep 60","timeout":5}}}"#,
    );
    assert!(resp["error"].is_null(), "sh_spawn failed: {resp}");
    let result = &resp["result"]["result"];
    assert_eq!(result["alias"], "bgtest");
    assert_eq!(result["state"], "running");
    assert!(result["pid"].is_number(), "pid should be a number: {result}");

    // Clean up
    server.request(
        r#"{"jsonrpc":"2.0","id":281,"method":"tools/call","params":{"name":"sh_interact","arguments":{"alias":"bgtest","action":"kill"}}}"#,
    );

    server.shutdown();
}

#[test]
#[serial(pty)]
fn test_29_sh_spawn_then_interact_status_and_kill() {
    let mut server = MishServer::start();
    server.init();

    // Spawn
    let spawn_resp = server.request(
        r#"{"jsonrpc":"2.0","id":290,"method":"tools/call","params":{"name":"sh_spawn","arguments":{"alias":"interact_test","cmd":"sleep 60","timeout":5}}}"#,
    );
    assert!(spawn_resp["error"].is_null(), "sh_spawn failed: {spawn_resp}");

    // Status check
    let status_resp = server.request(
        r#"{"jsonrpc":"2.0","id":291,"method":"tools/call","params":{"name":"sh_interact","arguments":{"alias":"interact_test","action":"status"}}}"#,
    );
    assert!(status_resp["error"].is_null(), "status failed: {status_resp}");
    assert_eq!(status_resp["result"]["result"]["alias"], "interact_test");
    assert_eq!(status_resp["result"]["result"]["action"], "status");

    // Kill
    let kill_resp = server.request(
        r#"{"jsonrpc":"2.0","id":292,"method":"tools/call","params":{"name":"sh_interact","arguments":{"alias":"interact_test","action":"kill"}}}"#,
    );
    assert!(kill_resp["error"].is_null(), "kill failed: {kill_resp}");
    assert_eq!(kill_resp["result"]["result"]["action"], "kill");

    server.shutdown();
}

#[test]
#[serial(pty)]
fn test_30_sh_spawn_appears_in_digest() {
    let mut server = MishServer::start();
    server.init();

    // Spawn a background process
    let spawn_resp = server.request(
        r#"{"jsonrpc":"2.0","id":300,"method":"tools/call","params":{"name":"sh_spawn","arguments":{"alias":"digest_bg","cmd":"sleep 60","timeout":5}}}"#,
    );
    assert!(spawn_resp["error"].is_null(), "sh_spawn failed: {spawn_resp}");

    // Call sh_help — its digest should include the spawned process
    let help_resp = server.request(
        r#"{"jsonrpc":"2.0","id":301,"method":"tools/call","params":{"name":"sh_help","arguments":{}}}"#,
    );
    let processes = &help_resp["result"]["processes"];
    assert!(processes.is_array(), "processes digest missing: {help_resp}");
    let aliases: Vec<&str> = processes
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["alias"].as_str())
        .collect();
    assert!(
        aliases.contains(&"digest_bg"),
        "digest should include 'digest_bg', got: {aliases:?}"
    );

    // Clean up
    server.request(
        r#"{"jsonrpc":"2.0","id":302,"method":"tools/call","params":{"name":"sh_interact","arguments":{"alias":"digest_bg","action":"kill"}}}"#,
    );

    server.shutdown();
}
