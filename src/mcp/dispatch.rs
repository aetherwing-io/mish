//! MCP Dispatch — routes JSON-RPC requests to tool handlers.
//!
//! Implements the MCP protocol lifecycle: initialize, notifications/initialized,
//! tools/list, tools/call. Attaches process table digest to every tool response.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::json;
use tokio::sync::Mutex as TokioMutex;
use uuid::Uuid;

use crate::audit::logger::{AuditEntry, AuditEvent, AuditLogger};
use crate::config::MishConfig;
use crate::core::grammar::Grammar;
use crate::mcp::types::{
    InitializeResult, JsonRpcError, JsonRpcRequest, JsonRpcResponse, ProcessDigestEntry,
    ServerCapabilities, ServerInfo, ShInteractParams, ShRunParams, ShSpawnParams,
    ToolDefinition, ToolsCapability, ERR_INTERNAL, ERR_INVALID_PARAMS, ERR_INVALID_REQUEST,
    ERR_METHOD_NOT_FOUND,
};
use crate::process::table::{DigestMode, ProcessTable};
use crate::router::categories::{CategoriesConfig, DangerousPattern};
use crate::session::manager::SessionManager;
use crate::tools::{sh_help, sh_interact, sh_run, sh_session, sh_spawn};
use crate::tools::sh_session::AuditContext;

/// The MCP server dispatcher.
pub struct McpDispatcher {
    session_manager: Arc<SessionManager>,
    process_table: Arc<TokioMutex<ProcessTable>>,
    config: Arc<MishConfig>,
    grammars: HashMap<String, Grammar>,
    categories_config: CategoriesConfig,
    dangerous_patterns: Vec<DangerousPattern>,
    initialized: std::sync::atomic::AtomicBool,
    audit_logger: Arc<TokioMutex<AuditLogger>>,
    session_id: String,
}

impl McpDispatcher {
    pub fn new(
        session_manager: Arc<SessionManager>,
        process_table: Arc<TokioMutex<ProcessTable>>,
        config: Arc<MishConfig>,
        grammars: HashMap<String, Grammar>,
        categories_config: CategoriesConfig,
        dangerous_patterns: Vec<DangerousPattern>,
    ) -> Self {
        let dispatch_session_id = Uuid::new_v4().to_string();
        let audit_logger = AuditLogger::new(&config.audit, &dispatch_session_id)
            .expect("AuditLogger::new should not fail (gracefully degrades to disabled)");
        Self {
            session_manager,
            process_table,
            config,
            grammars,
            categories_config,
            dangerous_patterns,
            initialized: std::sync::atomic::AtomicBool::new(false),
            audit_logger: Arc::new(TokioMutex::new(audit_logger)),
            session_id: dispatch_session_id,
        }
    }

    /// Mark the dispatcher as initialized (for testing or after protocol handshake).
    #[cfg(test)]
    pub fn mark_initialized(&self) {
        self.initialized.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Handle a single JSON-RPC request and return a response.
    /// Returns `None` for notifications (no response expected).
    pub async fn dispatch(&self, request: JsonRpcRequest) -> Option<JsonRpcResponse> {
        match request.method.as_str() {
            "initialize" => Some(self.handle_initialize(request)),
            "tools/list" => Some(self.handle_tools_list(request)),
            "tools/call" => {
                if !self.initialized.load(std::sync::atomic::Ordering::Relaxed) {
                    return Some(JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: request.id,
                        result: None,
                        error: Some(JsonRpcError {
                            code: ERR_INVALID_REQUEST,
                            message: "Server not initialized. Send 'initialize' and 'notifications/initialized' before calling tools.".to_string(),
                            data: None,
                        }),
                    });
                }
                Some(self.handle_tools_call(request).await)
            }
            m if m.starts_with("notifications/") => {
                if m == "notifications/initialized" {
                    self.initialized.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                None // Notifications get no response
            }
            _ => Some(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: None,
                error: Some(JsonRpcError {
                    code: ERR_METHOD_NOT_FOUND,
                    message: format!("Method not found: {}", request.method),
                    data: None,
                }),
            }),
        }
    }

    fn handle_initialize(&self, request: JsonRpcRequest) -> JsonRpcResponse {
        let result = InitializeResult {
            protocol_version: "2024-11-05".to_string(),
            capabilities: ServerCapabilities {
                tools: ToolsCapability {
                    list_changed: false,
                },
            },
            server_info: ServerInfo {
                name: "mish".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        };

        JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: request.id,
            result: Some(serde_json::to_value(result).unwrap()),
            error: None,
        }
    }

    fn handle_tools_list(&self, request: JsonRpcRequest) -> JsonRpcResponse {
        let tools = tool_definitions();
        JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: request.id,
            result: Some(json!({ "tools": tools })),
            error: None,
        }
    }

    async fn handle_tools_call(&self, request: JsonRpcRequest) -> JsonRpcResponse {
        let id = request.id.clone();

        // Extract name and arguments from params.
        let params = match request.params {
            Some(p) => p,
            None => {
                return self.error_with_digest(id, ERR_INVALID_PARAMS, "Missing params").await;
            }
        };

        let tool_name = match params.get("name").and_then(|n| n.as_str()) {
            Some(name) => name.to_string(),
            None => {
                return self.error_with_digest(id, ERR_INVALID_PARAMS, "Missing 'name' in params").await;
            }
        };

        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        // Extract cmd for audit logging before arguments are consumed.
        let audit_cmd = arguments.get("cmd").and_then(|v| v.as_str()).map(String::from);

        // Dispatch to tool handler.
        let tool_result = match tool_name.as_str() {
            "sh_run" => self.dispatch_sh_run(arguments).await,
            "sh_spawn" => self.dispatch_sh_spawn(arguments).await,
            "sh_interact" => self.dispatch_sh_interact(arguments).await,
            "sh_session" => self.dispatch_sh_session(arguments).await,
            "sh_help" => self.dispatch_sh_help().await,
            _ => {
                return self.error_with_digest(
                    id,
                    ERR_METHOD_NOT_FOUND,
                    format!("Unknown tool: {tool_name}"),
                ).await;
            }
        };

        // Audit log the tool call.
        let audit_exit_code = match &tool_result {
            Ok((ref result_value, _)) => result_value.get("exit_code").and_then(|v| v.as_i64()).map(|v| v as i32),
            Err(_) => None,
        };
        {
            let mut logger = self.audit_logger.lock().await;
            logger.log(AuditEntry::new(
                "server".into(),
                tool_name.clone(),
                audit_cmd.clone(),
                AuditEvent::ToolCall {
                    tool_name: tool_name.clone(),
                    cmd: audit_cmd,
                    exit_code: audit_exit_code,
                },
            ));
        }

        match tool_result {
            Ok((result_value, digest)) => {
                // Format as compact text (symbol-prefixed, token-efficient).
                let text = crate::mcp::format::format_tool_response(
                    &tool_name,
                    &result_value,
                    &digest,
                );
                JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id,
                    result: Some(json!({
                        "content": [
                            { "type": "text", "text": text }
                        ]
                    })),
                    error: None,
                }
            }
            Err((code, message)) => {
                self.error_with_digest(id, code, message).await
            }
        }
    }

    /// Build an error response with process digest attached in error.data.
    async fn error_with_digest(
        &self,
        id: serde_json::Value,
        code: i32,
        message: impl Into<String>,
    ) -> JsonRpcResponse {
        let digest = {
            let mut pt = self.process_table.lock().await;
            pt.digest(DigestMode::Full)
        };

        let digest_text = crate::mcp::format::format_digest(&digest);
        let data = if digest_text.is_empty() {
            None
        } else {
            Some(serde_json::Value::String(digest_text))
        };

        JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data,
            }),
        }
    }

    // ── Tool dispatch helpers ──

    async fn dispatch_sh_run(
        &self,
        arguments: serde_json::Value,
    ) -> Result<(serde_json::Value, Vec<ProcessDigestEntry>), (i32, String)> {
        let params: ShRunParams = serde_json::from_value(arguments)
            .map_err(|e| (ERR_INVALID_PARAMS, format!("Invalid sh_run params: {e}")))?;

        sh_run::handle(
            params,
            &self.session_manager,
            &self.process_table,
            &self.config,
            &self.grammars,
            &self.categories_config,
            &self.dangerous_patterns,
        )
            .await
            .map_err(|e| (e.code, e.message))
    }

    async fn dispatch_sh_spawn(
        &self,
        arguments: serde_json::Value,
    ) -> Result<(serde_json::Value, Vec<ProcessDigestEntry>), (i32, String)> {
        let params: ShSpawnParams = serde_json::from_value(arguments)
            .map_err(|e| (ERR_INVALID_PARAMS, format!("Invalid sh_spawn params: {e}")))?;

        // Validate wait_for regex early (before locking) to fail fast.
        if let Some(ref wait_pattern) = params.wait_for {
            regex::Regex::new(&format!("(?i){}", wait_pattern)).map_err(|e| {
                (ERR_INVALID_PARAMS, format!("invalid wait_for regex '{}': {}", wait_pattern, e))
            })?;
        }

        // Phase 1: Lock table, register process, clone spool Arc.
        let spawn_setup = {
            let mut pt = self.process_table.lock().await;
            sh_spawn::setup(params, &self.session_manager, &mut pt, &self.config)
                .await
                .map_err(|e| (e.code, e.message))?
            // Lock dropped here
        };

        // Phase 2: Wait for match WITHOUT holding the table lock.
        let response = sh_spawn::wait_for_match(&spawn_setup, &self.session_manager).await;
        let result = serde_json::to_value(&response)
            .map_err(|e| (ERR_INTERNAL, format!("Failed to serialize sh_spawn result: {e}")))?;

        // Phase 3: Re-acquire lock for digest only.
        let digest = {
            let mut pt = self.process_table.lock().await;
            pt.digest(DigestMode::Full)
        };

        Ok((result, digest))
    }

    async fn dispatch_sh_interact(
        &self,
        arguments: serde_json::Value,
    ) -> Result<(serde_json::Value, Vec<ProcessDigestEntry>), (i32, String)> {
        let params: ShInteractParams = serde_json::from_value(arguments)
            .map_err(|e| (ERR_INVALID_PARAMS, format!("Invalid sh_interact params: {e}")))?;

        let mut pt = self.process_table.lock().await;
        let result = sh_interact::handle(params, &self.session_manager, &mut pt)
            .await
            .map_err(|e| (e.code, e.message))?;
        let digest = pt.digest(DigestMode::Full);
        Ok((result, digest))
    }

    async fn dispatch_sh_session(
        &self,
        arguments: serde_json::Value,
    ) -> Result<(serde_json::Value, Vec<ProcessDigestEntry>), (i32, String)> {
        let params: sh_session::ShSessionParams = serde_json::from_value(arguments)
            .map_err(|e| (ERR_INVALID_PARAMS, format!("Invalid sh_session params: {e}")))?;

        let audit_ctx = AuditContext {
            config: &self.config.audit,
            session_id: &self.session_id,
        };

        let mut pt = self.process_table.lock().await;
        let result = sh_session::handle(params, &self.session_manager, &pt, Some(&audit_ctx))
            .await
            .map_err(|e| (e.code, e.message))?;
        let digest = pt.digest(DigestMode::Full);
        Ok((result, digest))
    }

    async fn dispatch_sh_help(
        &self,
    ) -> Result<(serde_json::Value, Vec<ProcessDigestEntry>), (i32, String)> {
        let mut pt = self.process_table.lock().await;
        let result = sh_help::handle(&self.config, &pt, &self.session_manager, None)
            .await
            .map_err(|e| (e.error_code(), e.to_string()))?;
        let result_value = serde_json::to_value(result)
            .map_err(|e| (ERR_INTERNAL, format!("Failed to serialize sh_help result: {e}")))?;
        let digest = pt.digest(DigestMode::Full);
        Ok((result_value, digest))
    }
}

/// Return the 5 MCP tool definitions with JSON Schema inputSchema.
fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "sh_run".to_string(),
            description: "Execute a command synchronously and return structured output. Commands persist CWD and environment between calls.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cmd": { "type": "string", "description": "Command to execute" },
                    "timeout": { "type": "integer", "description": "Seconds before kill", "default": 300 },
                    "watch": { "type": "string", "description": "Regex or @preset to filter output" },
                    "unmatched": { "type": "string", "enum": ["keep", "drop"], "description": "Handle non-matching lines when watch is set", "default": "keep" }
                },
                "required": ["cmd"]
            }),
        },
        ToolDefinition {
            name: "sh_spawn".to_string(),
            description: "Start a background process with alias tracking. Optionally wait for a regex match before returning.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "alias": { "type": "string", "description": "Unique name for this process" },
                    "cmd": { "type": "string", "description": "Command to execute" },
                    "wait_for": { "type": "string", "description": "Regex to match before returning success" },
                    "timeout": { "type": "integer", "description": "Seconds to wait", "default": 300 }
                },
                "required": ["alias", "cmd"]
            }),
        },
        ToolDefinition {
            name: "sh_interact".to_string(),
            description: "Interact with a running background process: send input, read output, signal, or kill.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "alias": { "type": "string", "description": "Target process alias" },
                    "action": { "type": "string", "enum": ["send_input", "read_tail", "read_full", "send_signal", "kill", "status"], "description": "Action to perform" },
                    "input": { "type": "string", "description": "For send_input: string to write (include \\n for enter). For send_signal: signal name (SIGINT, SIGTERM, etc.)" },
                    "lines": { "type": "integer", "description": "For read_tail: number of lines", "default": 50 }
                },
                "required": ["alias", "action"]
            }),
        },
        ToolDefinition {
            name: "sh_session".to_string(),
            description: "Manage shell sessions: create, list, close, or read audit logs.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["create", "list", "close", "audit"], "description": "Action to perform" },
                    "name": { "type": "string", "description": "Session name (required for create and close)" },
                    "shell": { "type": "string", "description": "Shell path for create (defaults to $SHELL or /bin/sh)" },
                    "last": { "type": "integer", "description": "For audit: return only the last N command records" },
                    "format": { "type": "string", "enum": ["summary"], "description": "For audit: 'summary' returns session aggregate metrics" }
                },
                "required": ["action"]
            }),
        },
        ToolDefinition {
            name: "sh_help".to_string(),
            description: "Return a reference card with all tools, watch presets, and resource usage.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;

    fn test_config() -> Arc<MishConfig> {
        Arc::new(MishConfig::default())
    }

    fn test_dispatcher() -> McpDispatcher {
        let config = test_config();
        let sm = Arc::new(SessionManager::new(config.clone()));
        let pt = Arc::new(TokioMutex::new(ProcessTable::new(&config)));
        let rc = crate::config_loader::default_runtime_config();
        let d = McpDispatcher::new(sm, pt, config, rc.grammars, rc.categories_config, rc.dangerous_patterns);
        d.mark_initialized();
        d
    }

    /// Extract the compact text from a content-wrapped MCP tools/call response.
    fn extract_tool_text(result: &serde_json::Value) -> String {
        result["content"][0]["text"].as_str()
            .expect("tools/call response should have content[0].text")
            .to_string()
    }

    fn make_request(id: serde_json::Value, method: &str, params: Option<serde_json::Value>) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
        }
    }

    // ── Test 1: Initialize returns correct server capabilities ──

    #[tokio::test]
    async fn initialize_returns_capabilities() {
        let dispatcher = test_dispatcher();
        let req = make_request(
            json!(1),
            "initialize",
            Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test-client" }
            })),
        );

        let resp = dispatcher.dispatch(req).await.unwrap();
        assert_eq!(resp.jsonrpc, "2.0");
        assert_eq!(resp.id, json!(1));
        assert!(resp.error.is_none());

        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert_eq!(result["serverInfo"]["name"], "mish");
        assert!(result["serverInfo"]["version"].is_string());
        assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
    }

    // ── Test 2: notifications/initialized returns None ──

    #[tokio::test]
    async fn notifications_initialized_returns_none() {
        let dispatcher = test_dispatcher();
        let req = make_request(
            serde_json::Value::Null,
            "notifications/initialized",
            None,
        );

        let resp = dispatcher.dispatch(req).await;
        assert!(resp.is_none(), "Notifications should not produce a response");
    }

    // ── Test 2b: tools/call before initialization returns error ──

    #[tokio::test]
    async fn tools_call_before_initialized_returns_error() {
        let config = test_config();
        let sm = Arc::new(SessionManager::new(config.clone()));
        let pt = Arc::new(TokioMutex::new(ProcessTable::new(&config)));
        let rc = crate::config_loader::default_runtime_config();
        // Intentionally do NOT mark_initialized.
        let dispatcher = McpDispatcher::new(sm, pt, config, rc.grammars, rc.categories_config, rc.dangerous_patterns);

        let req = make_request(
            json!(99),
            "tools/call",
            Some(json!({ "name": "sh_help", "arguments": {} })),
        );

        let resp = dispatcher.dispatch(req).await.unwrap();
        let err = resp.error.unwrap();
        assert_eq!(err.code, ERR_INVALID_REQUEST);
        assert!(err.message.contains("not initialized"));
    }

    // ── Test 3: Unknown method returns method-not-found ──

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let dispatcher = test_dispatcher();
        let req = make_request(json!(99), "bogus/method", None);

        let resp = dispatcher.dispatch(req).await.unwrap();
        assert_eq!(resp.id, json!(99));
        let err = resp.error.unwrap();
        assert_eq!(err.code, ERR_METHOD_NOT_FOUND);
        assert!(err.message.contains("bogus/method"));
    }

    // ── Test 4: tools/list returns 5 tool definitions ──

    #[tokio::test]
    async fn tools_list_returns_five_tools() {
        let dispatcher = test_dispatcher();
        let req = make_request(json!(2), "tools/list", None);

        let resp = dispatcher.dispatch(req).await.unwrap();
        assert!(resp.error.is_none());

        let result = resp.result.unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 5);

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"sh_run"));
        assert!(names.contains(&"sh_spawn"));
        assert!(names.contains(&"sh_interact"));
        assert!(names.contains(&"sh_session"));
        assert!(names.contains(&"sh_help"));

        // Each tool has description and inputSchema
        for tool in tools {
            assert!(tool["description"].is_string());
            assert!(tool["inputSchema"].is_object());
        }
    }

    // ── Test 5: sh_run schema has correct required fields ──

    #[tokio::test]
    async fn sh_run_schema_has_cmd_required() {
        let dispatcher = test_dispatcher();
        let req = make_request(json!(3), "tools/list", None);

        let resp = dispatcher.dispatch(req).await.unwrap();
        let result = resp.result.unwrap();
        let tools = result["tools"].as_array().unwrap();
        let sh_run = tools.iter().find(|t| t["name"] == "sh_run").unwrap();

        let schema = &sh_run["inputSchema"];
        assert_eq!(schema["type"], "object");
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("cmd")));
        assert!(schema["properties"]["cmd"]["type"] == "string");
    }

    // ── Test 6: tools/call with unknown tool name ──

    #[tokio::test]
    async fn tools_call_unknown_tool_returns_method_not_found() {
        let dispatcher = test_dispatcher();
        let req = make_request(
            json!(10),
            "tools/call",
            Some(json!({ "name": "sh_bogus", "arguments": {} })),
        );

        let resp = dispatcher.dispatch(req).await.unwrap();
        let err = resp.error.unwrap();
        assert_eq!(err.code, ERR_METHOD_NOT_FOUND);
        assert!(err.message.contains("sh_bogus"));
    }

    // ── Test 7: tools/call with sh_help returns compact text ──

    #[tokio::test]
    async fn tools_call_sh_help_returns_compact_text() {
        let dispatcher = test_dispatcher();
        let req = make_request(
            json!(20),
            "tools/call",
            Some(json!({ "name": "sh_help", "arguments": {} })),
        );

        let resp = dispatcher.dispatch(req).await.unwrap();
        assert!(resp.error.is_none(), "Expected success, got error: {:?}", resp.error);

        let result = resp.result.unwrap();
        let text = extract_tool_text(&result);
        assert!(text.contains("# mish reference card"), "should have reference card: {}", text);
        assert!(text.contains("## tools"), "should have tools section: {}", text);
        assert!(text.contains("sh_run"), "should list sh_run: {}", text);
    }

    // ── Test 8: tools/call with missing name returns invalid-params ──

    #[tokio::test]
    async fn tools_call_missing_name_returns_invalid_params() {
        let dispatcher = test_dispatcher();
        let req = make_request(
            json!(30),
            "tools/call",
            Some(json!({ "arguments": {} })),
        );

        let resp = dispatcher.dispatch(req).await.unwrap();
        let err = resp.error.unwrap();
        assert_eq!(err.code, ERR_INVALID_PARAMS);
    }

    // ── Test 9: Error responses include digest in data (or None if empty) ──

    #[tokio::test]
    async fn error_responses_include_process_digest() {
        let dispatcher = test_dispatcher();
        let req = make_request(
            json!(40),
            "tools/call",
            Some(json!({ "name": "sh_bogus", "arguments": {} })),
        );

        let resp = dispatcher.dispatch(req).await.unwrap();
        let err = resp.error.as_ref().unwrap();
        // With no processes running, data is None (empty digest).
        // If processes were running, data would be a string like "[procs] ..."
        // Either way, we just check it doesn't crash.
        if let Some(data) = &err.data {
            assert!(data.is_string(), "digest data should be text: {:?}", data);
        }
    }

    // ── Test 10: tools/call sh_run executes a real command ──

    #[tokio::test]
    #[serial(pty)]
    async fn tools_call_sh_run_executes_command() {
        let config = test_config();
        let sm = Arc::new(SessionManager::new(config.clone()));
        sm.create_session("main", Some("/bin/bash"))
            .await
            .expect("should create main session");
        let pt = Arc::new(TokioMutex::new(ProcessTable::new(&config)));
        let rc = crate::config_loader::default_runtime_config();
        let dispatcher = McpDispatcher::new(sm.clone(), pt, config, rc.grammars, rc.categories_config, rc.dangerous_patterns);
        dispatcher.mark_initialized();

        let req = make_request(
            json!(50),
            "tools/call",
            Some(json!({ "name": "sh_run", "arguments": { "cmd": "echo hello_dispatch", "timeout": 5 } })),
        );

        let resp = dispatcher.dispatch(req).await.unwrap();
        assert!(resp.error.is_none(), "Expected success, got error: {:?}", resp.error);

        let result = resp.result.unwrap();
        let text = extract_tool_text(&result);
        assert!(text.contains("exit:0"), "should show exit:0: {}", text);
        assert!(text.contains("hello_dispatch"), "should contain echo output: {}", text);

        sm.close_all().await;
    }

    // ── Test 11: tools/call sh_session list works ──

    #[tokio::test]
    #[serial(pty)]
    async fn tools_call_sh_session_list() {
        let config = test_config();
        let sm = Arc::new(SessionManager::new(config.clone()));
        sm.create_session("main", Some("/bin/bash"))
            .await
            .expect("should create main session");
        let pt = Arc::new(TokioMutex::new(ProcessTable::new(&config)));
        let rc = crate::config_loader::default_runtime_config();
        let dispatcher = McpDispatcher::new(sm.clone(), pt, config, rc.grammars, rc.categories_config, rc.dangerous_patterns);
        dispatcher.mark_initialized();

        let req = make_request(
            json!(60),
            "tools/call",
            Some(json!({ "name": "sh_session", "arguments": { "action": "list" } })),
        );

        let resp = dispatcher.dispatch(req).await.unwrap();
        assert!(resp.error.is_none(), "Expected success, got error: {:?}", resp.error);

        let result = resp.result.unwrap();
        let text = extract_tool_text(&result);
        assert!(text.contains("+ session list"), "should have session list header: {}", text);
        assert!(text.contains("main"), "should list main session: {}", text);

        sm.close_all().await;
    }

    // ── Test 12: String request id is preserved ──

    #[tokio::test]
    async fn string_request_id_preserved() {
        let dispatcher = test_dispatcher();
        let req = make_request(json!("abc-123"), "tools/list", None);

        let resp = dispatcher.dispatch(req).await.unwrap();
        assert_eq!(resp.id, json!("abc-123"));
    }

    // ── Test 13: tools/call with invalid JSON params for sh_run ──

    #[tokio::test]
    async fn tools_call_invalid_params_returns_error() {
        let dispatcher = test_dispatcher();
        let req = make_request(
            json!(70),
            "tools/call",
            Some(json!({ "name": "sh_run", "arguments": { "not_a_field": true } })),
        );

        let resp = dispatcher.dispatch(req).await.unwrap();
        let err = resp.error.unwrap();
        assert_eq!(err.code, ERR_INVALID_PARAMS);
        // With no running processes, error.data is None (empty digest)
        // or a string if processes were running.
    }

    // ── Test 14: sh_interact schema actions match handler ──

    #[test]
    fn sh_interact_schema_actions_match_handler() {
        let tools = tool_definitions();
        let sh_interact = tools.iter().find(|t| t.name == "sh_interact").unwrap();
        let schema_actions: Vec<&str> = sh_interact.input_schema["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();

        // These are the exact actions the handler accepts (sh_interact::handle match arms).
        let handler_actions = ["send_input", "read_tail", "read_full", "send_signal", "kill", "status"];

        for action in &handler_actions {
            assert!(
                schema_actions.contains(action),
                "sh_interact handler action '{action}' missing from schema enum: {schema_actions:?}"
            );
        }
        for action in &schema_actions {
            assert!(
                handler_actions.contains(action),
                "sh_interact schema enum '{action}' not handled: {handler_actions:?}"
            );
        }
    }

    // ── Test 15: sh_session schema actions match handler ──

    #[test]
    fn sh_session_schema_actions_match_handler() {
        let tools = tool_definitions();
        let sh_session = tools.iter().find(|t| t.name == "sh_session").unwrap();
        let schema_actions: Vec<&str> = sh_session.input_schema["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();

        // These are the exact actions the handler accepts (sh_session::handle match arms).
        let handler_actions = ["create", "list", "close", "audit"];

        for action in &handler_actions {
            assert!(
                schema_actions.contains(action),
                "sh_session handler action '{action}' missing from schema enum: {schema_actions:?}"
            );
        }
        for action in &schema_actions {
            assert!(
                handler_actions.contains(action),
                "sh_session schema enum '{action}' not handled: {handler_actions:?}"
            );
        }
    }

    // ── Test 16: sh_session schema has name and shell properties ──

    #[test]
    fn sh_session_schema_has_name_and_shell() {
        let tools = tool_definitions();
        let sh_session = tools.iter().find(|t| t.name == "sh_session").unwrap();
        let props = &sh_session.input_schema["properties"];

        assert!(props["name"].is_object(), "sh_session schema should have 'name' property");
        assert!(props["shell"].is_object(), "sh_session schema should have 'shell' property");
        assert_eq!(props["name"]["type"], "string");
        assert_eq!(props["shell"]["type"], "string");
    }

    // ── Test 17: sh_spawn wait_for does NOT block concurrent sh_help ──

    #[tokio::test]
    #[serial(pty)]
    async fn sh_spawn_wait_for_does_not_block_sh_help() {
        let config = test_config();
        let sm = Arc::new(SessionManager::new(config.clone()));
        sm.create_session("main", Some("/bin/bash"))
            .await
            .expect("should create main session");
        let pt = Arc::new(TokioMutex::new(ProcessTable::new(&config)));
        let rc = crate::config_loader::default_runtime_config();
        let dispatcher = Arc::new({
            let d = McpDispatcher::new(
                sm.clone(), pt, config, rc.grammars, rc.categories_config, rc.dangerous_patterns,
            );
            d.mark_initialized();
            d
        });

        // Spawn a command with wait_for that will NOT match (2s timeout).
        let d1 = dispatcher.clone();
        let spawn_handle = tokio::spawn(async move {
            let req = make_request(
                json!(100),
                "tools/call",
                Some(json!({
                    "name": "sh_spawn",
                    "arguments": {
                        "alias": "slow",
                        "cmd": "echo no_match_here",
                        "wait_for": "will_never_match_this_pattern",
                        "timeout": 2
                    }
                })),
            );
            d1.dispatch(req).await
        });

        // Give spawn a moment to acquire the lock and enter its poll loop.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Now call sh_help — this should complete quickly, not wait 2s.
        let d2 = dispatcher.clone();
        let start = std::time::Instant::now();
        let help_req = make_request(
            json!(101),
            "tools/call",
            Some(json!({ "name": "sh_help", "arguments": {} })),
        );
        let help_resp = d2.dispatch(help_req).await.unwrap();
        let help_elapsed = start.elapsed();

        assert!(
            help_resp.error.is_none(),
            "sh_help should succeed, got: {:?}",
            help_resp.error
        );
        // sh_help must complete in under 500ms — if the mutex is held for 2s, this fails.
        assert!(
            help_elapsed < std::time::Duration::from_millis(500),
            "sh_help took {:?} — blocked by sh_spawn mutex",
            help_elapsed,
        );

        // Wait for spawn to finish (timeout after 2s).
        let spawn_resp = spawn_handle.await.unwrap().unwrap();
        let spawn_text = extract_tool_text(&spawn_resp.result.unwrap());
        // Spawn with unmatched wait_for should show ~ (warning) prefix
        assert!(spawn_text.starts_with("~ spawned slow"), "spawn should show timeout: {}", spawn_text);

        sm.close_all().await;
    }
}
