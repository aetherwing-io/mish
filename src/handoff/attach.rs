//! Unix domain socket server and client for operator handoff.
//!
//! The server creates a Unix socket at `$XDG_RUNTIME_DIR/mish/<pid>/control.sock`
//! (or `/tmp/mish/<pid>/control.sock` on macOS). The operator runs `mish attach hf_...`
//! which connects to this socket, validates the handoff ID, and proxies PTY I/O.
//!
//! # Protocol
//!
//! 1. Client connects to the control socket
//! 2. Client sends a JSON request line: `{"type":"attach","handoff_id":"hf_..."}\n`
//! 3. Server validates: exists, not expired, not already attached
//! 4. Server responds with JSON: `{"status":"ok","alias":"<alias>"}\n`
//!    or `{"status":"error","message":"..."}\n`
//! 5. After "ok", all subsequent bytes are raw PTY I/O:
//!    - Client→Server: operator keystrokes → PTY stdin
//!    - Server→Client: PTY output → operator terminal
//! 6. Client sends `{"type":"detach"}\n` or disconnects → server recaptures
//!
//! The "list" command is separate: client sends `{"type":"list"}\n`,
//! server responds with `{"handoffs":[...]}\n`.

use std::fmt;
use std::os::unix::io::{AsRawFd, BorrowedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex as TokioMutex;

/// Type alias for the PTY fd lookup callback used by the control server.
type PtyFdLookup = Box<dyn Fn(&str) -> Option<RawFd> + Send + Sync>;

use super::state::{HandoffError, HandoffManager};

// ---------------------------------------------------------------------------
// Protocol types
// ---------------------------------------------------------------------------

/// Client request sent over the control socket.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlRequest {
    /// Attach to a handoff session.
    Attach {
        handoff_id: String,
        /// Whether operator opted in to sharing output with the LLM.
        #[serde(default)]
        share_output: bool,
    },
    /// Detach from current handoff session.
    Detach,
    /// List active handoffs.
    List,
}

/// Server response on the control socket.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ControlResponse {
    /// Attach succeeded — PTY I/O follows.
    Ok {
        alias: String,
    },
    /// Operation failed.
    Error {
        message: String,
    },
    /// List of active handoffs.
    Handoffs {
        entries: Vec<HandoffListEntry>,
    },
}

/// Entry in the handoffs list response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffListEntry {
    pub alias: String,
    pub reason: String,
    pub duration_secs: u64,
    pub attached: bool,
    pub reference_id: String,
}

// ---------------------------------------------------------------------------
// AttachError
// ---------------------------------------------------------------------------

/// Errors from the attach subsystem.
#[derive(Debug)]
pub enum AttachError {
    /// I/O error (socket, file system).
    Io(std::io::Error),
    /// Handoff state error.
    Handoff(HandoffError),
    /// Protocol error (invalid JSON, unexpected message).
    Protocol(String),
    /// Socket path is too long for Unix domain sockets (max ~104 bytes).
    PathTooLong(PathBuf),
    /// Server is not running or socket not found.
    ServerNotFound(String),
}

impl fmt::Display for AttachError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AttachError::Io(e) => write!(f, "I/O error: {e}"),
            AttachError::Handoff(e) => write!(f, "handoff error: {e}"),
            AttachError::Protocol(e) => write!(f, "protocol error: {e}"),
            AttachError::PathTooLong(p) => write!(f, "socket path too long: {}", p.display()),
            AttachError::ServerNotFound(msg) => write!(f, "server not found: {msg}"),
        }
    }
}

impl std::error::Error for AttachError {}

impl From<std::io::Error> for AttachError {
    fn from(e: std::io::Error) -> Self {
        AttachError::Io(e)
    }
}

impl From<HandoffError> for AttachError {
    fn from(e: HandoffError) -> Self {
        AttachError::Handoff(e)
    }
}

// ---------------------------------------------------------------------------
// Socket path management
// ---------------------------------------------------------------------------

/// Determine the control socket directory for a given server PID.
///
/// Path: `$XDG_RUNTIME_DIR/mish/<pid>/` or `/tmp/mish/<pid>/`.
pub fn socket_dir(server_pid: u32) -> PathBuf {
    let base = match std::env::var("XDG_RUNTIME_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("mish"),
        _ => PathBuf::from("/tmp/mish"),
    };
    base.join(server_pid.to_string())
}

/// Full path to the control socket for a given server PID.
pub fn socket_path(server_pid: u32) -> PathBuf {
    socket_dir(server_pid).join("control.sock")
}

/// Find control sockets for all running mish server instances.
///
/// Scans the mish runtime directory for `<pid>/control.sock` entries
/// where the PID corresponds to a still-running process.
pub fn find_server_sockets() -> Vec<(u32, PathBuf)> {
    let base = match std::env::var("XDG_RUNTIME_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("mish"),
        _ => PathBuf::from("/tmp/mish"),
    };

    if !base.exists() {
        return Vec::new();
    }

    let mut results = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if let Ok(pid) = name.parse::<u32>() {
                        let sock = path.join("control.sock");
                        if sock.exists() && is_process_alive(pid) {
                            results.push((pid, sock));
                        }
                    }
                }
            }
        }
    }

    results
}

/// Check if a process is still alive (using kill(pid, 0)).
fn is_process_alive(pid: u32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(nix::errno::Errno::EPERM) => true, // exists but not ours
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// ControlServer — Unix socket server
// ---------------------------------------------------------------------------

/// Shared state that the control server needs access to.
pub struct ControlServerState {
    pub handoff_manager: TokioMutex<HandoffManager>,
    /// Callback to get a PTY master fd for a given alias.
    /// Returns (raw_fd, spool_read_fn) or None if alias not found.
    pub pty_fd_lookup: PtyFdLookup,
}

/// Unix domain socket server for the mish control protocol.
pub struct ControlServer {
    listener: UnixListener,
    socket_path: PathBuf,
}

impl ControlServer {
    /// Create and bind the control socket.
    ///
    /// Creates parent directories and sets permissions to 0600.
    pub fn bind(server_pid: u32) -> Result<Self, AttachError> {
        let dir = socket_dir(server_pid);
        std::fs::create_dir_all(&dir)?;

        let path = dir.join("control.sock");

        // Validate path length (Unix domain sockets have a ~104 byte limit).
        let path_str = path.to_string_lossy();
        if path_str.len() > 100 {
            return Err(AttachError::PathTooLong(path));
        }

        // Remove stale socket file if it exists.
        if path.exists() {
            std::fs::remove_file(&path)?;
        }

        let listener = UnixListener::bind(&path)?;

        // Set socket file to 0600 (owner-only).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&path, permissions)?;
        }

        Ok(ControlServer {
            listener,
            socket_path: path,
        })
    }

    /// Get the socket path.
    pub fn path(&self) -> &Path {
        &self.socket_path
    }

    /// Accept connections and handle them.
    ///
    /// Runs until the shutdown signal is received.
    pub async fn serve(
        self,
        state: Arc<ControlServerState>,
        mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    ) {
        loop {
            tokio::select! {
                accept_result = self.listener.accept() => {
                    match accept_result {
                        Ok((stream, _addr)) => {
                            let state = state.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_client(stream, state).await {
                                    tracing::warn!("control client error: {e}");
                                }
                            });
                        }
                        Err(e) => {
                            tracing::warn!("control socket accept error: {e}");
                        }
                    }
                }
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }

        // Cleanup: remove socket file.
        let _ = std::fs::remove_file(&self.socket_path);
        // Remove parent directory if empty.
        if let Some(parent) = self.socket_path.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        // Best-effort cleanup.
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

// ---------------------------------------------------------------------------
// Client-side connection
// ---------------------------------------------------------------------------

/// Connect to a running mish server's control socket.
///
/// If `server_pid` is provided, connect to that specific server.
/// Otherwise, discover the server by scanning the runtime directory.
pub async fn connect_to_server(server_pid: Option<u32>) -> Result<UnixStream, AttachError> {
    let path = match server_pid {
        Some(pid) => {
            let p = socket_path(pid);
            if !p.exists() {
                return Err(AttachError::ServerNotFound(format!(
                    "no control socket for PID {pid} at {}",
                    p.display()
                )));
            }
            p
        }
        None => {
            let sockets = find_server_sockets();
            match sockets.len() {
                0 => {
                    return Err(AttachError::ServerNotFound(
                        "no running mish server found".into(),
                    ))
                }
                1 => sockets[0].1.clone(),
                n => {
                    return Err(AttachError::ServerNotFound(format!(
                        "{n} mish servers found; specify --pid to choose one"
                    )))
                }
            }
        }
    };

    let stream = UnixStream::connect(&path).await?;
    Ok(stream)
}

/// Send an attach request and wait for the response.
pub async fn send_attach_request(
    stream: &mut UnixStream,
    handoff_id: &str,
    share_output: bool,
) -> Result<String, AttachError> {
    let request = serde_json::json!({
        "type": "attach",
        "handoff_id": handoff_id,
        "share_output": share_output,
    });
    let mut line = serde_json::to_string(&request)
        .map_err(|e| AttachError::Protocol(format!("serialize error: {e}")))?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;

    // Read response line.
    let mut buf = BufReader::new(stream);
    let mut response_line = String::new();
    buf.read_line(&mut response_line).await?;

    let response: serde_json::Value = serde_json::from_str(response_line.trim())
        .map_err(|e| AttachError::Protocol(format!("invalid response: {e}")))?;

    match response.get("status").and_then(|s| s.as_str()) {
        Some("ok") => {
            let alias = response
                .get("alias")
                .and_then(|a| a.as_str())
                .unwrap_or("unknown")
                .to_string();
            Ok(alias)
        }
        Some("error") => {
            let msg = response
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_string();
            Err(AttachError::Protocol(msg))
        }
        _ => Err(AttachError::Protocol(format!(
            "unexpected response: {response_line}"
        ))),
    }
}

/// Send a list request and get active handoffs.
pub async fn send_list_request(
    stream: &mut UnixStream,
) -> Result<Vec<HandoffListEntry>, AttachError> {
    let request = serde_json::json!({"type": "list"});
    let mut line = serde_json::to_string(&request)
        .map_err(|e| AttachError::Protocol(format!("serialize error: {e}")))?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;

    // Read response line.
    let mut buf = BufReader::new(stream);
    let mut response_line = String::new();
    buf.read_line(&mut response_line).await?;

    let response: serde_json::Value = serde_json::from_str(response_line.trim())
        .map_err(|e| AttachError::Protocol(format!("invalid response: {e}")))?;

    match response.get("status").and_then(|s| s.as_str()) {
        Some("handoffs") => {
            let entries_val = response
                .get("entries")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([]));
            let entries: Vec<HandoffListEntry> = serde_json::from_value(entries_val)
                .map_err(|e| AttachError::Protocol(format!("invalid entries: {e}")))?;
            Ok(entries)
        }
        Some("error") => {
            let msg = response
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_string();
            Err(AttachError::Protocol(msg))
        }
        _ => Err(AttachError::Protocol(format!(
            "unexpected response: {response_line}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Server-side client handler
// ---------------------------------------------------------------------------

/// Handle a single client connection on the control socket.
async fn handle_client(
    stream: UnixStream,
    state: Arc<ControlServerState>,
) -> Result<(), AttachError> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    // Read the initial request line.
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(()); // Client disconnected.
    }

    let request: ControlRequest = serde_json::from_str(line.trim())
        .map_err(|e| AttachError::Protocol(format!("invalid request: {e}")))?;

    match request {
        ControlRequest::Attach {
            handoff_id,
            share_output: _,
        } => {
            handle_attach(&handoff_id, &state, reader, &mut writer).await
        }
        ControlRequest::List => {
            handle_list(&state, &mut writer).await
        }
        ControlRequest::Detach => {
            // Detach without prior attach is a no-op.
            Ok(())
        }
    }
}

/// Handle an attach request: validate, start PTY I/O proxy.
async fn handle_attach(
    handoff_id: &str,
    state: &Arc<ControlServerState>,
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
) -> Result<(), AttachError> {
    // Get operator PID (use connection peer credential if available, else 0).
    let operator_pid = std::process::id(); // Fallback; real impl would use SO_PEERCRED.

    // Validate and attach.
    let alias = {
        let mut mgr = state.handoff_manager.lock().await;
        match mgr.attach(handoff_id, operator_pid) {
            Ok(entry) => {
                let alias = entry.alias.clone();
                // Send success response.
                let response = ControlResponse::Ok {
                    alias: alias.clone(),
                };
                let mut resp_line = serde_json::to_string(&response).unwrap();
                resp_line.push('\n');
                writer.write_all(resp_line.as_bytes()).await?;
                alias
            }
            Err(e) => {
                let message = e.to_string();
                let response = ControlResponse::Error { message };
                let mut resp_line = serde_json::to_string(&response).unwrap();
                resp_line.push('\n');
                writer.write_all(resp_line.as_bytes()).await?;
                return Ok(());
            }
        }
    };

    // Look up the PTY fd for this alias.
    let pty_fd = match (state.pty_fd_lookup)(&alias) {
        Some(fd) => fd,
        None => {
            // PTY not found — detach and report error.
            let mut mgr = state.handoff_manager.lock().await;
            let _ = mgr.detach(handoff_id, 0);
            return Err(AttachError::Protocol(format!(
                "PTY not found for alias {alias:?}"
            )));
        }
    };

    // Proxy I/O between client and PTY.
    // Client → PTY: operator keystrokes
    // PTY → Client: process output
    let proxy_result = proxy_pty_io(pty_fd, reader, writer).await;

    // On disconnect or detach: recapture by detaching.
    let _summary = {
        let mut mgr = state.handoff_manager.lock().await;
        // lines_during_handoff is 0 here — real impl would track from spool.
        mgr.detach(handoff_id, 0).ok()
    };

    proxy_result
}

/// Proxy raw I/O between a Unix socket client and a PTY master fd.
///
/// This is the core of the attach experience: operator keystrokes go to the
/// PTY, and PTY output goes to the operator's terminal.
///
/// Returns when the client disconnects, sends a detach request, or an error occurs.
async fn proxy_pty_io(
    pty_fd: RawFd,
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
) -> Result<(), AttachError> {
    use tokio::io::AsyncReadExt;
    let mut reader = reader;

    // Wrap the PTY fd in an AsyncFd for non-blocking reads.
    let pty_async = match tokio::io::unix::AsyncFd::new(PtyFdWrapper(pty_fd)) {
        Ok(fd) => fd,
        Err(e) => return Err(AttachError::Io(e)),
    };

    let mut client_buf = [0u8; 4096];
    let mut pty_buf = [0u8; 4096];

    loop {
        tokio::select! {
            // Client → PTY: read from socket, write to PTY
            result = reader.read(&mut client_buf) => {
                match result {
                    Ok(0) => break, // Client disconnected
                    Ok(n) => {
                        // Check if it's a detach request (JSON line).
                        if let Ok(text) = std::str::from_utf8(&client_buf[..n]) {
                            if let Ok(req) = serde_json::from_str::<ControlRequest>(text.trim()) {
                                if matches!(req, ControlRequest::Detach) {
                                    break;
                                }
                            }
                        }
                        // Raw PTY write.
                        // SAFETY: pty_fd is valid for the duration of the proxy session.
                        let borrowed = unsafe { BorrowedFd::borrow_raw(pty_fd) };
                        let write_result = nix::unistd::write(borrowed, &client_buf[..n]);
                        if let Err(e) = write_result {
                            return Err(AttachError::Io(std::io::Error::from_raw_os_error(e as i32)));
                        }
                    }
                    Err(e) => return Err(AttachError::Io(e)),
                }
            }
            // PTY → Client: read from PTY, write to socket
            result = pty_async.readable() => {
                match result {
                    Ok(mut guard) => {
                        match guard.try_io(|inner| {
                            let fd = inner.get_ref().0;
                            match nix::unistd::read(fd, &mut pty_buf) {
                                Ok(n) => Ok(n),
                                Err(nix::errno::Errno::EAGAIN) => {
                                    Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
                                }
                                Err(nix::errno::Errno::EIO) => Ok(0), // child exited
                                Err(e) => Err(std::io::Error::from_raw_os_error(e as i32)),
                            }
                        }) {
                            Ok(Ok(0)) => break, // PTY closed (child exited)
                            Ok(Ok(n)) => {
                                if let Err(e) = writer.write_all(&pty_buf[..n]).await {
                                    return Err(AttachError::Io(e));
                                }
                            }
                            Ok(Err(e)) => return Err(AttachError::Io(e)),
                            Err(_would_block) => continue,
                        }
                    }
                    Err(e) => return Err(AttachError::Io(e)),
                }
            }
        }
    }

    Ok(())
}

/// Wrapper to implement AsRawFd for use with tokio::io::unix::AsyncFd.
#[derive(Debug)]
struct PtyFdWrapper(RawFd);

impl AsRawFd for PtyFdWrapper {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

/// Handle a list request: return all active handoffs.
async fn handle_list(
    state: &Arc<ControlServerState>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
) -> Result<(), AttachError> {
    let mgr = state.handoff_manager.lock().await;
    let active = mgr.list_active();

    let entries: Vec<HandoffListEntry> = active
        .iter()
        .map(|entry| HandoffListEntry {
            alias: entry.alias.clone(),
            reason: entry.reason.clone(),
            duration_secs: entry.initiated_at.elapsed().as_secs(),
            attached: entry.attached,
            reference_id: entry.reference_id.clone(),
        })
        .collect();

    let response = ControlResponse::Handoffs { entries };
    let mut resp_line = serde_json::to_string(&response).unwrap();
    resp_line.push('\n');
    writer.write_all(resp_line.as_bytes()).await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Terminal raw mode helpers (for the client side — `mish attach`)
// ---------------------------------------------------------------------------

/// Set the terminal to raw mode for PTY proxying.
///
/// Returns the original termios settings for restoration on detach.
///
/// # Safety
/// The caller must ensure that `fd` is a valid, open file descriptor
/// for the lifetime of this call.
pub fn set_raw_mode(fd: RawFd) -> Result<nix::sys::termios::Termios, AttachError> {
    use nix::sys::termios::{self, SetArg};

    // SAFETY: caller guarantees fd is valid and open.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };

    let original = termios::tcgetattr(borrowed)
        .map_err(|e| AttachError::Io(std::io::Error::from_raw_os_error(e as i32)))?;

    let mut raw = original.clone();
    termios::cfmakeraw(&mut raw);
    termios::tcsetattr(borrowed, SetArg::TCSANOW, &raw)
        .map_err(|e| AttachError::Io(std::io::Error::from_raw_os_error(e as i32)))?;

    Ok(original)
}

/// Restore terminal settings from raw mode.
///
/// # Safety
/// The caller must ensure that `fd` is a valid, open file descriptor
/// for the lifetime of this call.
pub fn restore_terminal(fd: RawFd, termios: &nix::sys::termios::Termios) -> Result<(), AttachError> {
    use nix::sys::termios::{self, SetArg};

    // SAFETY: caller guarantees fd is valid and open.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };

    termios::tcsetattr(borrowed, SetArg::TCSANOW, termios)
        .map_err(|e| AttachError::Io(std::io::Error::from_raw_os_error(e as i32)))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    // ── Test 1: socket_dir uses XDG_RUNTIME_DIR when set ──

    #[test]
    #[serial(xdg)]
    fn socket_dir_with_xdg() {
        // Save original.
        let original = std::env::var("XDG_RUNTIME_DIR").ok();

        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        let dir = socket_dir(12345);
        assert_eq!(dir, PathBuf::from("/run/user/1000/mish/12345"));

        // Restore.
        match original {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    // ── Test 2: socket_dir falls back to /tmp when XDG not set ──

    #[test]
    #[serial(xdg)]
    fn socket_dir_fallback() {
        let original = std::env::var("XDG_RUNTIME_DIR").ok();

        std::env::remove_var("XDG_RUNTIME_DIR");
        let dir = socket_dir(99999);
        assert_eq!(dir, PathBuf::from("/tmp/mish/99999"));

        if let Some(v) = original {
            std::env::set_var("XDG_RUNTIME_DIR", v);
        }
    }

    // ── Test 3: socket_path is dir + control.sock ──

    #[test]
    #[serial(xdg)]
    fn socket_path_format() {
        let original = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::remove_var("XDG_RUNTIME_DIR");

        let path = socket_path(42);
        assert_eq!(path, PathBuf::from("/tmp/mish/42/control.sock"));

        if let Some(v) = original {
            std::env::set_var("XDG_RUNTIME_DIR", v);
        }
    }

    // ── Test 4: ControlRequest deserialization — attach ──

    #[test]
    fn deserialize_attach_request() {
        let json = r#"{"type":"attach","handoff_id":"hf_abc123","share_output":false}"#;
        let req: ControlRequest = serde_json::from_str(json).unwrap();
        match req {
            ControlRequest::Attach {
                handoff_id,
                share_output,
            } => {
                assert_eq!(handoff_id, "hf_abc123");
                assert!(!share_output);
            }
            _ => panic!("expected Attach"),
        }
    }

    // ── Test 5: ControlRequest deserialization — attach with share_output default ──

    #[test]
    fn deserialize_attach_request_default_share() {
        let json = r#"{"type":"attach","handoff_id":"hf_xyz"}"#;
        let req: ControlRequest = serde_json::from_str(json).unwrap();
        match req {
            ControlRequest::Attach {
                handoff_id,
                share_output,
            } => {
                assert_eq!(handoff_id, "hf_xyz");
                assert!(!share_output, "share_output should default to false");
            }
            _ => panic!("expected Attach"),
        }
    }

    // ── Test 6: ControlRequest deserialization — list ──

    #[test]
    fn deserialize_list_request() {
        let json = r#"{"type":"list"}"#;
        let req: ControlRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(req, ControlRequest::List));
    }

    // ── Test 7: ControlRequest deserialization — detach ──

    #[test]
    fn deserialize_detach_request() {
        let json = r#"{"type":"detach"}"#;
        let req: ControlRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(req, ControlRequest::Detach));
    }

    // ── Test 8: ControlResponse serialization — ok ──

    #[test]
    fn serialize_ok_response() {
        let resp = ControlResponse::Ok {
            alias: "deploy".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"ok\""));
        assert!(json.contains("\"alias\":\"deploy\""));
    }

    // ── Test 9: ControlResponse serialization — error ──

    #[test]
    fn serialize_error_response() {
        let resp = ControlResponse::Error {
            message: "not found".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"error\""));
        assert!(json.contains("\"message\":\"not found\""));
    }

    // ── Test 10: ControlResponse serialization — handoffs list ──

    #[test]
    fn serialize_handoffs_response() {
        let resp = ControlResponse::Handoffs {
            entries: vec![HandoffListEntry {
                alias: "deploy".into(),
                reason: "MFA required".into(),
                duration_secs: 30,
                attached: true,
                reference_id: "ref_abc".into(),
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"handoffs\""));
        assert!(json.contains("deploy"));
        assert!(json.contains("MFA required"));
    }

    // ── Test 11: AttachError Display formatting ──

    #[test]
    fn attach_error_display() {
        let io_err = AttachError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        ));
        assert!(format!("{io_err}").contains("refused"));

        let handoff_err = AttachError::Handoff(HandoffError::NotFound);
        assert!(format!("{handoff_err}").contains("not found"));

        let protocol_err = AttachError::Protocol("bad json".into());
        assert!(format!("{protocol_err}").contains("bad json"));

        let path_err = AttachError::PathTooLong(PathBuf::from("/very/long/path"));
        assert!(format!("{path_err}").contains("too long"));

        let server_err = AttachError::ServerNotFound("no socket".into());
        assert!(format!("{server_err}").contains("no socket"));
    }

    // ── Test 12: ControlServer bind creates socket file ──

    #[tokio::test]
    #[serial(xdg)]
    async fn control_server_bind_creates_socket() {
        let dir = TempDir::new().unwrap();
        let pid = std::process::id();

        // Override runtime dir to temp.
        let original = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::set_var("XDG_RUNTIME_DIR", dir.path().to_str().unwrap());

        let server = ControlServer::bind(pid);

        // Restore env immediately.
        match &original {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }

        match server {
            Ok(server) => {
                assert!(server.path().exists(), "socket file should exist");
                // Check permissions are 0600.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let meta = std::fs::metadata(server.path()).unwrap();
                    let mode = meta.permissions().mode() & 0o777;
                    assert_eq!(mode, 0o600, "socket should be 0600, got {mode:o}");
                }
                // Drop cleans up.
                drop(server);
            }
            Err(e) => {
                panic!("bind failed: {e}");
            }
        }
    }

    // ── Test 13: ControlServer bind removes stale socket ──

    #[tokio::test]
    #[serial(xdg)]
    async fn control_server_bind_removes_stale_socket() {
        let dir = TempDir::new().unwrap();
        let pid = std::process::id();

        let original = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::set_var("XDG_RUNTIME_DIR", dir.path().to_str().unwrap());

        // Create a stale socket file.
        let sock_dir = dir.path().join("mish").join(pid.to_string());
        std::fs::create_dir_all(&sock_dir).unwrap();
        std::fs::write(sock_dir.join("control.sock"), "stale").unwrap();

        // Bind should succeed, replacing the stale file.
        let server = ControlServer::bind(pid);

        match &original {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }

        assert!(server.is_ok(), "should replace stale socket: {:?}", server.err());
        drop(server);
    }

    // ── Test 14: Unix socket server + client integration — list empty ──

    #[tokio::test]
    #[serial(xdg)]
    async fn integration_list_empty_handoffs() {
        let dir = TempDir::new().unwrap();
        let pid = std::process::id();

        let original = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::set_var("XDG_RUNTIME_DIR", dir.path().to_str().unwrap());

        let server = ControlServer::bind(pid).unwrap();
        let server_path = server.path().to_path_buf();

        let state = Arc::new(ControlServerState {
            handoff_manager: TokioMutex::new(HandoffManager::new()),
            pty_fd_lookup: Box::new(|_| None),
        });

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        // Spawn server.
        let server_handle = tokio::spawn(async move {
            server.serve(state, shutdown_rx).await;
        });

        // Give server time to start.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Connect as client.
        let mut client = UnixStream::connect(&server_path).await.unwrap();

        // Send list request.
        let request = r#"{"type":"list"}"#.to_string() + "\n";
        client.write_all(request.as_bytes()).await.unwrap();

        // Read response.
        let mut buf = BufReader::new(&mut client);
        let mut response_line = String::new();
        buf.read_line(&mut response_line).await.unwrap();

        let response: serde_json::Value = serde_json::from_str(response_line.trim()).unwrap();
        assert_eq!(response["status"], "handoffs");
        assert_eq!(response["entries"].as_array().unwrap().len(), 0);

        // Shutdown.
        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            server_handle,
        )
        .await;

        match &original {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    // ── Test 15: Unix socket — attach with invalid ID returns error ──

    #[tokio::test]
    #[serial(xdg)]
    async fn integration_attach_invalid_id() {
        let dir = TempDir::new().unwrap();
        let pid = std::process::id();

        let original = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::set_var("XDG_RUNTIME_DIR", dir.path().to_str().unwrap());

        let server = ControlServer::bind(pid).unwrap();
        let server_path = server.path().to_path_buf();

        let state = Arc::new(ControlServerState {
            handoff_manager: TokioMutex::new(HandoffManager::new()),
            pty_fd_lookup: Box::new(|_| None),
        });

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let server_handle = tokio::spawn(async move {
            server.serve(state, shutdown_rx).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client = UnixStream::connect(&server_path).await.unwrap();

        // Send attach with non-existent handoff_id.
        let request = r#"{"type":"attach","handoff_id":"hf_nonexistent"}"#.to_string() + "\n";
        client.write_all(request.as_bytes()).await.unwrap();

        let mut buf = BufReader::new(&mut client);
        let mut response_line = String::new();
        buf.read_line(&mut response_line).await.unwrap();

        let response: serde_json::Value = serde_json::from_str(response_line.trim()).unwrap();
        assert_eq!(response["status"], "error");
        assert!(
            response["message"]
                .as_str()
                .unwrap()
                .contains("not found"),
            "expected 'not found' in message, got: {}",
            response["message"]
        );

        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server_handle).await;

        match &original {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    // ── Test 16: Unix socket — attach valid ID then already-attached ──

    #[tokio::test]
    #[serial(xdg)]
    async fn integration_attach_already_attached() {
        let dir = TempDir::new().unwrap();
        let pid = std::process::id();

        let original = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::set_var("XDG_RUNTIME_DIR", dir.path().to_str().unwrap());

        let server = ControlServer::bind(pid).unwrap();
        let server_path = server.path().to_path_buf();

        // Create a handoff in the manager.
        let mut mgr = HandoffManager::new();
        let (hid, _rid) = mgr.create("deploy", "MFA").unwrap();

        let state = Arc::new(ControlServerState {
            handoff_manager: TokioMutex::new(mgr),
            pty_fd_lookup: Box::new(|_| None), // No real PTY — attach will succeed then fail on proxy
        });

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let state_clone = state.clone();

        let server_handle = tokio::spawn(async move {
            server.serve(state_clone, shutdown_rx).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // First attach — should succeed with "ok".
        let mut client1 = UnixStream::connect(&server_path).await.unwrap();
        let request = format!(r#"{{"type":"attach","handoff_id":"{hid}"}}"#) + "\n";
        client1.write_all(request.as_bytes()).await.unwrap();

        let mut buf = BufReader::new(&mut client1);
        let mut response_line = String::new();
        buf.read_line(&mut response_line).await.unwrap();

        let response: serde_json::Value = serde_json::from_str(response_line.trim()).unwrap();
        assert_eq!(response["status"], "ok");
        assert_eq!(response["alias"], "deploy");

        // Drop client1 — server will detach. Wait briefly for cleanup.
        drop(client1);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Second attach — the handoff was detached when client1 dropped, so it's removed.
        // This should return "not found" because detach cleans up.
        let mut client2 = UnixStream::connect(&server_path).await.unwrap();
        let request2 = format!(r#"{{"type":"attach","handoff_id":"{hid}"}}"#) + "\n";
        client2.write_all(request2.as_bytes()).await.unwrap();

        let mut buf2 = BufReader::new(&mut client2);
        let mut response_line2 = String::new();
        buf2.read_line(&mut response_line2).await.unwrap();

        let response2: serde_json::Value = serde_json::from_str(response_line2.trim()).unwrap();
        assert_eq!(
            response2["status"], "error",
            "second attach after detach should fail: {response2}"
        );

        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server_handle).await;

        match &original {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    // ── Test 17: Unix socket — list with active handoffs ──

    #[tokio::test]
    #[serial(xdg)]
    async fn integration_list_active_handoffs() {
        let dir = TempDir::new().unwrap();
        let pid = std::process::id();

        let original = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::set_var("XDG_RUNTIME_DIR", dir.path().to_str().unwrap());

        let server = ControlServer::bind(pid).unwrap();
        let server_path = server.path().to_path_buf();

        let mut mgr = HandoffManager::new();
        mgr.create("deploy", "MFA required").unwrap();
        mgr.create("build", "auth prompt").unwrap();

        let state = Arc::new(ControlServerState {
            handoff_manager: TokioMutex::new(mgr),
            pty_fd_lookup: Box::new(|_| None),
        });

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let server_handle = tokio::spawn(async move {
            server.serve(state, shutdown_rx).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client = UnixStream::connect(&server_path).await.unwrap();
        let request = r#"{"type":"list"}"#.to_string() + "\n";
        client.write_all(request.as_bytes()).await.unwrap();

        let mut buf = BufReader::new(&mut client);
        let mut response_line = String::new();
        buf.read_line(&mut response_line).await.unwrap();

        let response: serde_json::Value = serde_json::from_str(response_line.trim()).unwrap();
        assert_eq!(response["status"], "handoffs");
        let entries = response["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);

        let aliases: Vec<&str> = entries
            .iter()
            .map(|e| e["alias"].as_str().unwrap())
            .collect();
        assert!(aliases.contains(&"deploy"));
        assert!(aliases.contains(&"build"));

        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server_handle).await;

        match &original {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    // ── Test 18: HandoffListEntry serialization ──

    #[test]
    fn handoff_list_entry_serialization() {
        let entry = HandoffListEntry {
            alias: "deploy".into(),
            reason: "MFA".into(),
            duration_secs: 120,
            attached: true,
            reference_id: "ref_abc".into(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("deploy"));
        assert!(json.contains("MFA"));
        assert!(json.contains("120"));
        assert!(json.contains("true"));
        assert!(json.contains("ref_abc"));
    }

    // ── Test 19: is_process_alive for current process ──

    #[test]
    fn is_process_alive_current() {
        assert!(is_process_alive(std::process::id()));
    }

    // ── Test 20: is_process_alive for dead process ──

    #[test]
    fn is_process_alive_dead() {
        assert!(!is_process_alive(4_000_000));
    }

    // ── Test 21: find_server_sockets returns empty when no servers ──

    #[test]
    #[serial(xdg)]
    fn find_server_sockets_empty() {
        let original = std::env::var("XDG_RUNTIME_DIR").ok();
        let dir = TempDir::new().unwrap();
        std::env::set_var("XDG_RUNTIME_DIR", dir.path().to_str().unwrap());

        let sockets = find_server_sockets();
        assert!(sockets.is_empty());

        match original {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    // ── Test 22: set_raw_mode / restore_terminal on a PTY ──
    // (Skipped in unit tests — requires a real terminal. Tested via integration tests.)

    // ── Test 23: AttachError conversions ──

    #[test]
    fn attach_error_from_io() {
        let io_err = std::io::Error::other("test");
        let attach_err: AttachError = io_err.into();
        assert!(matches!(attach_err, AttachError::Io(_)));
    }

    #[test]
    fn attach_error_from_handoff() {
        let handoff_err = HandoffError::NotFound;
        let attach_err: AttachError = handoff_err.into();
        assert!(matches!(attach_err, AttachError::Handoff(_)));
    }
}
