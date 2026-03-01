use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ----- JSON-RPC 2.0 base types -----

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub method: String,
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

// ----- MCP Protocol Messages -----

#[derive(Debug, Clone, Deserialize)]
pub struct InitializeParams {
    pub protocol_version: String,
    pub capabilities: ClientCapabilities,
    pub client_info: ClientInfo,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClientCapabilities {}

#[derive(Debug, Clone, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InitializeResult {
    pub protocol_version: String,
    pub capabilities: ServerCapabilities,
    pub server_info: ServerInfo,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerCapabilities {
    pub tools: ToolsCapability,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolsCapability {
    pub list_changed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

// ----- Tool Schemas -----

#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
}

// ----- Tool Parameter Types -----

#[derive(Debug, Clone, Deserialize)]
pub struct ShRunParams {
    pub cmd: String,
    pub timeout: Option<u64>,
    pub watch: Option<String>,
    pub unmatched: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShSpawnParams {
    pub alias: String,
    pub cmd: String,
    pub wait_for: Option<String>,
    pub timeout: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShInteractParams {
    pub alias: String,
    pub action: String,
    pub input: Option<String>,
    pub lines: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShSessionParams {
    pub action: String,
}

// sh_help has no parameters

// ----- Tool Response Types -----

#[derive(Debug, Clone, Serialize)]
pub struct ShRunResponse {
    pub exit_code: i32,
    pub duration_ms: u64,
    pub cwd: String,
    pub category: String,
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_lines: Option<Vec<String>>,
    pub lines: LineCount,
}

#[derive(Debug, Clone, Serialize)]
pub struct LineCount {
    pub total: u64,
    pub shown: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShSpawnResponse {
    pub alias: String,
    pub pid: u32,
    pub session: String,
    pub state: String,
    pub wait_matched: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_line: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_to_match_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShInteractSendResponse {
    pub alias: String,
    pub action: String,
    pub bytes_written: usize,
    pub state: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShInteractReadTailResponse {
    pub alias: String,
    pub action: String,
    pub output: String,
    pub lines_returned: usize,
    pub state: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShInteractSignalResponse {
    pub alias: String,
    pub action: String,
    pub signal_sent: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShInteractKillResponse {
    pub alias: String,
    pub action: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShInteractStatusResponse {
    pub alias: String,
    pub action: String,
    pub session: String,
    pub state: String,
    pub pid: u32,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    pub elapsed_ms: u64,
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_tail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShSessionListResponse {
    pub sessions: Vec<SessionInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub session: String,
    pub shell: String,
    pub cwd: String,
    pub active_processes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShHelpResponse {
    pub tools: Vec<ToolSummary>,
    pub watch_presets: HashMap<String, String>,
    pub squasher_defaults: SquasherDefaults,
    pub resource_limits: ResourceLimits,
    pub resource_usage: ResourceUsage,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolSummary {
    pub name: String,
    pub params: Vec<ParamSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ParamSummary {
    pub name: String,
    pub r#type: String,
    pub required: bool,
    pub default: Option<String>,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SquasherDefaults {
    pub max_lines: usize,
    pub oreo_head: usize,
    pub oreo_tail: usize,
    pub max_bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourceLimits {
    pub max_sessions: usize,
    pub max_processes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourceUsage {
    pub active_sessions: usize,
    pub active_processes: usize,
}

// ----- Process Table Digest -----

#[derive(Debug, Serialize, Clone)]
pub struct ProcessDigestEntry {
    pub alias: String,
    pub session: String,
    pub state: String,
    pub pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_match: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handoff_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_tail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify_operator: Option<bool>,
}

// ----- MCP Tool Response Wrapper -----

#[derive(Debug, Clone, Serialize)]
pub struct ToolResponse {
    pub result: serde_json::Value,
    pub processes: Vec<ProcessDigestEntry>,
}

// ----- Application Error Codes -----

pub const ERR_SHELL_ERROR: i32 = -32000;
pub const ERR_PROCESS_HANDED_OFF: i32 = -32001;
pub const ERR_SESSION_NOT_FOUND: i32 = -32002;
pub const ERR_ALIAS_NOT_FOUND: i32 = -32003;
pub const ERR_ALIAS_IN_USE: i32 = -32004;
pub const ERR_COMMAND_BLOCKED: i32 = -32005;
pub const ERR_SESSION_LIMIT: i32 = -32006;
pub const ERR_PROCESS_LIMIT: i32 = -32007;
pub const ERR_SESSION_NOT_READY: i32 = -32008;
pub const ERR_INVALID_ACTION: i32 = -32009;

pub const ERR_PARSE_ERROR: i32 = -32700;
pub const ERR_INVALID_REQUEST: i32 = -32600;
pub const ERR_METHOD_NOT_FOUND: i32 = -32601;
pub const ERR_INVALID_PARAMS: i32 = -32602;
pub const ERR_INTERNAL: i32 = -32603;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- Test 1: JsonRpcRequest round-trip ----

    #[test]
    fn json_rpc_request_round_trip() {
        let json_str = r#"{
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "sh_run", "arguments": {"cmd": "ls"}}
        }"#;

        let req: JsonRpcRequest = serde_json::from_str(json_str).unwrap();
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.id, json!(1));
        assert_eq!(req.method, "tools/call");
        assert!(req.params.is_some());

        // Serialize back and deserialize again
        let serialized = serde_json::to_string(&req).unwrap();
        let req2: JsonRpcRequest = serde_json::from_str(&serialized).unwrap();
        assert_eq!(req2.jsonrpc, "2.0");
        assert_eq!(req2.id, json!(1));
        assert_eq!(req2.method, "tools/call");
    }

    #[test]
    fn json_rpc_request_with_string_id() {
        let json_str = r#"{
            "jsonrpc": "2.0",
            "id": "abc-123",
            "method": "initialize",
            "params": null
        }"#;

        let req: JsonRpcRequest = serde_json::from_str(json_str).unwrap();
        assert_eq!(req.id, json!("abc-123"));
    }

    #[test]
    fn json_rpc_request_without_params() {
        let json_str = r#"{
            "jsonrpc": "2.0",
            "id": 42,
            "method": "notifications/initialized"
        }"#;

        let req: JsonRpcRequest = serde_json::from_str(json_str).unwrap();
        assert!(req.params.is_none());
    }

    // ---- Test 2: Serialize/deserialize response types ----

    #[test]
    fn sh_run_response_serialization() {
        let resp = ShRunResponse {
            exit_code: 0,
            duration_ms: 150,
            cwd: "/home/user".to_string(),
            category: "passthrough".to_string(),
            output: "file1.txt\nfile2.txt".to_string(),
            matched_lines: None,
            lines: LineCount {
                total: 2,
                shown: 2,
            },
        };

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["exit_code"], 0);
        assert_eq!(json["duration_ms"], 150);
        assert_eq!(json["cwd"], "/home/user");
        assert_eq!(json["category"], "passthrough");
        assert_eq!(json["output"], "file1.txt\nfile2.txt");
        assert_eq!(json["lines"]["total"], 2);
        assert_eq!(json["lines"]["shown"], 2);
    }

    #[test]
    fn sh_run_response_with_matched_lines() {
        let resp = ShRunResponse {
            exit_code: 0,
            duration_ms: 250,
            cwd: "/tmp".to_string(),
            category: "condense".to_string(),
            output: "ERROR: something\nERROR: else".to_string(),
            matched_lines: Some(vec![
                "ERROR: something".to_string(),
                "ERROR: else".to_string(),
            ]),
            lines: LineCount {
                total: 100,
                shown: 2,
            },
        };

        let json = serde_json::to_value(&resp).unwrap();
        assert!(json["matched_lines"].is_array());
        assert_eq!(json["matched_lines"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn sh_spawn_response_serialization() {
        let resp = ShSpawnResponse {
            alias: "myserver".to_string(),
            pid: 12345,
            session: "default".to_string(),
            state: "running".to_string(),
            wait_matched: true,
            match_line: Some("Server listening on port 3000".to_string()),
            duration_to_match_ms: Some(1200),
            output_tail: None,
            reason: None,
        };

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["alias"], "myserver");
        assert_eq!(json["pid"], 12345);
        assert_eq!(json["session"], "default");
        assert_eq!(json["state"], "running");
        assert!(json["wait_matched"].as_bool().unwrap());
        assert_eq!(
            json["match_line"],
            "Server listening on port 3000"
        );
        assert_eq!(json["duration_to_match_ms"], 1200);
    }

    #[test]
    fn sh_interact_send_response_serialization() {
        let resp = ShInteractSendResponse {
            alias: "myapp".to_string(),
            action: "send".to_string(),
            bytes_written: 5,
            state: "running".to_string(),
        };

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["alias"], "myapp");
        assert_eq!(json["action"], "send");
        assert_eq!(json["bytes_written"], 5);
        assert_eq!(json["state"], "running");
    }

    #[test]
    fn sh_interact_read_tail_response_serialization() {
        let resp = ShInteractReadTailResponse {
            alias: "myapp".to_string(),
            action: "read_tail".to_string(),
            output: "line1\nline2\nline3".to_string(),
            lines_returned: 3,
            state: "running".to_string(),
        };

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["lines_returned"], 3);
        assert_eq!(json["output"], "line1\nline2\nline3");
    }

    #[test]
    fn sh_interact_signal_response_serialization() {
        let resp = ShInteractSignalResponse {
            alias: "myapp".to_string(),
            action: "signal".to_string(),
            signal_sent: "SIGTERM".to_string(),
            state: "running".to_string(),
        };

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["signal_sent"], "SIGTERM");
    }

    #[test]
    fn sh_interact_kill_response_serialization() {
        let resp = ShInteractKillResponse {
            alias: "myapp".to_string(),
            action: "kill".to_string(),
            state: "exited".to_string(),
        };

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["alias"], "myapp");
        assert_eq!(json["action"], "kill");
        assert_eq!(json["state"], "exited");
    }

    #[test]
    fn sh_interact_status_response_serialization() {
        let resp = ShInteractStatusResponse {
            alias: "myapp".to_string(),
            action: "status".to_string(),
            session: "default".to_string(),
            state: "exited".to_string(),
            pid: 9999,
            exit_code: Some(1),
            signal: None,
            elapsed_ms: 5000,
            duration_ms: Some(5000),
            prompt_tail: None,
            output_summary: Some("Build failed".to_string()),
            error_tail: Some("error[E0308]: mismatched types".to_string()),
        };

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["exit_code"], 1);
        assert_eq!(json["elapsed_ms"], 5000);
        assert_eq!(json["duration_ms"], 5000);
        assert_eq!(json["output_summary"], "Build failed");
    }

    #[test]
    fn sh_session_list_response_serialization() {
        let resp = ShSessionListResponse {
            sessions: vec![
                SessionInfo {
                    session: "default".to_string(),
                    shell: "/bin/bash".to_string(),
                    cwd: "/home/user".to_string(),
                    active_processes: 2,
                },
                SessionInfo {
                    session: "build".to_string(),
                    shell: "/bin/zsh".to_string(),
                    cwd: "/home/user/project".to_string(),
                    active_processes: 0,
                },
            ],
        };

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["sessions"].as_array().unwrap().len(), 2);
        assert_eq!(json["sessions"][0]["session"], "default");
        assert_eq!(json["sessions"][1]["active_processes"], 0);
    }

    #[test]
    fn sh_help_response_serialization() {
        let mut watch_presets = HashMap::new();
        watch_presets.insert("@errors".to_string(), "(?i)(error|fail|panic)".to_string());

        let resp = ShHelpResponse {
            tools: vec![ToolSummary {
                name: "sh_run".to_string(),
                params: vec![ParamSummary {
                    name: "cmd".to_string(),
                    r#type: "string".to_string(),
                    required: true,
                    default: None,
                    description: "Command to execute".to_string(),
                }],
            }],
            watch_presets,
            squasher_defaults: SquasherDefaults {
                max_lines: 200,
                oreo_head: 80,
                oreo_tail: 40,
                max_bytes: 32768,
            },
            resource_limits: ResourceLimits {
                max_sessions: 4,
                max_processes: 16,
            },
            resource_usage: ResourceUsage {
                active_sessions: 1,
                active_processes: 3,
            },
        };

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["tools"][0]["name"], "sh_run");
        assert_eq!(json["tools"][0]["params"][0]["required"], true);
        assert_eq!(json["squasher_defaults"]["max_lines"], 200);
        assert_eq!(json["resource_limits"]["max_sessions"], 4);
        assert_eq!(json["resource_usage"]["active_processes"], 3);
        assert!(json["watch_presets"]["@errors"].is_string());
    }

    // ---- Test 3: Error codes are correct integer values ----

    #[test]
    fn application_error_codes_correct() {
        assert_eq!(ERR_PROCESS_HANDED_OFF, -32001);
        assert_eq!(ERR_SESSION_NOT_FOUND, -32002);
        assert_eq!(ERR_ALIAS_NOT_FOUND, -32003);
        assert_eq!(ERR_ALIAS_IN_USE, -32004);
        assert_eq!(ERR_COMMAND_BLOCKED, -32005);
        assert_eq!(ERR_SESSION_LIMIT, -32006);
        assert_eq!(ERR_PROCESS_LIMIT, -32007);
        assert_eq!(ERR_SESSION_NOT_READY, -32008);
        assert_eq!(ERR_INVALID_ACTION, -32009);
    }

    #[test]
    fn json_rpc_error_codes_correct() {
        assert_eq!(ERR_PARSE_ERROR, -32700);
        assert_eq!(ERR_INVALID_REQUEST, -32600);
        assert_eq!(ERR_METHOD_NOT_FOUND, -32601);
        assert_eq!(ERR_INVALID_PARAMS, -32602);
        assert_eq!(ERR_INTERNAL, -32603);
    }

    // ---- Test 4: skip_serializing_if works correctly ----

    #[test]
    fn none_fields_omitted_from_json_rpc_response() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: json!(1),
            result: Some(json!({"ok": true})),
            error: None,
        };

        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("result").is_some());
        assert!(json.get("error").is_none());
    }

    #[test]
    fn none_fields_omitted_from_json_rpc_error() {
        let err = JsonRpcError {
            code: ERR_INTERNAL,
            message: "Internal error".to_string(),
            data: None,
        };

        let json = serde_json::to_value(&err).unwrap();
        assert!(json.get("data").is_none());
        assert_eq!(json["code"], -32603);
    }

    #[test]
    fn none_fields_omitted_from_sh_run_response() {
        let resp = ShRunResponse {
            exit_code: 0,
            duration_ms: 50,
            cwd: "/tmp".to_string(),
            category: "condense".to_string(),
            output: "hello".to_string(),
            matched_lines: None,
            lines: LineCount {
                total: 1,
                shown: 1,
            },
        };

        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("matched_lines").is_none());
    }

    #[test]
    fn none_fields_omitted_from_sh_spawn_response() {
        let resp = ShSpawnResponse {
            alias: "bg".to_string(),
            pid: 100,
            session: "default".to_string(),
            state: "running".to_string(),
            wait_matched: false,
            match_line: None,
            duration_to_match_ms: None,
            output_tail: None,
            reason: Some("timeout".to_string()),
        };

        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("match_line").is_none());
        assert!(json.get("duration_to_match_ms").is_none());
        assert!(json.get("output_tail").is_none());
        assert_eq!(json["reason"], "timeout");
    }

    #[test]
    fn none_fields_omitted_from_sh_interact_status_response() {
        let resp = ShInteractStatusResponse {
            alias: "test".to_string(),
            action: "status".to_string(),
            session: "default".to_string(),
            state: "running".to_string(),
            pid: 555,
            exit_code: None,
            signal: None,
            elapsed_ms: 3000,
            duration_ms: None,
            prompt_tail: None,
            output_summary: None,
            error_tail: None,
        };

        let json = serde_json::to_value(&resp).unwrap();
        // These are not skip_serializing_if, they serialize as null
        assert!(json.get("exit_code").is_some());
        assert!(json.get("signal").is_some());
        assert!(json.get("duration_ms").is_some());
        // These ARE skip_serializing_if
        assert!(json.get("prompt_tail").is_none());
        assert!(json.get("output_summary").is_none());
        assert!(json.get("error_tail").is_none());
    }

    // ---- Test 5: ProcessDigestEntry with only required fields ----

    #[test]
    fn process_digest_entry_required_fields_only() {
        let entry = ProcessDigestEntry {
            alias: "myproc".to_string(),
            session: "default".to_string(),
            state: "running".to_string(),
            pid: 1234,
            exit_code: None,
            signal: None,
            elapsed_ms: 5000,
            duration_ms: None,
            prompt_tail: None,
            last_match: None,
            match_count: None,
            handoff_id: None,
            output_summary: None,
            error_tail: None,
            notify_operator: None,
        };

        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["alias"], "myproc");
        assert_eq!(json["session"], "default");
        assert_eq!(json["state"], "running");
        assert_eq!(json["pid"], 1234);
        assert_eq!(json["elapsed_ms"], 5000);

        // All optional fields should be absent
        assert!(json.get("exit_code").is_none());
        assert!(json.get("signal").is_none());
        assert!(json.get("duration_ms").is_none());
        assert!(json.get("prompt_tail").is_none());
        assert!(json.get("last_match").is_none());
        assert!(json.get("match_count").is_none());
        assert!(json.get("handoff_id").is_none());
        assert!(json.get("output_summary").is_none());
        assert!(json.get("error_tail").is_none());
        assert!(json.get("notify_operator").is_none());
    }

    // ---- Test 6: ProcessDigestEntry with all optional fields ----

    #[test]
    fn process_digest_entry_all_optional_fields() {
        let entry = ProcessDigestEntry {
            alias: "server".to_string(),
            session: "build".to_string(),
            state: "yielding".to_string(),
            pid: 9876,
            exit_code: Some(0),
            signal: Some("SIGTERM".to_string()),
            elapsed_ms: 30000,
            duration_ms: Some(30000),
            prompt_tail: Some("$ ".to_string()),
            last_match: Some("Error: connection refused".to_string()),
            match_count: Some(3),
            handoff_id: Some("abc123def456".to_string()),
            output_summary: Some("Server started, 3 errors detected".to_string()),
            error_tail: Some("ECONNREFUSED".to_string()),
            notify_operator: Some(true),
        };

        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["alias"], "server");
        assert_eq!(json["exit_code"], 0);
        assert_eq!(json["signal"], "SIGTERM");
        assert_eq!(json["duration_ms"], 30000);
        assert_eq!(json["prompt_tail"], "$ ");
        assert_eq!(json["last_match"], "Error: connection refused");
        assert_eq!(json["match_count"], 3);
        assert_eq!(json["handoff_id"], "abc123def456");
        assert_eq!(
            json["output_summary"],
            "Server started, 3 errors detected"
        );
        assert_eq!(json["error_tail"], "ECONNREFUSED");
        assert_eq!(json["notify_operator"], true);
    }

    // ---- Test 7: ToolResponse wraps result + processes ----

    #[test]
    fn tool_response_wraps_result_and_processes() {
        let run_resp = ShRunResponse {
            exit_code: 0,
            duration_ms: 100,
            cwd: "/home".to_string(),
            category: "condense".to_string(),
            output: "ok".to_string(),
            matched_lines: None,
            lines: LineCount {
                total: 1,
                shown: 1,
            },
        };

        let tool_resp = ToolResponse {
            result: serde_json::to_value(&run_resp).unwrap(),
            processes: vec![ProcessDigestEntry {
                alias: "bg1".to_string(),
                session: "default".to_string(),
                state: "running".to_string(),
                pid: 111,
                exit_code: None,
                signal: None,
                elapsed_ms: 2000,
                duration_ms: None,
                prompt_tail: None,
                last_match: None,
                match_count: None,
                handoff_id: None,
                output_summary: None,
                error_tail: None,
                notify_operator: None,
            }],
        };

        let json = serde_json::to_value(&tool_resp).unwrap();
        assert_eq!(json["result"]["exit_code"], 0);
        assert_eq!(json["result"]["output"], "ok");
        assert_eq!(json["processes"].as_array().unwrap().len(), 1);
        assert_eq!(json["processes"][0]["alias"], "bg1");
        assert_eq!(json["processes"][0]["pid"], 111);
    }

    #[test]
    fn tool_response_with_empty_processes() {
        let tool_resp = ToolResponse {
            result: json!({"exit_code": 0}),
            processes: vec![],
        };

        let json = serde_json::to_value(&tool_resp).unwrap();
        assert!(json["processes"].as_array().unwrap().is_empty());
    }

    #[test]
    fn tool_response_with_error_still_has_processes() {
        // Error responses should still include process digest
        let error_resp = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: json!(5),
            result: None,
            error: Some(JsonRpcError {
                code: ERR_ALIAS_NOT_FOUND,
                message: "Process 'myproc' not found".to_string(),
                data: Some(json!({
                    "processes": [{
                        "alias": "other",
                        "session": "default",
                        "state": "running",
                        "pid": 200,
                        "elapsed_ms": 1000
                    }]
                })),
            }),
        };

        let json = serde_json::to_value(&error_resp).unwrap();
        assert!(json.get("result").is_none());
        assert!(json.get("error").is_some());
        assert_eq!(json["error"]["code"], -32003);
        assert!(json["error"]["data"]["processes"].is_array());
    }

    // ---- Test 8: All tool parameter types deserialize from JSON ----

    #[test]
    fn sh_run_params_deserialize() {
        let json_str = r#"{"cmd": "ls -la", "timeout": 30, "watch": "@errors", "unmatched": "tail"}"#;
        let params: ShRunParams = serde_json::from_str(json_str).unwrap();
        assert_eq!(params.cmd, "ls -la");
        assert_eq!(params.timeout, Some(30));
        assert_eq!(params.watch, Some("@errors".to_string()));
        assert_eq!(params.unmatched, Some("tail".to_string()));
    }

    #[test]
    fn sh_run_params_deserialize_minimal() {
        let json_str = r#"{"cmd": "echo hello"}"#;
        let params: ShRunParams = serde_json::from_str(json_str).unwrap();
        assert_eq!(params.cmd, "echo hello");
        assert!(params.timeout.is_none());
        assert!(params.watch.is_none());
        assert!(params.unmatched.is_none());
    }

    #[test]
    fn sh_spawn_params_deserialize() {
        let json_str = r#"{"alias": "server", "cmd": "npm start", "wait_for": "listening on", "timeout": 10}"#;
        let params: ShSpawnParams = serde_json::from_str(json_str).unwrap();
        assert_eq!(params.alias, "server");
        assert_eq!(params.cmd, "npm start");
        assert_eq!(params.wait_for, Some("listening on".to_string()));
        assert_eq!(params.timeout, Some(10));
    }

    #[test]
    fn sh_spawn_params_deserialize_minimal() {
        let json_str = r#"{"alias": "bg", "cmd": "sleep 60"}"#;
        let params: ShSpawnParams = serde_json::from_str(json_str).unwrap();
        assert_eq!(params.alias, "bg");
        assert_eq!(params.cmd, "sleep 60");
        assert!(params.wait_for.is_none());
        assert!(params.timeout.is_none());
    }

    #[test]
    fn sh_interact_params_deserialize() {
        let json_str = r#"{"alias": "myapp", "action": "send", "input": "yes\n", "lines": 50}"#;
        let params: ShInteractParams = serde_json::from_str(json_str).unwrap();
        assert_eq!(params.alias, "myapp");
        assert_eq!(params.action, "send");
        assert_eq!(params.input, Some("yes\n".to_string()));
        assert_eq!(params.lines, Some(50));
    }

    #[test]
    fn sh_interact_params_deserialize_minimal() {
        let json_str = r#"{"alias": "myapp", "action": "status"}"#;
        let params: ShInteractParams = serde_json::from_str(json_str).unwrap();
        assert_eq!(params.alias, "myapp");
        assert_eq!(params.action, "status");
        assert!(params.input.is_none());
        assert!(params.lines.is_none());
    }

    #[test]
    fn sh_session_params_deserialize() {
        let json_str = r#"{"action": "list"}"#;
        let params: ShSessionParams = serde_json::from_str(json_str).unwrap();
        assert_eq!(params.action, "list");
    }

    // ---- Additional tests for protocol types ----

    #[test]
    fn initialize_params_deserialize() {
        let json_str = r#"{
            "protocol_version": "2024-11-05",
            "capabilities": {},
            "client_info": {
                "name": "claude-desktop",
                "version": "1.0.0"
            }
        }"#;

        let params: InitializeParams = serde_json::from_str(json_str).unwrap();
        assert_eq!(params.protocol_version, "2024-11-05");
        assert_eq!(params.client_info.name, "claude-desktop");
        assert_eq!(params.client_info.version, Some("1.0.0".to_string()));
    }

    #[test]
    fn initialize_params_deserialize_without_client_version() {
        let json_str = r#"{
            "protocol_version": "2024-11-05",
            "capabilities": {},
            "client_info": {
                "name": "test-client"
            }
        }"#;

        let params: InitializeParams = serde_json::from_str(json_str).unwrap();
        assert!(params.client_info.version.is_none());
    }

    #[test]
    fn initialize_result_serialization() {
        let result = InitializeResult {
            protocol_version: "2024-11-05".to_string(),
            capabilities: ServerCapabilities {
                tools: ToolsCapability {
                    list_changed: false,
                },
            },
            server_info: ServerInfo {
                name: "mish".to_string(),
                version: "0.1.0".to_string(),
            },
        };

        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["protocol_version"], "2024-11-05");
        assert_eq!(json["capabilities"]["tools"]["list_changed"], false);
        assert_eq!(json["server_info"]["name"], "mish");
        assert_eq!(json["server_info"]["version"], "0.1.0");
    }

    #[test]
    fn tool_definition_serialization() {
        let tool = ToolDefinition {
            name: "sh_run".to_string(),
            description: "Run a command synchronously".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cmd": {"type": "string", "description": "Command to execute"}
                },
                "required": ["cmd"]
            }),
        };

        let json = serde_json::to_value(&tool).unwrap();
        assert_eq!(json["name"], "sh_run");
        assert_eq!(json["inputSchema"]["type"], "object");
        assert_eq!(
            json["inputSchema"]["required"].as_array().unwrap(),
            &[json!("cmd")]
        );
    }

    #[test]
    fn json_rpc_response_success() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: json!(1),
            result: Some(json!({"exit_code": 0})),
            error: None,
        };

        let json_str = serde_json::to_string(&resp).unwrap();
        assert!(json_str.contains("\"jsonrpc\":\"2.0\""));
        assert!(json_str.contains("\"result\""));
        assert!(!json_str.contains("\"error\""));
    }

    #[test]
    fn json_rpc_response_error() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: json!(2),
            result: None,
            error: Some(JsonRpcError {
                code: ERR_METHOD_NOT_FOUND,
                message: "Method not found".to_string(),
                data: None,
            }),
        };

        let json_str = serde_json::to_string(&resp).unwrap();
        assert!(!json_str.contains("\"result\""));
        assert!(json_str.contains("\"error\""));
        assert!(json_str.contains("-32601"));
    }

    #[test]
    fn process_digest_entry_clone() {
        let entry = ProcessDigestEntry {
            alias: "test".to_string(),
            session: "default".to_string(),
            state: "running".to_string(),
            pid: 42,
            exit_code: None,
            signal: None,
            elapsed_ms: 100,
            duration_ms: None,
            prompt_tail: None,
            last_match: None,
            match_count: None,
            handoff_id: None,
            output_summary: None,
            error_tail: None,
            notify_operator: None,
        };

        let cloned = entry.clone();
        assert_eq!(cloned.alias, "test");
        assert_eq!(cloned.pid, 42);
    }
}
