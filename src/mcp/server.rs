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

use crate::audit::logger::{AuditEntry, AuditEvent, AuditLogger};
use crate::config::{load_config, MishConfig};
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
    dispatcher: McpDispatcher,
    config: Arc<MishConfig>,
}

impl McpServer {
    /// Create a new MCP server with all components wired together.
    pub fn new(config: Arc<MishConfig>) -> Result<Self, ServerError> {
        let session_manager = Arc::new(SessionManager::new(config.clone()));
        let process_table = Arc::new(TokioMutex::new(ProcessTable::new(&config)));
        let dispatcher = McpDispatcher::new(
            session_manager.clone(),
            process_table.clone(),
            config.clone(),
        );
        Ok(Self {
            session_manager,
            process_table,
            dispatcher,
            config,
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
    // 1. Load config
    let path = config_path.unwrap_or("~/.config/mish/mish.toml");
    let config = Arc::new(
        load_config(path)
            .map_err(|e| ServerError::Config(e.to_string()))?,
    );

    // 2. Clean stale PID files
    let stale = ShutdownManager::cleanup_stale_pid_files();
    for pid in &stale {
        eprintln!("mish: cleaned up stale PID {pid}");
    }

    // 3. Write PID file
    let pid_path = ShutdownManager::current_pid_file_path();
    ShutdownManager::write_pid_file(&pid_path)?;

    // 4. Create server + main session
    let server = McpServer::new(config.clone())?;
    server
        .session_manager
        .create_session("main", None)
        .await
        .map_err(|e| ServerError::Session(e.to_string()))?;

    // 5. Audit log: ServerStarted
    let mut audit_logger = AuditLogger::new(&config.audit)?;
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

    let mut transport = StdioTransport::new();
    let run_result = server
        .run_with_shutdown(&mut transport, shutdown_rx)
        .await;

    // Cancel cleanup task and await shutdown task.
    cleanup_handle.abort();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        shutdown_handle,
    ).await;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_config;

    fn test_config() -> Arc<MishConfig> {
        Arc::new(default_config())
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
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"2024-11-05","capabilities":{},"client_info":{"name":"test"}}}"#,
            "\n",
        );
        let mut transport = make_transport(input);

        server.run(&mut transport).await.unwrap();

        let output = get_output(transport);
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["result"]["server_info"]["name"], "mish");
        assert_eq!(parsed["result"]["protocol_version"], "2024-11-05");
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
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"2024-11-05","capabilities":{},"client_info":{"name":"test"}}}"#, "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, "\n",
        );
        let mut transport = make_transport(input);

        server.run(&mut transport).await.unwrap();

        let output = get_output(transport);
        let lines: Vec<&str> = output.trim().lines().collect();
        assert_eq!(lines.len(), 2, "Should have 2 responses");

        let resp1: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(resp1["id"], 1);
        assert_eq!(resp1["result"]["server_info"]["name"], "mish");

        let resp2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(resp2["id"], 2);
        assert!(resp2["result"]["tools"].is_array());
    }

    // ── Test 7: Server processes tools/call sh_help ──

    #[tokio::test]
    async fn test_server_tools_call_sh_help() {
        let server = McpServer::new(test_config()).unwrap();
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"sh_help","arguments":{}}}"#,
            "\n",
        );
        let mut transport = make_transport(input);

        server.run(&mut transport).await.unwrap();

        let output = get_output(transport);
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert!(parsed["error"].is_null());
        assert!(parsed["result"]["result"]["tools"].is_array());
        assert!(parsed["result"]["processes"].is_array());
    }

    // ── Test 8: Unknown tool returns error with process digest ──

    #[tokio::test]
    async fn test_server_unknown_tool_error() {
        let server = McpServer::new(test_config()).unwrap();
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"bogus","arguments":{}}}"#,
            "\n",
        );
        let mut transport = make_transport(input);

        server.run(&mut transport).await.unwrap();

        let output = get_output(transport);
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert!(parsed["error"].is_object());
        assert!(parsed["error"]["data"]["processes"].is_array());
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
    async fn test_server_full_stack_sh_run() {
        let config = test_config();
        let server = McpServer::new(config).unwrap();

        server
            .session_manager
            .create_session("main", Some("/bin/bash"))
            .await
            .expect("should create main session");

        let input = concat!(
            r#"{"jsonrpc":"2.0","id":100,"method":"tools/call","params":{"name":"sh_run","arguments":{"cmd":"echo mcp_server_test","timeout":5}}}"#,
            "\n",
        );
        let mut transport = make_transport(input);

        server.run(&mut transport).await.unwrap();

        let output = get_output(transport);
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert!(
            parsed["error"].is_null(),
            "Expected success, got: {parsed}"
        );
        assert_eq!(parsed["result"]["result"]["exit_code"], 0);
        assert!(
            parsed["result"]["result"]["output"]
                .as_str()
                .unwrap()
                .contains("mcp_server_test")
        );
        assert!(parsed["result"]["processes"].is_array());

        server.session_manager.close_all().await;
    }

    // ── Test 12: Full MCP lifecycle: init → notification → tools/list → tools/call ──

    #[tokio::test]
    async fn test_server_full_mcp_lifecycle() {
        let config = test_config();
        let server = McpServer::new(config).unwrap();

        server
            .session_manager
            .create_session("main", Some("/bin/bash"))
            .await
            .expect("should create main session");

        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"2024-11-05","capabilities":{},"client_info":{"name":"test"}}}"#, "\n",
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
        assert_eq!(resp1["result"]["server_info"]["name"], "mish");

        let resp2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(resp2["id"], 2);
        assert_eq!(resp2["result"]["tools"].as_array().unwrap().len(), 5);

        let resp3: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(resp3["id"], 3);
        assert!(resp3["result"]["result"]["tools"].is_array());

        server.session_manager.close_all().await;
    }
}
