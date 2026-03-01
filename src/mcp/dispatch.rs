//! MCP Dispatch — routes JSON-RPC requests to tool handlers.
//!
//! Implements the MCP protocol lifecycle: initialize, notifications/initialized,
//! tools/list, tools/call. Attaches process table digest to every tool response.

use std::sync::Arc;

use serde_json::json;
use tokio::sync::Mutex as TokioMutex;

use crate::config::MishConfig;
use crate::mcp::types::{
    InitializeResult, JsonRpcError, JsonRpcRequest, JsonRpcResponse, ProcessDigestEntry,
    ServerCapabilities, ServerInfo, ShInteractParams, ShRunParams, ShSpawnParams,
    ToolDefinition, ToolsCapability, ERR_INTERNAL, ERR_INVALID_PARAMS, ERR_METHOD_NOT_FOUND,
};
use crate::process::table::{DigestMode, ProcessTable};
use crate::session::manager::SessionManager;
use crate::tools::{sh_help, sh_interact, sh_run, sh_session, sh_spawn};

/// The MCP server dispatcher.
pub struct McpDispatcher {
    session_manager: Arc<SessionManager>,
    process_table: Arc<TokioMutex<ProcessTable>>,
    config: Arc<MishConfig>,
    initialized: std::sync::atomic::AtomicBool,
}

impl McpDispatcher {
    pub fn new(
        session_manager: Arc<SessionManager>,
        process_table: Arc<TokioMutex<ProcessTable>>,
        config: Arc<MishConfig>,
    ) -> Self {
        Self {
            session_manager,
            process_table,
            config,
            initialized: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Handle a single JSON-RPC request and return a response.
    /// Returns `None` for notifications (no response expected).
    pub async fn dispatch(&self, request: JsonRpcRequest) -> Option<JsonRpcResponse> {
        match request.method.as_str() {
            "initialize" => Some(self.handle_initialize(request)),
            "tools/list" => Some(self.handle_tools_list(request)),
            "tools/call" => Some(self.handle_tools_call(request).await),
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

        match tool_result {
            Ok((result_value, digest)) => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(json!({
                    "result": result_value,
                    "processes": digest,
                })),
                error: None,
            },
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

        JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: Some(json!({ "processes": digest })),
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

        sh_run::handle(params, &self.session_manager, &self.process_table, &self.config)
            .await
            .map_err(|e| (e.code, e.message))
    }

    async fn dispatch_sh_spawn(
        &self,
        arguments: serde_json::Value,
    ) -> Result<(serde_json::Value, Vec<ProcessDigestEntry>), (i32, String)> {
        let params: ShSpawnParams = serde_json::from_value(arguments)
            .map_err(|e| (ERR_INVALID_PARAMS, format!("Invalid sh_spawn params: {e}")))?;

        let mut pt = self.process_table.lock().await;
        let result = sh_spawn::handle(params, &self.session_manager, &mut pt, &self.config)
            .await
            .map_err(|e| (e.code, e.message))?;
        let digest = pt.digest(DigestMode::Full);
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

        let mut pt = self.process_table.lock().await;
        let result = sh_session::handle(params, &self.session_manager, &pt)
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
            description: "Manage shell sessions: create, list, or close named sessions.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["create", "list", "close"], "description": "Action to perform" },
                    "name": { "type": "string", "description": "Session name (required for create and close)" },
                    "shell": { "type": "string", "description": "Shell path for create (defaults to $SHELL or /bin/sh)" }
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

    fn test_config() -> Arc<MishConfig> {
        Arc::new(MishConfig::default())
    }

    fn test_dispatcher() -> McpDispatcher {
        let config = test_config();
        let sm = Arc::new(SessionManager::new(config.clone()));
        let pt = Arc::new(TokioMutex::new(ProcessTable::new(&config)));
        McpDispatcher::new(sm, pt, config)
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
                "protocol_version": "2024-11-05",
                "capabilities": {},
                "client_info": { "name": "test-client" }
            })),
        );

        let resp = dispatcher.dispatch(req).await.unwrap();
        assert_eq!(resp.jsonrpc, "2.0");
        assert_eq!(resp.id, json!(1));
        assert!(resp.error.is_none());

        let result = resp.result.unwrap();
        assert_eq!(result["protocol_version"], "2024-11-05");
        assert_eq!(result["server_info"]["name"], "mish");
        assert!(result["server_info"]["version"].is_string());
        assert_eq!(result["capabilities"]["tools"]["list_changed"], false);
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

    // ── Test 7: tools/call with sh_help returns result + processes ──

    #[tokio::test]
    async fn tools_call_sh_help_returns_result_with_processes() {
        let dispatcher = test_dispatcher();
        let req = make_request(
            json!(20),
            "tools/call",
            Some(json!({ "name": "sh_help", "arguments": {} })),
        );

        let resp = dispatcher.dispatch(req).await.unwrap();
        assert!(resp.error.is_none(), "Expected success, got error: {:?}", resp.error);

        let result = resp.result.unwrap();
        // sh_help returns tools, watch_presets, etc.
        assert!(result["result"]["tools"].is_array());
        // Every tools/call response has a processes field
        assert!(result["processes"].is_array());
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

    // ── Test 9: Error responses still include processes digest ──

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
        // Error data should contain processes array
        let data = err.data.as_ref().unwrap();
        assert!(data["processes"].is_array());
    }

    // ── Test 10: tools/call sh_run executes a real command ──

    #[tokio::test]
    async fn tools_call_sh_run_executes_command() {
        let config = test_config();
        let sm = Arc::new(SessionManager::new(config.clone()));
        sm.create_session("main", Some("/bin/bash"))
            .await
            .expect("should create main session");
        let pt = Arc::new(TokioMutex::new(ProcessTable::new(&config)));
        let dispatcher = McpDispatcher::new(sm.clone(), pt, config);

        let req = make_request(
            json!(50),
            "tools/call",
            Some(json!({ "name": "sh_run", "arguments": { "cmd": "echo hello_dispatch", "timeout": 5 } })),
        );

        let resp = dispatcher.dispatch(req).await.unwrap();
        assert!(resp.error.is_none(), "Expected success, got error: {:?}", resp.error);

        let result = resp.result.unwrap();
        assert!(result["processes"].is_array());
        let inner = &result["result"];
        assert_eq!(inner["exit_code"], 0);
        assert!(inner["output"].as_str().unwrap().contains("hello_dispatch"));

        sm.close_all().await;
    }

    // ── Test 11: tools/call sh_session list works ──

    #[tokio::test]
    async fn tools_call_sh_session_list() {
        let config = test_config();
        let sm = Arc::new(SessionManager::new(config.clone()));
        sm.create_session("main", Some("/bin/bash"))
            .await
            .expect("should create main session");
        let pt = Arc::new(TokioMutex::new(ProcessTable::new(&config)));
        let dispatcher = McpDispatcher::new(sm.clone(), pt, config);

        let req = make_request(
            json!(60),
            "tools/call",
            Some(json!({ "name": "sh_session", "arguments": { "action": "list" } })),
        );

        let resp = dispatcher.dispatch(req).await.unwrap();
        assert!(resp.error.is_none(), "Expected success, got error: {:?}", resp.error);

        let result = resp.result.unwrap();
        let sessions = &result["result"]["sessions"];
        assert!(sessions.is_array());
        assert!(sessions.as_array().unwrap().len() >= 1);

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
        // Error should still have processes digest
        assert!(err.data.unwrap()["processes"].is_array());
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
        let handler_actions = ["create", "list", "close"];

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
}
