//! End-to-end integration tests for the mish MCP server (`mish serve`).
//!
//! Tests the compiled binary by spawning `mish serve` as a subprocess,
//! sending JSON-RPC requests via stdin, and verifying responses on stdout.
//!
//! Battery tests share a single server instance to reduce spawn overhead.
//! Protocol-only tests (no tools/call → no PTY) skip #[serial(pty)].

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
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"integration-test"}}}"#,
        );
        assert_eq!(resp["result"]["serverInfo"]["name"], "mish");

        self.notify(
            r#"{"jsonrpc":"2.0","id":null,"method":"notifications/initialized"}"#,
        );
    }

    /// Extract the tool payload from a content-wrapped MCP tools/call response.
    /// Response format: result.content[0].text = JSON string with {result, processes}
    fn extract_tool_payload(resp: &serde_json::Value) -> serde_json::Value {
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("tools/call response should have result.content[0].text");
        serde_json::from_str(text).expect("content text should be valid JSON")
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
// Battery 1: sh_run commands (tests 3,4,5,6,7,8,27)
// =========================================================================

#[test]
#[serial(pty)]
fn test_sh_run_battery() {
    let mut server = MishServer::start();
    server.init();

    // -- echo output (test_03) --
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":30,"method":"tools/call","params":{"name":"sh_run","arguments":{"cmd":"echo integration_test_output","timeout":10}}}"#,
    );
    assert!(resp["error"].is_null(), "sh_run echo failed: {resp}");
    let payload = MishServer::extract_tool_payload(&resp);
    assert_eq!(payload["result"]["exit_code"], 0);
    assert!(
        payload["result"]["output"]
            .as_str()
            .unwrap()
            .contains("integration_test_output"),
        "output should contain echo text, got: {}",
        payload["result"]["output"]
    );

    // -- response structure (test_04) --
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":40,"method":"tools/call","params":{"name":"sh_run","arguments":{"cmd":"echo structure_check","timeout":10}}}"#,
    );
    assert!(resp["error"].is_null(), "sh_run structure failed: {resp}");
    let payload = MishServer::extract_tool_payload(&resp);
    let result = &payload["result"];
    assert!(result["exit_code"].is_number(), "exit_code should be number");
    assert!(result["duration_ms"].is_number(), "duration_ms should be number");
    assert!(result["cwd"].is_string(), "cwd should be string");
    assert!(result["category"].is_string(), "category should be string");
    assert!(result["output"].is_string(), "output should be string");
    assert!(result["lines"].is_object(), "lines should be object");
    assert!(result["lines"]["total"].is_number(), "lines.total should be number");
    assert!(result["lines"]["shown"].is_number(), "lines.shown should be number");

    // -- category present (test_05) --
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":50,"method":"tools/call","params":{"name":"sh_run","arguments":{"cmd":"echo hello","timeout":10}}}"#,
    );
    assert!(resp["error"].is_null(), "sh_run category failed: {resp}");
    let payload = MishServer::extract_tool_payload(&resp);
    assert!(
        payload["result"]["category"].is_string(),
        "category should be a string, got: {}",
        payload["result"]["category"]
    );

    // -- line counts (test_06) --
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":60,"method":"tools/call","params":{"name":"sh_run","arguments":{"cmd":"echo line1; echo line2; echo line3","timeout":10}}}"#,
    );
    assert!(resp["error"].is_null(), "sh_run lines failed: {resp}");
    let payload = MishServer::extract_tool_payload(&resp);
    assert!(payload["result"]["lines"]["total"].is_number());
    assert!(payload["result"]["lines"]["shown"].is_number());
    assert!(payload["result"]["lines"]["total"].as_u64().unwrap() >= 1);

    // -- digest on sh_help (test_07) --
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":70,"method":"tools/call","params":{"name":"sh_help","arguments":{}}}"#,
    );
    assert!(resp["error"].is_null(), "sh_help failed: {resp}");
    let payload = MishServer::extract_tool_payload(&resp);
    assert!(
        payload["processes"].is_array(),
        "processes digest missing from sh_help response: {resp}"
    );

    // -- digest on sh_run (test_08) --
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":80,"method":"tools/call","params":{"name":"sh_run","arguments":{"cmd":"echo digest_check","timeout":5}}}"#,
    );
    assert!(resp["error"].is_null(), "sh_run digest failed: {resp}");
    let payload = MishServer::extract_tool_payload(&resp);
    assert!(
        payload["processes"].is_array(),
        "processes digest missing from sh_run response: {resp}"
    );

    // -- multiple sequential commands (test_27) --
    for i in 0..5 {
        let id = 270 + i;
        let cmd = format!(
            r#"{{"jsonrpc":"2.0","id":{},"method":"tools/call","params":{{"name":"sh_run","arguments":{{"cmd":"echo seq_{}","timeout":5}}}}}}"#,
            id, i
        );
        let resp = server.request(&cmd);
        assert!(resp["error"].is_null(), "sh_run #{i} failed: {resp}");
        assert_eq!(resp["id"], id);
        let payload = MishServer::extract_tool_payload(&resp);
        assert_eq!(payload["result"]["exit_code"], 0);
    }

    server.shutdown();
}

// =========================================================================
// Battery 2: error codes (tests 9,11,12,13,14,17,18,19)
// =========================================================================

#[test]
#[serial(pty)]
fn test_error_codes_battery() {
    let mut server = MishServer::start();
    server.init();

    // -- digest on error response (test_09) --
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":90,"method":"tools/call","params":{"name":"nonexistent_tool","arguments":{}}}"#,
    );
    assert!(resp["error"].is_object(), "Expected error: {resp}");
    assert!(
        resp["error"]["data"]["processes"].is_array(),
        "processes digest missing from error response: {resp}"
    );

    // -- unknown tool error code (test_11) --
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":110,"method":"tools/call","params":{"name":"bogus_tool","arguments":{}}}"#,
    );
    assert_eq!(
        resp["error"]["code"], -32601,
        "unknown tool should return -32601 (method not found): {resp}"
    );

    // -- invalid params error code (test_12) --
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":120,"method":"tools/call","params":{"name":"sh_run","arguments":{"not_a_field":true}}}"#,
    );
    assert_eq!(
        resp["error"]["code"], -32602,
        "invalid params should return -32602: {resp}"
    );

    // -- missing params error code (test_13) --
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":130,"method":"tools/call","params":{"arguments":{}}}"#,
    );
    assert_eq!(
        resp["error"]["code"], -32602,
        "missing 'name' should return -32602: {resp}"
    );

    // -- unknown method error code (test_14) --
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":140,"method":"bogus/method"}"#,
    );
    assert_eq!(
        resp["error"]["code"], -32601,
        "unknown method should return -32601: {resp}"
    );

    // -- close nonexistent session (test_17) --
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":170,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"close","name":"ghost"}}}"#,
    );
    assert!(resp["error"].is_object(), "Expected error: {resp}");
    assert_eq!(
        resp["error"]["code"], -32002,
        "close nonexistent should return -32002 (session not found): {resp}"
    );

    // -- interact nonexistent alias (test_18) --
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":180,"method":"tools/call","params":{"name":"sh_interact","arguments":{"alias":"ghost","action":"status"}}}"#,
    );
    assert!(resp["error"].is_object(), "Expected error: {resp}");
    assert_eq!(
        resp["error"]["code"], -32003,
        "interact nonexistent alias should return -32003: {resp}"
    );

    // -- denied command (test_19) --
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
// Battery 3: session lifecycle (tests 1,2,10,25)
// =========================================================================

#[test]
#[serial(pty)]
fn test_session_battery() {
    let mut server = MishServer::start();
    server.init();

    // -- session list shows main (test_01) --
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"list"}}}"#,
    );
    assert!(resp["error"].is_null(), "Expected success, got: {resp}");
    let payload = MishServer::extract_tool_payload(&resp);
    let sessions = &payload["result"]["sessions"];
    assert!(sessions.is_array());
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

    // -- create/list/close lifecycle (test_02) --
    let create_resp = server.request(
        r#"{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"create","name":"test-session","shell":"/bin/bash"}}}"#,
    );
    assert!(
        create_resp["error"].is_null(),
        "create failed: {create_resp}"
    );
    let create_payload = MishServer::extract_tool_payload(&create_resp);
    assert_eq!(create_payload["result"]["session"], "test-session");
    assert_eq!(create_payload["result"]["ready"], true);

    let list_resp = server.request(
        r#"{"jsonrpc":"2.0","id":21,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"list"}}}"#,
    );
    let list_payload = MishServer::extract_tool_payload(&list_resp);
    let sessions = &list_payload["result"]["sessions"];
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

    let close_resp = server.request(
        r#"{"jsonrpc":"2.0","id":22,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"close","name":"test-session"}}}"#,
    );
    assert!(
        close_resp["error"].is_null(),
        "close failed: {close_resp}"
    );
    let close_payload = MishServer::extract_tool_payload(&close_resp);
    assert_eq!(close_payload["result"]["closed"], true);

    let list2_resp = server.request(
        r#"{"jsonrpc":"2.0","id":23,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"list"}}}"#,
    );
    let list2_payload = MishServer::extract_tool_payload(&list2_resp);
    let sessions2 = &list2_payload["result"]["sessions"];
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

    // -- digest on session operations (test_10) --
    let create_resp = server.request(
        r#"{"jsonrpc":"2.0","id":100,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"create","name":"digest-test","shell":"/bin/bash"}}}"#,
    );
    assert!(create_resp["error"].is_null(), "create failed: {create_resp}");
    let create_payload = MishServer::extract_tool_payload(&create_resp);
    assert!(
        create_payload["processes"].is_array(),
        "processes digest missing from session create: {create_resp}"
    );

    let list_resp = server.request(
        r#"{"jsonrpc":"2.0","id":101,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"list"}}}"#,
    );
    assert!(list_resp["error"].is_null(), "list failed: {list_resp}");
    let list_payload = MishServer::extract_tool_payload(&list_resp);
    assert!(
        list_payload["processes"].is_array(),
        "processes digest missing from session list: {list_resp}"
    );

    let close_resp = server.request(
        r#"{"jsonrpc":"2.0","id":102,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"close","name":"digest-test"}}}"#,
    );
    assert!(close_resp["error"].is_null(), "close failed: {close_resp}");
    let close_payload = MishServer::extract_tool_payload(&close_resp);
    assert!(
        close_payload["processes"].is_array(),
        "processes digest missing from session close: {close_resp}"
    );

    // -- sh_run on custom session (test_25) --
    let create_resp = server.request(
        r#"{"jsonrpc":"2.0","id":250,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"create","name":"custom-run","shell":"/bin/bash"}}}"#,
    );
    assert!(create_resp["error"].is_null(), "create failed: {create_resp}");

    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":251,"method":"tools/call","params":{"name":"sh_run","arguments":{"cmd":"echo custom_session_output","session":"custom-run","timeout":10}}}"#,
    );
    assert!(resp["error"].is_null(), "sh_run on custom session failed: {resp}");
    let payload = MishServer::extract_tool_payload(&resp);
    assert_eq!(payload["result"]["exit_code"], 0);
    assert!(
        payload["result"]["output"]
            .as_str()
            .unwrap()
            .contains("custom_session_output"),
        "output should contain echo text: {}",
        payload["result"]["output"]
    );

    server.request(
        r#"{"jsonrpc":"2.0","id":252,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"close","name":"custom-run"}}}"#,
    );

    server.shutdown();
}

// =========================================================================
// Battery 4: background processes (tests 28,29,30)
// =========================================================================

#[test]
#[serial(pty)]
fn test_background_battery() {
    let mut server = MishServer::start();
    server.init();

    // -- sh_spawn basic (test_28) --
    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":280,"method":"tools/call","params":{"name":"sh_spawn","arguments":{"alias":"bgtest","cmd":"sleep 60","timeout":5}}}"#,
    );
    assert!(resp["error"].is_null(), "sh_spawn failed: {resp}");
    let payload = MishServer::extract_tool_payload(&resp);
    let result = &payload["result"];
    assert_eq!(result["alias"], "bgtest");
    assert_eq!(result["state"], "running");
    assert!(result["pid"].is_number(), "pid should be a number: {result}");

    // Kill bgtest before next spawn
    server.request(
        r#"{"jsonrpc":"2.0","id":281,"method":"tools/call","params":{"name":"sh_interact","arguments":{"alias":"bgtest","action":"kill"}}}"#,
    );

    // -- spawn + interact status + kill (test_29) --
    let spawn_resp = server.request(
        r#"{"jsonrpc":"2.0","id":290,"method":"tools/call","params":{"name":"sh_spawn","arguments":{"alias":"interact_test","cmd":"sleep 60","timeout":5}}}"#,
    );
    assert!(spawn_resp["error"].is_null(), "sh_spawn failed: {spawn_resp}");

    let status_resp = server.request(
        r#"{"jsonrpc":"2.0","id":291,"method":"tools/call","params":{"name":"sh_interact","arguments":{"alias":"interact_test","action":"status"}}}"#,
    );
    assert!(status_resp["error"].is_null(), "status failed: {status_resp}");
    let status_payload = MishServer::extract_tool_payload(&status_resp);
    assert_eq!(status_payload["result"]["alias"], "interact_test");
    assert_eq!(status_payload["result"]["action"], "status");

    let kill_resp = server.request(
        r#"{"jsonrpc":"2.0","id":292,"method":"tools/call","params":{"name":"sh_interact","arguments":{"alias":"interact_test","action":"kill"}}}"#,
    );
    assert!(kill_resp["error"].is_null(), "kill failed: {kill_resp}");
    let kill_payload = MishServer::extract_tool_payload(&kill_resp);
    assert_eq!(kill_payload["result"]["action"], "kill");

    // -- spawn appears in digest (test_30) --
    let spawn_resp = server.request(
        r#"{"jsonrpc":"2.0","id":300,"method":"tools/call","params":{"name":"sh_spawn","arguments":{"alias":"digest_bg","cmd":"sleep 60","timeout":5}}}"#,
    );
    assert!(spawn_resp["error"].is_null(), "sh_spawn failed: {spawn_resp}");

    let help_resp = server.request(
        r#"{"jsonrpc":"2.0","id":301,"method":"tools/call","params":{"name":"sh_help","arguments":{}}}"#,
    );
    let help_payload = MishServer::extract_tool_payload(&help_resp);
    let processes = &help_payload["processes"];
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

// =========================================================================
// Standalone serial tests
// =========================================================================

#[test]
#[serial(pty)]
fn test_full_lifecycle() {
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
    let run_payload = MishServer::extract_tool_payload(&run_resp);
    assert_eq!(run_payload["result"]["exit_code"], 0);
    assert!(
        run_payload["result"]["output"]
            .as_str()
            .unwrap()
            .contains("lifecycle_test")
    );

    // 3. sh_session — create, use, close a session
    let create_resp = server.request(
        r#"{"jsonrpc":"2.0","id":222,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"create","name":"lifecycle-sess","shell":"/bin/bash"}}}"#,
    );
    assert!(create_resp["error"].is_null());
    let create_payload = MishServer::extract_tool_payload(&create_resp);
    assert_eq!(create_payload["result"]["session"], "lifecycle-sess");

    let close_resp = server.request(
        r#"{"jsonrpc":"2.0","id":223,"method":"tools/call","params":{"name":"sh_session","arguments":{"action":"close","name":"lifecycle-sess"}}}"#,
    );
    assert!(close_resp["error"].is_null());
    let close_payload = MishServer::extract_tool_payload(&close_resp);
    assert_eq!(close_payload["result"]["closed"], true);

    // 4. sh_help — reference card with digest
    let help_resp = server.request(
        r#"{"jsonrpc":"2.0","id":224,"method":"tools/call","params":{"name":"sh_help","arguments":{}}}"#,
    );
    assert!(help_resp["error"].is_null());
    let help_payload = MishServer::extract_tool_payload(&help_resp);
    assert!(help_payload["result"]["tools"].is_array());
    assert!(help_payload["processes"].is_array());

    // 5. Graceful shutdown
    let status = server.shutdown();
    assert!(status.success());
}

#[test]
#[serial(pty)]
fn test_graceful_eof() {
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

// =========================================================================
// Protocol-only tests (no PTY spawned — no #[serial(pty)])
// =========================================================================

#[test]
fn test_eof_no_requests() {
    let server = MishServer::start();

    // Immediately close stdin — server should still exit cleanly
    let status = server.shutdown();
    assert!(
        status.success(),
        "mish serve should exit cleanly on immediate EOF, got: {status}"
    );
}

#[test]
fn test_init_response() {
    let mut server = MishServer::start();

    let resp = server.request(
        r#"{"jsonrpc":"2.0","id":230,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test-client","version":"1.0"}}}"#,
    );

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 230);
    assert!(resp["error"].is_null());

    let result = &resp["result"];
    assert_eq!(result["protocolVersion"], "2024-11-05");
    assert_eq!(result["serverInfo"]["name"], "mish");
    assert!(result["serverInfo"]["version"].is_string());
    assert_eq!(result["capabilities"]["tools"]["listChanged"], false);

    server.shutdown();
}

#[test]
fn test_request_ids() {
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

#[test]
fn test_tools_list_schema() {
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

#[test]
fn test_invalid_json() {
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
fn test_before_init_error() {
    let mut server = MishServer::start();

    // Send initialize but NOT notifications/initialized
    server.request(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#,
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
