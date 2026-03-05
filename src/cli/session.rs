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
use uuid::Uuid;

use crate::core::line_buffer::LineBuffer;
use crate::core::pty::PtyCapture;
use crate::squasher::pipeline::{Pipeline, PipelineConfig};
use crate::squasher::vte_strip::VteStripper;

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

// ---------------------------------------------------------------------------
// Interpreter kind + sentinel
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterpreterKind {
    Python,
    Node,
    Generic,
}

impl InterpreterKind {
    fn detect(cmd: &str) -> Self {
        let basename = cmd.rsplit('/').next().unwrap_or(cmd);
        if basename.contains("python") {
            InterpreterKind::Python
        } else if basename.contains("node") {
            InterpreterKind::Node
        } else {
            InterpreterKind::Generic
        }
    }

    fn sentinel_cmd(&self, token: &str) -> String {
        match self {
            InterpreterKind::Python => format!("print(\"{token}\")"),
            InterpreterKind::Node => format!("console.log(\"{token}\")"),
            InterpreterKind::Generic => format!("echo {token}"),
        }
    }
}

/// Strip interpreter prompt prefix from a line.
/// Handles Python (`>>> `, `... `), Node (`> `), and generic shells.
fn strip_prompt(line: &str) -> &str {
    for prefix in &[">>> ", "... ", "> "] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return rest;
        }
    }
    line
}

fn make_sentinel() -> (String, String) {
    let id = Uuid::new_v4().simple().to_string();
    let token = format!("__MISH_{id}__");
    (token.clone(), token)
}

// ---------------------------------------------------------------------------
// InterpreterSession — raw PTY + sentinel
// ---------------------------------------------------------------------------

struct InterpreterSession {
    pty: PtyCapture,
    kind: InterpreterKind,
    created_at: Instant,
}

impl InterpreterSession {
    fn spawn(cmd: &str) -> Result<Self, String> {
        let kind = InterpreterKind::detect(cmd);

        // Build argv: for python use -i for interactive, for node use -i
        let args: Vec<String> = match kind {
            InterpreterKind::Python => vec![cmd.to_string(), "-i".to_string()],
            InterpreterKind::Node => vec![cmd.to_string(), "-i".to_string()],
            InterpreterKind::Generic => vec![cmd.to_string()],
        };

        let pty = PtyCapture::spawn(&args).map_err(|e| format!("spawn failed: {e}"))?;

        // Wait briefly for initial prompt
        std::thread::sleep(Duration::from_millis(500));

        // Drain any initial banner/prompt output
        let mut buf = [0u8; 8192];
        loop {
            match pty.read_output(&mut buf) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(_) => break,
            }
        }

        Ok(InterpreterSession {
            pty,
            kind,
            created_at: Instant::now(),
        })
    }

    fn execute(&self, input: &str, timeout: Duration) -> Result<ExecuteResult, String> {
        let start = Instant::now();
        let (sentinel_token, _) = make_sentinel();
        let sentinel_cmd = self.kind.sentinel_cmd(&sentinel_token);

        // Write user input + sentinel.
        // Extra blank line before sentinel closes any open blocks in Python
        // (e.g., `for`, `if`, `def` blocks need a blank line to terminate).
        let separator = match self.kind {
            InterpreterKind::Python => "\n\n",
            _ => "\n",
        };
        let payload = format!("{input}{separator}{sentinel_cmd}\n");
        self.pty
            .write_stdin(payload.as_bytes())
            .map_err(|e| format!("write failed: {e}"))?;

        // Read until sentinel appears on its own line (not inside the echoed command).
        // The PTY echoes the sentinel_cmd, which contains the token. We need to see
        // the token on a line that doesn't also contain the sentinel command text.
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        let deadline = Instant::now() + timeout;

        loop {
            if Instant::now() > deadline {
                return Err("timeout waiting for output".into());
            }

            match self.pty.read_output(&mut buf) {
                Ok(0) => {
                    // Check if interpreter died
                    if let Ok(Some(_status)) = self.pty.try_wait() {
                        return Err("interpreter exited unexpectedly".into());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(n) => {
                    raw.extend_from_slice(&buf[..n]);
                    // Check if sentinel appeared on its own output line.
                    // The echoed command line contains both sentinel_cmd text AND
                    // sentinel_token, so we must distinguish echo from real output.
                    let text = String::from_utf8_lossy(&raw);
                    let found_standalone = text.lines().any(|line| {
                        let clean = line.trim().trim_end_matches('\r');
                        clean.contains(&sentinel_token)
                            && !clean.contains(&sentinel_cmd)
                    });
                    if found_standalone {
                        break;
                    }
                }
                Err(e) => return Err(format!("read error: {e}")),
            }
        }

        // Drain any remaining PTY data (next prompt, etc.) so it doesn't
        // leak into the next command's output.
        std::thread::sleep(Duration::from_millis(50));
        let mut drain_buf = [0u8; 4096];
        loop {
            match self.pty.read_output(&mut drain_buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => continue,
            }
        }

        let elapsed_ms = start.elapsed().as_millis() as u64;

        // Process: VTE strip the raw output, then extract between echo and sentinel
        let raw_text = String::from_utf8_lossy(&raw).to_string();
        let clean = self.strip_echo_and_sentinel(&raw_text, input, &sentinel_token, &sentinel_cmd);

        // Run through squasher pipeline
        let squashed = squash_session_output(&clean);

        Ok(ExecuteResult {
            output: squashed,
            exit_code: 0,
            elapsed_ms,
        })
    }

    /// Strip echoed input, sentinel command, and sentinel output from raw PTY output.
    ///
    /// Uses content-based filtering rather than positional skipping because PTY
    /// echo and interpreter output are interleaved (Python echoes input, then
    /// produces output, then prints a new prompt before echoing the next line).
    fn strip_echo_and_sentinel(
        &self,
        raw: &str,
        input: &str,
        sentinel_token: &str,
        sentinel_cmd: &str,
    ) -> String {
        // VTE strip line-by-line (preserving newline structure).
        // Also strip \r from PTY output.
        let lines: Vec<String> = raw
            .lines()
            .map(|l| {
                let stripped = VteStripper::strip(l.as_bytes()).clean_text;
                stripped.trim_end_matches('\r').to_string()
            })
            .collect();

        let input_lines: Vec<&str> = input.lines().collect();

        let mut result: Vec<&str> = Vec::new();
        for line in &lines {
            let trimmed = line.trim();

            // Skip empty lines and prompt-only lines
            if trimmed.is_empty()
                || trimmed == ">>>"
                || trimmed == ">"
                || trimmed == "..."
            {
                continue;
            }

            // Skip lines containing the sentinel command (echoed sentinel)
            if line.contains(sentinel_cmd) {
                continue;
            }

            // Skip standalone sentinel token output
            let without_prompt = strip_prompt(trimmed);
            if without_prompt == sentinel_token {
                continue;
            }

            // Skip echoed input lines: strip prompt prefix, then exact-match
            // against each input line. This avoids false positives where
            // output text happens to contain the input as a substring.
            let is_echo = input_lines.iter().any(|il| {
                let il = il.trim();
                !il.is_empty() && without_prompt == il
            });
            if is_echo {
                continue;
            }

            result.push(line.as_str());
        }

        result.join("\n")
    }

    fn is_alive(&self) -> bool {
        matches!(self.pty.try_wait(), Ok(None))
    }

    fn uptime_secs(&self) -> u64 {
        self.created_at.elapsed().as_secs()
    }
}

struct ExecuteResult {
    output: String,
    exit_code: i32,
    elapsed_ms: u64,
}

/// Run output through the squasher pipeline (VTE strip + dedup + truncation).
fn squash_session_output(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    let mut lb = LineBuffer::new();
    let lines = lb.finalize(text.as_bytes());

    let mut pipeline = Pipeline::new(PipelineConfig::default());
    for line in lines {
        pipeline.feed(line);
    }
    let result = pipeline.finalize();
    result.join("\n")
}

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
    use serial_test::serial;

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

    #[test]
    fn test_interpreter_kind_detection() {
        assert_eq!(InterpreterKind::detect("python3"), InterpreterKind::Python);
        assert_eq!(
            InterpreterKind::detect("/usr/bin/python3"),
            InterpreterKind::Python
        );
        assert_eq!(InterpreterKind::detect("python"), InterpreterKind::Python);
        assert_eq!(InterpreterKind::detect("node"), InterpreterKind::Node);
        assert_eq!(
            InterpreterKind::detect("/usr/local/bin/node"),
            InterpreterKind::Node
        );
        assert_eq!(InterpreterKind::detect("bash"), InterpreterKind::Generic);
        assert_eq!(InterpreterKind::detect("zsh"), InterpreterKind::Generic);
        assert_eq!(InterpreterKind::detect("ruby"), InterpreterKind::Generic);
    }

    #[test]
    fn test_sentinel_cmd_python() {
        let cmd = InterpreterKind::Python.sentinel_cmd("__MISH_abc123__");
        assert_eq!(cmd, "print(\"__MISH_abc123__\")");
    }

    #[test]
    fn test_sentinel_cmd_node() {
        let cmd = InterpreterKind::Node.sentinel_cmd("__MISH_abc123__");
        assert_eq!(cmd, "console.log(\"__MISH_abc123__\")");
    }

    #[test]
    fn test_sentinel_cmd_generic() {
        let cmd = InterpreterKind::Generic.sentinel_cmd("__MISH_abc123__");
        assert_eq!(cmd, "echo __MISH_abc123__");
    }

    #[test]
    fn test_make_sentinel_format() {
        let (token, _) = make_sentinel();
        assert!(token.starts_with("__MISH_"));
        assert!(token.ends_with("__"));
        assert!(token.len() > 10);
    }

    #[test]
    fn test_make_sentinel_unique() {
        let (t1, _) = make_sentinel();
        let (t2, _) = make_sentinel();
        assert_ne!(t1, t2);
    }

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

    #[test]
    fn test_squash_session_output_empty() {
        assert_eq!(squash_session_output(""), "");
    }

    #[test]
    fn test_squash_session_output_simple() {
        let output = squash_session_output("hello world");
        assert!(output.contains("hello world"));
    }

    #[test]
    fn test_default_timeout_value() {
        let json = r#"{"type":"send","input":"x"}"#;
        let req: SessionRequest = serde_json::from_str(json).unwrap();
        match req {
            SessionRequest::Send { timeout_secs, .. } => assert_eq!(timeout_secs, 30),
            _ => panic!("wrong variant"),
        }
    }

    fn make_test_session() -> InterpreterSession {
        InterpreterSession {
            pty: PtyCapture::spawn(&["/bin/echo".into()]).unwrap(),
            kind: InterpreterKind::Python,
            created_at: Instant::now(),
        }
    }

    #[test]
    #[serial(pty)]
    fn test_strip_consecutive_echo_then_output() {
        // Simple case: echo and output are consecutive (command 1 after start)
        let input = "print(\"hello\")";
        let sentinel_token = "__MISH_test123__";
        let sentinel_cmd = "print(\"__MISH_test123__\")";
        // Observed pattern for first command: prompt-prefixed echo
        let raw = format!(
            ">>> {input}\n>>> {sentinel_cmd}\nhello\n{sentinel_token}\n>>> "
        );

        let session = make_test_session();
        let result = session.strip_echo_and_sentinel(&raw, input, sentinel_token, sentinel_cmd);
        assert_eq!(result, "hello");
    }

    #[test]
    #[serial(pty)]
    fn test_strip_interleaved_echo_and_output() {
        // Real PTY behavior: echo, output, prompt+echo, sentinel output
        // This is what happens after the first command when prompt was drained.
        let input = "print(x)";
        let sentinel_token = "__MISH_abc123__";
        let sentinel_cmd = "print(\"__MISH_abc123__\")";
        let raw = format!(
            "{input}\r\n42\r\n>>> {sentinel_cmd}\r\n{sentinel_token}\r\n"
        );

        let session = make_test_session();
        let result = session.strip_echo_and_sentinel(&raw, input, sentinel_token, sentinel_cmd);
        assert_eq!(result, "42");
    }

    #[test]
    #[serial(pty)]
    fn test_strip_no_output_command() {
        // Assignment produces no output
        let input = "x = 42";
        let sentinel_token = "__MISH_def456__";
        let sentinel_cmd = "print(\"__MISH_def456__\")";
        let raw = format!(
            ">>> {input}\r\n>>> {sentinel_cmd}\r\n{sentinel_token}\r\n"
        );

        let session = make_test_session();
        let result = session.strip_echo_and_sentinel(&raw, input, sentinel_token, sentinel_cmd);
        assert_eq!(result, "");
    }

    #[test]
    #[serial(pty)]
    fn test_strip_multiline_output() {
        let input = "for i in range(3): print(i)";
        let sentinel_token = "__MISH_ghi789__";
        let sentinel_cmd = "print(\"__MISH_ghi789__\")";
        let raw = format!(
            ">>> {input}\r\n0\r\n1\r\n2\r\n>>> {sentinel_cmd}\r\n{sentinel_token}\r\n>>> "
        );

        let session = make_test_session();
        let result = session.strip_echo_and_sentinel(&raw, input, sentinel_token, sentinel_cmd);
        assert_eq!(result, "0\n1\n2");
    }

    #[test]
    fn test_strip_prompt_helper() {
        assert_eq!(strip_prompt(">>> print(x)"), "print(x)");
        assert_eq!(strip_prompt("... print(x)"), "print(x)");
        assert_eq!(strip_prompt("> console.log(x)"), "console.log(x)");
        assert_eq!(strip_prompt("hello"), "hello");
        assert_eq!(strip_prompt(">>>"), ">>>");
    }
}
