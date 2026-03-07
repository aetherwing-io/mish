//! MCP Server — main event loop for `mish serve`.
//!
//! Provides `McpServer` which wires together config, session management,
//! process table, MCP dispatch, audit logging, and graceful shutdown into
//! a single event loop reading JSON-RPC from a transport.

use std::fmt;
use std::sync::Arc;

use tokio::io::{AsyncBufRead, AsyncWrite};
use tokio::sync::watch;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinSet;

use uuid::Uuid;

use crate::audit::logger::{AuditEntry, AuditEvent, AuditLogger};
use crate::config::{load_config, MishConfig};
use crate::config_loader::default_runtime_config;
use crate::mcp::dispatch::McpDispatcher;
use crate::mcp::transport::{StdioTransport, TransportError};
use crate::process::table::ProcessTable;
use crate::session::manager::SessionManager;
use crate::shutdown::ShutdownManager;

// ---------------------------------------------------------------------------
// ServerError
// ---------------------------------------------------------------------------

/// Errors that can occur during MCP server operation.
#[derive(Debug)]
pub enum ServerError {
    /// Configuration loading or validation error.
    Config(String),
    /// Transport (I/O) error.
    Transport(String),
    /// Session management error.
    Session(String),
    /// General I/O error.
    Io(std::io::Error),
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServerError::Config(e) => write!(f, "config error: {e}"),
            ServerError::Transport(e) => write!(f, "transport error: {e}"),
            ServerError::Session(e) => write!(f, "session error: {e}"),
            ServerError::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for ServerError {}

impl From<std::io::Error> for ServerError {
    fn from(e: std::io::Error) -> Self {
        ServerError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// McpServer
// ---------------------------------------------------------------------------

/// The MCP server, holding all shared state for request processing.
pub struct McpServer {
    session_manager: Arc<SessionManager>,
    process_table: Arc<TokioMutex<ProcessTable>>,
    dispatcher: Arc<McpDispatcher>,
    _config: Arc<MishConfig>,
}

impl McpServer {
    /// Create a new MCP server with all components wired together.
    pub fn new(config: Arc<MishConfig>) -> Result<Self, ServerError> {
        let session_manager = Arc::new(SessionManager::new(config.clone()));
        let process_table = Arc::new(TokioMutex::new(ProcessTable::new(&config)));
        let rc = default_runtime_config();
        let dispatcher = McpDispatcher::new(
            session_manager.clone(),
            process_table.clone(),
            config.clone(),
            rc.grammars,
            rc.categories_config,
            rc.dangerous_patterns,
        );
        Ok(Self {
            session_manager,
            process_table,
            dispatcher: Arc::new(dispatcher),
            _config: config,
        })
    }

    /// Access the session manager.
    pub fn session_manager(&self) -> &Arc<SessionManager> {
        &self.session_manager
    }

    /// Run the server event loop on the given transport.
    ///
    /// Reads JSON-RPC requests, dispatches them, and writes responses.
    /// Returns on EOF or fatal transport error.
    pub async fn run<R, W>(
        &self,
        transport: &mut StdioTransport<R, W>,
    ) -> Result<(), ServerError>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        loop {
            match transport.read_request().await {
                Ok(Some(request)) => {
                    if let Some(response) = self.dispatcher.dispatch(request).await {
                        transport
                            .write_response(response)
                            .await
                            .map_err(|e| ServerError::Transport(e.to_string()))?;
                    }
                }
                Ok(None) => break,
                Err(TransportError::Eof) => break,
                Err(e) => return Err(ServerError::Transport(e.to_string())),
            }
        }
        Ok(())
    }

    /// Run with shutdown awareness via a watch channel.
    ///
    /// Same as `run()` but also exits when the shutdown signal is received.
    pub async fn run_with_shutdown<R, W>(
        &self,
        transport: &mut StdioTransport<R, W>,
        mut shutdown_rx: watch::Receiver<bool>,
    ) -> Result<(), ServerError>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        loop {
            tokio::select! {
                result = transport.read_request() => {
                    match result {
                        Ok(Some(request)) => {
                            if let Some(response) = self.dispatcher.dispatch(request).await {
                                transport
                                    .write_response(response)
                                    .await
                                    .map_err(|e| ServerError::Transport(e.to_string()))?;
                            }
                        }
                        Ok(None) => break,
                        Err(TransportError::Eof) => break,
                        Err(e) => return Err(ServerError::Transport(e.to_string())),
                    }
                }
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// run_server — entry point for `mish serve`
// ---------------------------------------------------------------------------

/// Entry point for `mish serve`.
///
/// Full lifecycle:
/// 1. Load config
/// 2. Clean stale PID files
/// 3. Write PID file
/// 4. Create server + main session
/// 5. Audit log: ServerStarted
/// 6. Install signal handlers + run transport loop
/// 7. On exit: audit ServerShutdown, flush, remove PID, close sessions
pub async fn run_server(config_path: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    // 0. Check if daemon is running — if so, proxy stdio↔socket
    //    Skip if MISH_NO_DAEMON=1 (for tests and standalone mode)
    if std::env::var("MISH_NO_DAEMON").is_err() {
        let sock = daemon_socket_path();
        if let Ok(stream) = tokio::net::UnixStream::connect(&sock).await {
            return run_shim(stream).await;
        }
    }

    // 1. Load config
    let path = config_path.unwrap_or("~/.config/mish/mish.toml");
    let config = Arc::new(
        load_config(path)
            .map_err(|e| ServerError::Config(e.to_string()))?,
    );

    // 2. Clean stale PID files
    let stale = ShutdownManager::cleanup_stale_pid_files();
    for pid in &stale {
        tracing::warn!("cleaned up stale PID {pid}");
    }

    // 3. Write PID file
    let pid_path = ShutdownManager::current_pid_file_path();
    ShutdownManager::write_pid_file(&pid_path)?;

    // 4. Create server (session created lazily below)
    let server = McpServer::new(config.clone())?;

    // Spawn "main" session creation in the background so the MCP transport
    // loop can start immediately.  The initialize handshake completes in
    // <50 ms; the shell/PTY spawn takes ~5-6 s.  The event loop below
    // awaits this handle before dispatching the first tools/call, so the
    // session is guaranteed ready before any command runs.
    let bg_sm = server.session_manager.clone();
    let session_handle = tokio::spawn(async move {
        if let Err(e) = bg_sm.create_session("main", None).await {
            tracing::error!("failed to create main session: {e}");
        }
    });

    // 5. Audit log: ServerStarted
    let session_id = Uuid::new_v4().to_string();
    let mut audit_logger = AuditLogger::new(&config.audit, &session_id)?;
    audit_logger.log(AuditEntry::new(
        "server".into(),
        "".into(),
        None,
        AuditEvent::ServerStarted,
    ));

    // 6. Install signal handlers + run transport loop
    let shutdown_mgr = ShutdownManager::with_defaults(server.session_manager.clone());
    let shutdown_rx = shutdown_mgr.subscribe();

    let shutdown_handle = tokio::spawn(async move {
        shutdown_mgr.wait_for_shutdown().await;
    });

    // Spawn periodic cleanup task for expired processes and idle sessions.
    let cleanup_pt = server.process_table.clone();
    let cleanup_sm = server.session_manager.clone();
    let cleanup_config = config.clone();
    let mut cleanup_shutdown_rx = shutdown_rx.clone();
    let cleanup_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        let retention = std::time::Duration::from_secs(
            cleanup_config.server.idle_session_timeout_sec,
        );
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    // Clean up expired process entries.
                    {
                        let mut pt = cleanup_pt.lock().await;
                        pt.cleanup_expired(retention);
                    }
                    // Clean up idle sessions.
                    cleanup_sm.cleanup_idle_sessions().await;
                }
                _ = cleanup_shutdown_rx.changed() => {
                    if *cleanup_shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
    });

    let transport = StdioTransport::new();
    let (mut reader, writer) = transport.into_split();
    let mut shutdown_rx_loop = shutdown_rx;
    let mut session_handle = Some(session_handle);
    let mut inflight: JoinSet<()> = JoinSet::new();
    let dispatcher = server.dispatcher.clone();
    let run_result: Result<(), ServerError> = async {
        loop {
            tokio::select! {
                result = reader.read_request() => {
                    match result {
                        Ok(Some(request)) => {
                            // Before the first tools/call, ensure the main
                            // session has finished spawning.  The handshake
                            // (initialize, tools/list) flows through instantly;
                            // only actual tool invocations block here.
                            if request.method == "tools/call" {
                                if let Some(handle) = session_handle.take() {
                                    let _ = handle.await;
                                }
                            }
                            let d = dispatcher.clone();
                            let w = writer.clone();
                            inflight.spawn(async move {
                                if let Some(response) = d.dispatch(request).await {
                                    if let Err(e) = w.write_response(response).await {
                                        tracing::error!("write_response error: {e}");
                                    }
                                }
                            });
                        }
                        Ok(None) => break,
                        Err(TransportError::Eof) => break,
                        Err(e) => return Err(ServerError::Transport(e.to_string())),
                    }
                }
                // Reap completed tasks (ignore results — errors logged inside)
                Some(_) = inflight.join_next(), if !inflight.is_empty() => {}
                _ = shutdown_rx_loop.changed() => {
                    if *shutdown_rx_loop.borrow() {
                        break;
                    }
                }
            }
        }

        // Drain in-flight tasks with a 5s timeout
        if !inflight.is_empty() {
            let drain = async {
                while inflight.join_next().await.is_some() {}
            };
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), drain).await;
            inflight.abort_all();
        }

        Ok(())
    }.await;

    // Shut down cleanly based on how we exited the transport loop.
    cleanup_handle.abort();
    if *shutdown_rx_loop.borrow() {
        // Signal-triggered exit: wait for the graceful shutdown sequence
        // (SIGTERM → drain → SIGKILL) that's already in progress.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            shutdown_handle,
        ).await;
    } else {
        // Stdin EOF: MCP client disconnected. Abort the shutdown task
        // (it was waiting for signals that will never arrive).
        // DO NOT kill children — they may be long-running agents that
        // should survive transport reconnection (BUG-004).
        shutdown_handle.abort();
    }

    // Propagate any error from the run loop.
    run_result?;

    // 7. Cleanup
    audit_logger.log(AuditEntry::new(
        "server".into(),
        "".into(),
        None,
        AuditEvent::ServerShutdown,
    ));
    audit_logger.flush();
    ShutdownManager::remove_pid_file(&pid_path)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Daemon mode — shared process table over Unix socket
// ---------------------------------------------------------------------------

/// Shim: proxy stdio↔daemon socket. Claude Code talks to us on stdio,
/// we forward everything to the daemon and relay responses back.
async fn run_shim(stream: tokio::net::UnixStream) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("[mish] connected to daemon, proxying");
    let (sock_read, sock_write) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut sock_writer = tokio::io::BufWriter::new(sock_write);
    let mut sock_reader = tokio::io::BufReader::new(sock_read);

    let up = tokio::io::copy(&mut stdin, &mut sock_writer);
    let down = tokio::io::copy(&mut sock_reader, &mut stdout);

    // Run both directions concurrently. When either side closes, we're done.
    tokio::select! {
        r = up => { r?; }
        r = down => { r?; }
    }
    Ok(())
}

/// Default daemon socket path.
fn daemon_socket_path() -> std::path::PathBuf {
    let base = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
    base.join(".local/share/mish/mish.sock")
}

/// Run the daemon: listen on Unix socket, accept connections, share one McpServer.
pub async fn run_daemon(socket_path: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let sock_path = socket_path
        .map(std::path::PathBuf::from)
        .unwrap_or_else(daemon_socket_path);

    // Clean stale socket
    if sock_path.exists() {
        std::fs::remove_file(&sock_path)?;
    }
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = tokio::net::UnixListener::bind(&sock_path)?;
    eprintln!("[mish daemon] listening on {}", sock_path.display());

    // Shared server — all connections share the same process table
    let config = Arc::new(
        load_config("~/.config/mish/mish.toml")
            .map_err(|e| ServerError::Config(e.to_string()))?,
    );
    let server = Arc::new(McpServer::new(config.clone())?);

    // Create main session
    let sm = server.session_manager.clone();
    tokio::spawn(async move {
        if let Err(e) = sm.create_session("main", None).await {
            tracing::error!("failed to create main session: {e}");
        }
    });

    // Accept loop
    loop {
        let (stream, _addr) = listener.accept().await?;
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            if let Err(e) = handle_daemon_connection(stream, &server).await {
                tracing::warn!("daemon connection error: {e}");
            }
        });
    }
}

/// Handle a single daemon client connection.
async fn handle_daemon_connection(
    stream: tokio::net::UnixStream,
    server: &McpServer,
) -> Result<(), Box<dyn std::error::Error>> {
    let (read_half, write_half) = stream.into_split();
    let reader = tokio::io::BufReader::new(read_half);
    let transport = StdioTransport::with_io(reader, write_half);
    let (mut reader, writer) = transport.into_split();
    let dispatcher = server.dispatcher.clone();
    let mut inflight: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    loop {
        tokio::select! {
            result = reader.read_request() => {
                match result {
                    Ok(Some(request)) => {
                        let d = dispatcher.clone();
                        let w = writer.clone();
                        inflight.spawn(async move {
                            if let Some(response) = d.dispatch(request).await {
                                if let Err(e) = w.write_response(response).await {
                                    tracing::error!("daemon write error: {e}");
                                }
                            }
                        });
                    }
                    Ok(None) | Err(TransportError::Eof) => break,
                    Err(e) => {
                        tracing::warn!("daemon read error: {e}");
                        break;
                    }
                }
            }
            Some(_) = inflight.join_next(), if !inflight.is_empty() => {}
        }
    }

    // Drain in-flight
    if !inflight.is_empty() {
        let drain = async { while inflight.join_next().await.is_some() {} };
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), drain).await;
        inflight.abort_all();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_config;
    use serial_test::serial;

    fn test_config() -> Arc<MishConfig> {
        Arc::new(default_config())
    }

    /// Extract compact text from a content-wrapped MCP tools/call response.
    fn extract_tool_text(parsed: &serde_json::Value) -> String {
        parsed["result"]["content"][0]["text"].as_str()
            .expect("tools/call response should have result.content[0].text")
            .to_string()
    }

    fn make_transport(
        input: &str,
    ) -> StdioTransport<
        tokio::io::BufReader<std::io::Cursor<Vec<u8>>>,
        Vec<u8>,
    > {
        let reader =
            tokio::io::BufReader::new(std::io::Cursor::new(input.as_bytes().to_vec()));
        let writer = Vec::new();
        StdioTransport::with_io(reader, writer)
    }

    fn get_output<R>(
        transport: StdioTransport<R, Vec<u8>>,
    ) -> String {
        let (_reader, writer) = transport.into_parts();
        String::from_utf8(writer).unwrap()
    }

    // ── Test 1: McpServer::new() succeeds with default config ──

    #[test]
    fn test_server_new() {
        let server = McpServer::new(test_config());
        assert!(server.is_ok());
    }

    // ── Test 2: Server processes initialize request ──

    #[tokio::test]
    async fn test_server_initialize() {
        let server = McpServer::new(test_config()).unwrap();
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#,
            "\n",
        );
        let mut transport = make_transport(input);

        server.run(&mut transport).await.unwrap();

        let output = get_output(transport);
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["result"]["serverInfo"]["name"], "mish");
        assert_eq!(parsed["result"]["protocolVersion"], "2024-11-05");
    }

    // ── Test 3: Server exits cleanly on empty input (EOF) ──

    #[tokio::test]
    async fn test_server_eof() {
        let server = McpServer::new(test_config()).unwrap();
        let mut transport = make_transport("");

        let result = server.run(&mut transport).await;
        assert!(result.is_ok(), "Server should exit cleanly on EOF");
    }

    // ── Test 4: Server processes tools/list and returns 5 tools ──

    #[tokio::test]
    async fn test_server_tools_list() {
        let server = McpServer::new(test_config()).unwrap();
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            "\n",
        );
        let mut transport = make_transport(input);

        server.run(&mut transport).await.unwrap();

        let output = get_output(transport);
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        let tools = parsed["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 5);
    }

    // ── Test 5: Notifications produce no output ──

    #[tokio::test]
    async fn test_server_notification_no_output() {
        let server = McpServer::new(test_config()).unwrap();
        // Include id:null so serde can parse the request struct
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":null,"method":"notifications/initialized"}"#,
            "\n",
        );
        let mut transport = make_transport(input);

        server.run(&mut transport).await.unwrap();

        let output = get_output(transport);
        assert!(
            output.trim().is_empty(),
            "Notification should produce no output, got: {output}"
        );
    }

    // ── Test 6: Multiple requests processed sequentially ──

    #[tokio::test]
    async fn test_server_multiple_requests() {
        let server = McpServer::new(test_config()).unwrap();
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#, "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, "\n",
        );
        let mut transport = make_transport(input);

        server.run(&mut transport).await.unwrap();

        let output = get_output(transport);
        let lines: Vec<&str> = output.trim().lines().collect();
        assert_eq!(lines.len(), 2, "Should have 2 responses");

        let resp1: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(resp1["id"], 1);
        assert_eq!(resp1["result"]["serverInfo"]["name"], "mish");

        let resp2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(resp2["id"], 2);
        assert!(resp2["result"]["tools"].is_array());
    }

    // ── Test 7: Server processes tools/call sh_help ──

    #[tokio::test]
    async fn test_server_tools_call_sh_help() {
        let server = McpServer::new(test_config()).unwrap();
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#, "\n",
            r#"{"jsonrpc":"2.0","id":null,"method":"notifications/initialized"}"#, "\n",
            r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"sh_help","arguments":{}}}"#, "\n",
        );
        let mut transport = make_transport(input);

        server.run(&mut transport).await.unwrap();

        let output = get_output(transport);
        let lines: Vec<&str> = output.trim().lines().collect();
        // 2 responses: initialize + tools/call (notification produces none)
        assert_eq!(lines.len(), 2, "Expected 2 responses, got: {lines:?}");
        let parsed: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert!(parsed["error"].is_null());
        let text = extract_tool_text(&parsed);
        assert!(text.contains("# mish reference card"), "should have reference card: {}", text);
        assert!(text.contains("## tools"), "should have tools section: {}", text);
    }

    // ── Test 8: Unknown tool returns error with process digest ──

    #[tokio::test]
    async fn test_server_unknown_tool_error() {
        let server = McpServer::new(test_config()).unwrap();
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#, "\n",
            r#"{"jsonrpc":"2.0","id":null,"method":"notifications/initialized"}"#, "\n",
            r#"{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"bogus","arguments":{}}}"#, "\n",
        );
        let mut transport = make_transport(input);

        server.run(&mut transport).await.unwrap();

        let output = get_output(transport);
        let lines: Vec<&str> = output.trim().lines().collect();
        assert_eq!(lines.len(), 2, "Expected 2 responses, got: {lines:?}");
        let parsed: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert!(parsed["error"].is_object());
        // With no running processes, error.data may be null (empty digest)
    }

    // ── Test 9: run_with_shutdown exits when shutdown triggered ──

    #[tokio::test]
    async fn test_server_shutdown_signal() {
        let server = McpServer::new(test_config()).unwrap();
        let (tx, rx) = watch::channel(false);

        // Pre-trigger shutdown
        let _ = tx.send(true);

        let mut transport = make_transport("");
        let result = server.run_with_shutdown(&mut transport, rx).await;
        assert!(result.is_ok());
    }

    // ── Test 10: ServerError Display formatting ──

    #[test]
    fn test_server_error_display() {
        let config_err = ServerError::Config("bad config".to_string());
        assert!(format!("{config_err}").contains("bad config"));

        let transport_err = ServerError::Transport("broken pipe".to_string());
        assert!(format!("{transport_err}").contains("broken pipe"));

        let session_err = ServerError::Session("no session".to_string());
        assert!(format!("{session_err}").contains("no session"));

        let io_err = ServerError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "not found",
        ));
        assert!(format!("{io_err}").contains("not found"));
    }

    // ── Test 11: Full-stack sh_run through server ──

    #[tokio::test]
    #[serial(pty)]
    async fn test_server_full_stack_sh_run() {
        let config = test_config();
        let server = McpServer::new(config).unwrap();

        server
            .session_manager
            .create_session("main", Some("/bin/bash"))
            .await
            .expect("should create main session");

        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#, "\n",
            r#"{"jsonrpc":"2.0","id":null,"method":"notifications/initialized"}"#, "\n",
            r#"{"jsonrpc":"2.0","id":100,"method":"tools/call","params":{"name":"sh_run","arguments":{"cmd":"echo mcp_server_test","timeout":5}}}"#, "\n",
        );
        let mut transport = make_transport(input);

        server.run(&mut transport).await.unwrap();

        let output = get_output(transport);
        let lines: Vec<&str> = output.trim().lines().collect();
        assert_eq!(lines.len(), 2, "Expected 2 responses, got: {lines:?}");
        let parsed: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert!(
            parsed["error"].is_null(),
            "Expected success, got: {parsed}"
        );
        let text = extract_tool_text(&parsed);
        assert!(text.contains("exit:0"), "should show exit:0: {}", text);
        assert!(text.contains("mcp_server_test"), "should contain output: {}", text);

        server.session_manager.close_all().await;
    }

    // ── Test 12: Full MCP lifecycle: init → notification → tools/list → tools/call ──

    #[tokio::test]
    #[serial(pty)]
    async fn test_server_full_mcp_lifecycle() {
        let config = test_config();
        let server = McpServer::new(config).unwrap();

        server
            .session_manager
            .create_session("main", Some("/bin/bash"))
            .await
            .expect("should create main session");

        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#, "\n",
            r#"{"jsonrpc":"2.0","id":null,"method":"notifications/initialized"}"#, "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"sh_help","arguments":{}}}"#, "\n",
        );
        let mut transport = make_transport(input);

        server.run(&mut transport).await.unwrap();

        let output = get_output(transport);
        let lines: Vec<&str> = output.trim().lines().collect();
        // 3 responses: initialize, tools/list, tools/call (notification has none)
        assert_eq!(
            lines.len(),
            3,
            "Expected 3 responses (notification has none), got: {lines:?}"
        );

        let resp1: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(resp1["id"], 1);
        assert_eq!(resp1["result"]["serverInfo"]["name"], "mish");

        let resp2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(resp2["id"], 2);
        assert_eq!(resp2["result"]["tools"].as_array().unwrap().len(), 5);

        let resp3: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(resp3["id"], 3);
        let text3 = extract_tool_text(&resp3);
        assert!(text3.contains("# mish reference card"), "should have reference card: {}", text3);

        server.session_manager.close_all().await;
    }
}
