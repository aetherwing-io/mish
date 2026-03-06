//! CLI sessions: persistent interpreter REPL via Unix socket.
//!
//! `mish session start py --cmd python3` spawns a detached host process with a
//! PTY-backed interpreter and a Unix socket. `mish session send py "import ast"`
//! connects, sends input, receives squashed output, and exits.
//!
//! Sentinel-based boundary detection: after writing user input, the host appends
//! a sentinel print command (e.g. `print("__MISH_<uuid>__")`). Output is captured
//! until the sentinel appears, then stripped and squashed.

use std::io::{BufRead, Write as IoWrite};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::interpreter::InterpreterSession;

// ---------------------------------------------------------------------------
// Protocol types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionRequest {
    Send {
        input: String,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },
    List,
    Close,
    Ping,
}

fn default_timeout() -> u64 {
    30
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SessionResponse {
    Ok {
        output: String,
        exit_code: i32,
        elapsed_ms: u64,
    },
    Sessions {
        entries: Vec<SessionListEntry>,
    },
    Error {
        message: String,
    },
    Pong {
        alias: String,
        pid: u32,
        uptime_secs: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListEntry {
    pub alias: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub cmd: String,
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

fn sessions_dir() -> PathBuf {
    dirs_session_base().join("sessions")
}

fn dirs_session_base() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("mish")
}

fn socket_path(alias: &str) -> PathBuf {
    sessions_dir().join(format!("{alias}.sock"))
}

fn pid_path(alias: &str) -> PathBuf {
    sessions_dir().join(format!("{alias}.pid"))
}

fn cmd_path(alias: &str) -> PathBuf {
    sessions_dir().join(format!("{alias}.cmd"))
}

/// Scan sessions dir for live `.sock` files.
fn find_sessions() -> Vec<(String, PathBuf)> {
    let dir = sessions_dir();
    if !dir.exists() {
        return Vec::new();
    }

    let mut results = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("sock") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    results.push((stem.to_string(), path));
                }
            }
        }
    }
    results
}

// InterpreterKind, InterpreterSession, ExecuteResult, strip_prompt,
// make_sentinel, squash_session_output are imported from crate::interpreter.

// ---------------------------------------------------------------------------
// SessionHost — Unix socket server
// ---------------------------------------------------------------------------

/// Default idle timeout: 30 minutes with no client activity.
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 30 * 60;

struct SessionHost {
    listener: UnixListener,
    session: InterpreterSession,
    alias: String,
    cmd: String,
    socket_path: PathBuf,
    idle_timeout: Duration,
}

impl SessionHost {
    fn bind(
        alias: String,
        cmd: String,
        session: InterpreterSession,
    ) -> Result<Self, String> {
        let dir = sessions_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("create sessions dir: {e}"))?;

        let sock = socket_path(&alias);

        // Validate path length
        if sock.to_string_lossy().len() > 100 {
            return Err(format!("socket path too long: {}", sock.display()));
        }

        // Remove stale socket
        if sock.exists() {
            let _ = std::fs::remove_file(&sock);
        }

        let listener =
            UnixListener::bind(&sock).map_err(|e| format!("bind socket: {e}"))?;

        // Set socket permissions to 0600
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
        }

        Ok(SessionHost {
            listener,
            session,
            alias,
            cmd,
            socket_path: sock,
            idle_timeout: Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS),
        })
    }

    fn serve(&self) -> Result<(), String> {
        self.listener
            .set_nonblocking(true)
            .map_err(|e| format!("set_nonblocking: {e}"))?;

        let mut last_activity = Instant::now();

        loop {
            // Check interpreter health
            if !self.session.is_alive() {
                eprintln!("mish session host: interpreter exited, shutting down");
                break;
            }

            // Check idle timeout
            if last_activity.elapsed() > self.idle_timeout {
                eprintln!(
                    "mish session host: idle timeout ({}s), shutting down",
                    self.idle_timeout.as_secs()
                );
                break;
            }

            match self.listener.accept() {
                Ok((stream, _)) => {
                    last_activity = Instant::now();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(30)))
                        .ok();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(30)))
                        .ok();

                    match self.handle_client(stream) {
                        HandleResult::Continue => {}
                        HandleResult::Shutdown => break,
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    eprintln!("mish session host: accept error: {e}");
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }

        self.cleanup();
        Ok(())
    }

    fn handle_client(&self, stream: UnixStream) -> HandleResult {
        let mut reader = std::io::BufReader::new(&stream);
        let mut line = String::new();

        if reader.read_line(&mut line).is_err() {
            return HandleResult::Continue;
        }

        let request: SessionRequest = match serde_json::from_str(line.trim()) {
            Ok(r) => r,
            Err(e) => {
                let resp = SessionResponse::Error {
                    message: format!("invalid request: {e}"),
                };
                let _ = write_response(&stream, &resp);
                return HandleResult::Continue;
            }
        };

        match request {
            SessionRequest::Send { input, timeout_secs } => {
                let timeout = Duration::from_secs(timeout_secs);
                let resp = match self.session.execute(&input, timeout) {
                    Ok(result) => SessionResponse::Ok {
                        output: result.output,
                        exit_code: result.exit_code,
                        elapsed_ms: result.elapsed_ms,
                    },
                    Err(msg) => SessionResponse::Error { message: msg },
                };
                let _ = write_response(&stream, &resp);
                HandleResult::Continue
            }
            SessionRequest::Ping => {
                let resp = SessionResponse::Pong {
                    alias: self.alias.clone(),
                    pid: std::process::id(),
                    uptime_secs: self.session.uptime_secs(),
                };
                let _ = write_response(&stream, &resp);
                HandleResult::Continue
            }
            SessionRequest::List => {
                // List is handled client-side by scanning sessions dir
                let resp = SessionResponse::Sessions {
                    entries: vec![SessionListEntry {
                        alias: self.alias.clone(),
                        pid: std::process::id(),
                        uptime_secs: self.session.uptime_secs(),
                        cmd: self.cmd.clone(),
                    }],
                };
                let _ = write_response(&stream, &resp);
                HandleResult::Continue
            }
            SessionRequest::Close => {
                let resp = SessionResponse::Ok {
                    output: "session closing".into(),
                    exit_code: 0,
                    elapsed_ms: 0,
                };
                let _ = write_response(&stream, &resp);
                HandleResult::Shutdown
            }
        }
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(pid_path(&self.alias));
        let _ = std::fs::remove_file(cmd_path(&self.alias));
    }
}

enum HandleResult {
    Continue,
    Shutdown,
}

fn write_response(stream: &UnixStream, resp: &SessionResponse) -> Result<(), String> {
    let json = serde_json::to_string(resp).map_err(|e| format!("serialize: {e}"))?;
    let mut writer = std::io::BufWriter::new(stream);
    writer
        .write_all(json.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    writer
        .write_all(b"\n")
        .map_err(|e| format!("write newline: {e}"))?;
    writer.flush().map_err(|e| format!("flush: {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Client helpers
// ---------------------------------------------------------------------------

fn send_request(alias: &str, request: &SessionRequest) -> Result<SessionResponse, String> {
    let sock = socket_path(alias);
    let stream = UnixStream::connect(&sock).map_err(|e| {
        // Clean stale socket on connection refused
        if e.kind() == std::io::ErrorKind::ConnectionRefused
            || e.kind() == std::io::ErrorKind::NotFound
        {
            let _ = std::fs::remove_file(&sock);
            let _ = std::fs::remove_file(pid_path(alias));
            let _ = std::fs::remove_file(cmd_path(alias));
        }
        format!("session \"{alias}\" not running (connect failed: {e})")
    })?;

    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .ok();

    let json = serde_json::to_string(request).map_err(|e| format!("serialize: {e}"))?;
    let mut writer = std::io::BufWriter::new(&stream);
    writer
        .write_all(json.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    writer
        .write_all(b"\n")
        .map_err(|e| format!("write newline: {e}"))?;
    writer.flush().map_err(|e| format!("flush: {e}"))?;

    let mut reader = std::io::BufReader::new(&stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read response: {e}"))?;

    if line.is_empty() {
        return Err("session host died (empty response)".into());
    }

    serde_json::from_str(line.trim()).map_err(|e| format!("parse response: {e}"))
}

// ---------------------------------------------------------------------------
// Public command entry points
// ---------------------------------------------------------------------------

/// `mish session host <alias> --cmd <cmd>` — hidden, called by re-exec detach.
pub fn cmd_session_host(alias: &str, cmd: &str) -> i32 {
    // Spawn interpreter
    let session = match InterpreterSession::spawn(cmd) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("! session host: {e}");
            return 1;
        }
    };

    // Bind socket + write PID/cmd files
    let host = match SessionHost::bind(alias.to_string(), cmd.to_string(), session) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("! session host: {e}");
            return 1;
        }
    };

    let pid_file = pid_path(alias);
    let cmd_file = cmd_path(alias);
    if let Err(e) = std::fs::write(&pid_file, std::process::id().to_string()) {
        eprintln!("! write pid file: {e}");
        return 1;
    }
    if let Err(e) = std::fs::write(&cmd_file, cmd) {
        eprintln!("! write cmd file: {e}");
        return 1;
    }

    // Install signal handlers
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let r = running.clone();
    let _ = ctrlc_handler(r);

    // Serve until shutdown
    if let Err(e) = host.serve() {
        eprintln!("! session host: {e}");
        return 1;
    }

    0
}

fn ctrlc_handler(running: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Result<(), String> {
    // We can't easily use ctrlc crate, just handle via signal
    // The host loop checks interpreter health and will exit when it dies
    // SIGTERM from the OS will kill us anyway
    let _ = running; // used for future graceful shutdown
    Ok(())
}

/// `mish session start <alias> --cmd <cmd> [--fg]`
pub fn cmd_session_start(alias: &str, cmd: &str, foreground: bool) -> i32 {
    // Check if session already running
    let sock = socket_path(alias);
    if sock.exists() {
        if let Ok(SessionResponse::Pong { pid, .. }) =
            send_request(alias, &SessionRequest::Ping)
        {
            println!("+ session \"{alias}\" already running (pid {pid})");
            return 0;
        }
        // Stale socket — clean up
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(pid_path(alias));
        let _ = std::fs::remove_file(cmd_path(alias));
    }

    if foreground {
        return cmd_session_host(alias, cmd);
    }

    // Re-exec as detached host
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("! cannot find mish binary: {e}");
            return 1;
        }
    };

    let child = std::process::Command::new(&exe)
        .args(["session", "host", alias, "--cmd", cmd])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    match child {
        Ok(_) => {}
        Err(e) => {
            eprintln!("! spawn host: {e}");
            return 1;
        }
    }

    // Poll for socket file
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if Instant::now() > deadline {
            eprintln!("! timeout waiting for session host to start");
            return 1;
        }
        std::thread::sleep(Duration::from_millis(100));

        if !sock.exists() {
            continue;
        }

        // Verify host is responsive
        match send_request(alias, &SessionRequest::Ping) {
            Ok(SessionResponse::Pong { pid, .. }) => {
                println!("+ session \"{alias}\" started (pid {pid})");
                return 0;
            }
            _ => continue,
        }
    }
}

/// `mish session send <alias> <input>` or `mish send <input>`
pub fn cmd_session_send(alias: &str, input: &str, timeout: u64) -> i32 {
    let request = SessionRequest::Send {
        input: input.to_string(),
        timeout_secs: timeout,
    };

    match send_request(alias, &request) {
        Ok(SessionResponse::Ok {
            output, exit_code, ..
        }) => {
            if !output.is_empty() {
                println!("{output}");
            }
            exit_code
        }
        Ok(SessionResponse::Error { message }) => {
            eprintln!("! {message}");
            1
        }
        Ok(other) => {
            eprintln!("! unexpected response: {:?}", other);
            1
        }
        Err(e) => {
            eprintln!("! {e}");
            1
        }
    }
}

/// `mish send <input>` — sends to the sole active session.
pub fn cmd_send(input: &str, timeout: u64) -> i32 {
    let sessions = find_sessions();
    match sessions.len() {
        0 => {
            eprintln!("! no active sessions (use `mish session start` first)");
            1
        }
        1 => cmd_session_send(&sessions[0].0, input, timeout),
        n => {
            eprintln!("! {n} active sessions — specify alias:");
            for (alias, _) in &sessions {
                eprintln!("  mish session send {alias} \"...\"");
            }
            1
        }
    }
}

/// `mish session list`
pub fn cmd_session_list() -> i32 {
    let sessions = find_sessions();
    if sessions.is_empty() {
        println!("no active sessions");
        return 0;
    }

    println!("{:<12} {:<8} {:<10} CMD", "ALIAS", "PID", "UPTIME");

    for (alias, _) in &sessions {
        match send_request(alias, &SessionRequest::Ping) {
            Ok(SessionResponse::Pong {
                pid, uptime_secs, ..
            }) => {
                let cmd_text = std::fs::read_to_string(cmd_path(alias)).unwrap_or_default();
                let uptime = format_uptime(uptime_secs);
                println!("{:<12} {:<8} {:<10} {}", alias, pid, uptime, cmd_text.trim());
            }
            _ => {
                // Stale socket
                let _ = std::fs::remove_file(socket_path(alias));
                let _ = std::fs::remove_file(pid_path(alias));
                let _ = std::fs::remove_file(cmd_path(alias));
            }
        }
    }

    0
}

fn format_uptime(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// `mish session close <alias>`
pub fn cmd_session_close(alias: &str) -> i32 {
    match send_request(alias, &SessionRequest::Close) {
        Ok(SessionResponse::Ok { .. }) => {
            // Wait briefly for socket removal
            let sock = socket_path(alias);
            let deadline = Instant::now() + Duration::from_secs(2);
            while sock.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            println!("+ session \"{alias}\" closed");
            0
        }
        Ok(SessionResponse::Error { message }) => {
            eprintln!("! {message}");
            1
        }
        Err(e) => {
            eprintln!("! {e}");
            1
        }
        _ => 1,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protocol_serde_roundtrip_send() {
        let req = SessionRequest::Send {
            input: "import ast".into(),
            timeout_secs: 30,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: SessionRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            SessionRequest::Send { input, timeout_secs } => {
                assert_eq!(input, "import ast");
                assert_eq!(timeout_secs, 30);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_protocol_serde_roundtrip_ping() {
        let req = SessionRequest::Ping;
        let json = serde_json::to_string(&req).unwrap();
        let parsed: SessionRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, SessionRequest::Ping));
    }

    #[test]
    fn test_protocol_serde_roundtrip_close() {
        let req = SessionRequest::Close;
        let json = serde_json::to_string(&req).unwrap();
        let parsed: SessionRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, SessionRequest::Close));
    }

    #[test]
    fn test_protocol_serde_roundtrip_list() {
        let req = SessionRequest::List;
        let json = serde_json::to_string(&req).unwrap();
        let parsed: SessionRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, SessionRequest::List));
    }

    #[test]
    fn test_response_serde_ok() {
        let resp = SessionResponse::Ok {
            output: "hello".into(),
            exit_code: 0,
            elapsed_ms: 42,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: SessionResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            SessionResponse::Ok {
                output,
                exit_code,
                elapsed_ms,
            } => {
                assert_eq!(output, "hello");
                assert_eq!(exit_code, 0);
                assert_eq!(elapsed_ms, 42);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_response_serde_pong() {
        let resp = SessionResponse::Pong {
            alias: "py".into(),
            pid: 1234,
            uptime_secs: 60,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: SessionResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            SessionResponse::Pong {
                alias,
                pid,
                uptime_secs,
            } => {
                assert_eq!(alias, "py");
                assert_eq!(pid, 1234);
                assert_eq!(uptime_secs, 60);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_response_serde_error() {
        let resp = SessionResponse::Error {
            message: "boom".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("boom"));
    }

    #[test]
    fn test_response_serde_sessions() {
        let resp = SessionResponse::Sessions {
            entries: vec![SessionListEntry {
                alias: "py".into(),
                pid: 999,
                uptime_secs: 120,
                cmd: "python3".into(),
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: SessionResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            SessionResponse::Sessions { entries } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].alias, "py");
            }
            _ => panic!("wrong variant"),
        }
    }

    // Interpreter kind/sentinel/strip tests are in crate::interpreter::session::tests.

    #[test]
    fn test_sessions_dir_path() {
        let dir = sessions_dir();
        assert!(dir.to_string_lossy().contains("mish/sessions"));
    }

    #[test]
    fn test_socket_path_format() {
        let p = socket_path("py");
        assert!(p.to_string_lossy().ends_with("py.sock"));
    }

    #[test]
    fn test_pid_path_format() {
        let p = pid_path("py");
        assert!(p.to_string_lossy().ends_with("py.pid"));
    }

    #[test]
    fn test_format_uptime() {
        assert_eq!(format_uptime(5), "5s");
        assert_eq!(format_uptime(65), "1m5s");
        assert_eq!(format_uptime(3661), "1h1m");
    }

    // squash_session_output tests are in crate::interpreter::session::tests.

    #[test]
    fn test_default_timeout_value() {
        let json = r#"{"type":"send","input":"x"}"#;
        let req: SessionRequest = serde_json::from_str(json).unwrap();
        match req {
            SessionRequest::Send { timeout_secs, .. } => assert_eq!(timeout_secs, 30),
            _ => panic!("wrong variant"),
        }
    }

    // strip_echo_and_sentinel + strip_prompt tests are in crate::interpreter::session::tests.
}
