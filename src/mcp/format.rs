//! MCP-specific output formatters.
//!
//! Converts structured JSON tool results into compact, symbol-prefixed text
//! that is token-efficient for LLM consumption. Shared format with CLI proxy
//! via `core::format::format_result`.

use crate::core::format::{
    self, EnrichmentLine, FormatInput, OutputMode, RecommendationEntry,
};
use crate::mcp::types::ProcessDigestEntry;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Format a complete MCP tool response (result + digest) as compact text.
pub fn format_tool_response(
    tool_name: &str,
    result: &serde_json::Value,
    digest: &[ProcessDigestEntry],
) -> String {
    let body = match tool_name {
        "sh_run" => format_sh_run(result),
        "sh_spawn" => format_sh_spawn(result),
        "sh_interact" => format_sh_interact(result),
        "sh_session" => format_sh_session(result),
        "sh_help" => format_sh_help(result),
        _ => serde_json::to_string_pretty(result).unwrap_or_default(),
    };

    let digest_text = format_digest(digest);

    if digest_text.is_empty() {
        body
    } else {
        format!("{}\n{}", body, digest_text)
    }
}

/// Format process digest as compact text: `[procs] alias:state:elapsed ...`
///
/// Returns empty string when digest is empty.
pub fn format_digest(digest: &[ProcessDigestEntry]) -> String {
    if digest.is_empty() {
        return String::new();
    }

    let entries: Vec<String> = digest
        .iter()
        .map(|p| {
            let elapsed = format::format_elapsed(Some(p.elapsed_ms as f64 / 1000.0));
            format!("{}:{}:{}", p.alias, p.state, elapsed)
        })
        .collect();

    format!("[procs] {}", entries.join(" "))
}

// ---------------------------------------------------------------------------
// Per-tool formatters
// ---------------------------------------------------------------------------

/// Format sh_run result using the shared core formatter.
fn format_sh_run(result: &serde_json::Value) -> String {
    let exit_code = result["exit_code"].as_i64().unwrap_or(0) as i32;
    let duration_ms = result["duration_ms"].as_u64().unwrap_or(0);
    let category = result["category"].as_str().unwrap_or("condense").to_string();
    let output = result["output"].as_str().unwrap_or("");
    let total_lines = result["lines"]["total"].as_u64();
    let elapsed_secs = Some(duration_ms as f64 / 1000.0);

    let mut enrichment = Vec::new();
    if let Some(arr) = result["enrichment"].as_array() {
        for e in arr {
            enrichment.push(EnrichmentLine {
                kind: e["kind"].as_str().unwrap_or("").to_string(),
                message: e["message"].as_str().unwrap_or("").to_string(),
            });
        }
    }

    let mut recommendations = Vec::new();
    if let Some(arr) = result["recommendations"].as_array() {
        for r in arr {
            recommendations.push(RecommendationEntry {
                flag: r["flag"].as_str().unwrap_or("").to_string(),
                reason: r["reason"].as_str().unwrap_or("").to_string(),
            });
        }
    }

    let input = FormatInput {
        command: String::new(),
        exit_code,
        category,
        body: output.to_string(),
        raw_output: None,
        total_lines,
        elapsed_secs,
        outcomes: vec![],
        hazards: vec![],
        enrichment,
        recommendations,
    };

    format::format_result(&input, OutputMode::Human)
}

/// Format sh_spawn result.
///
/// `+ spawned alias pid:N session:S matched:Xms`
/// `~ spawned alias pid:N session:S timeout:Xms`
fn format_sh_spawn(result: &serde_json::Value) -> String {
    let alias = result["alias"].as_str().unwrap_or("?");
    let pid = result["pid"].as_u64().unwrap_or(0);
    let session = result["session"].as_str().unwrap_or("default");
    let wait_matched = result["wait_matched"].as_bool().unwrap_or(false);

    let symbol = if wait_matched { "+" } else { "~" };
    let mut header = format!("{} spawned {} pid:{} session:{}", symbol, alias, pid, session);

    if let Some(ms) = result["duration_to_match_ms"].as_u64() {
        let label = if wait_matched { "matched" } else { "timeout" };
        header.push_str(&format!(
            " {}:{}",
            label,
            format::format_elapsed(Some(ms as f64 / 1000.0))
        ));
    }

    let mut lines = vec![header];

    if let Some(match_line) = result["match_line"].as_str() {
        if !match_line.is_empty() {
            lines.push(match_line.to_string());
        }
    }
    if let Some(output_tail) = result["output_tail"].as_str() {
        if !output_tail.is_empty() {
            lines.push(output_tail.to_string());
        }
    }

    lines.join("\n")
}

/// Format sh_interact result based on action.
fn format_sh_interact(result: &serde_json::Value) -> String {
    let alias = result["alias"].as_str().unwrap_or("?");
    let action = result["action"].as_str().unwrap_or("?");
    let state = result["state"].as_str().unwrap_or("?");

    match action {
        "read_tail" | "read_full" => {
            let lines_returned = result["lines_returned"].as_u64().unwrap_or(0);
            let output = result["output"].as_str().unwrap_or("");
            let mut text = format!("+ {} {} {} lines {}", alias, action, lines_returned, state);
            if !output.is_empty() {
                text.push('\n');
                text.push_str(output);
            }
            text
        }
        "send_input" | "send" => {
            // Background mode: fire-and-forget REPL send.
            if result["background"].as_bool().unwrap_or(false) {
                let bytes = result["bytes_written"].as_u64().unwrap_or(0);
                format!("+ {} send_input(bg) {}B {}", alias, bytes, state)
            // Interpreter mode: response has "output" field with actual result.
            } else if let Some(output) = result["output"].as_str() {
                let elapsed_ms = result["elapsed_ms"].as_u64().unwrap_or(0);
                let elapsed = format::format_elapsed(Some(elapsed_ms as f64 / 1000.0));
                let mut text = format!("+ {} send_input {} {}", alias, elapsed, state);
                if !output.is_empty() {
                    text.push('\n');
                    text.push_str(output);
                }
                text
            } else {
                // Regular mode: response has "bytes_written" field.
                let bytes = result["bytes_written"].as_u64().unwrap_or(0);
                format!("+ {} send_input {}B {}", alias, bytes, state)
            }
        }
        "signal" | "send_signal" => {
            let signal = result["signal_sent"].as_str().unwrap_or("?");
            format!("+ {} signal {} {}", alias, signal, state)
        }
        "kill" => {
            if state == "killed" {
                format!("- {} killed", alias)
            } else {
                format!("- {} killed {}", alias, state)
            }
        }
        "status" => {
            let pid = result["pid"].as_u64().unwrap_or(0);
            let elapsed_ms = result["elapsed_ms"].as_u64().unwrap_or(0);
            let session = result["session"].as_str().unwrap_or("default");
            let elapsed = format::format_elapsed(Some(elapsed_ms as f64 / 1000.0));
            format!(
                "+ {} status {} pid:{} {} session:{}",
                alias, state, pid, elapsed, session
            )
        }
        _ => serde_json::to_string_pretty(result).unwrap_or_default(),
    }
}

/// Format sh_session result based on response shape.
fn format_sh_session(result: &serde_json::Value) -> String {
    if let Some(sessions) = result.get("sessions") {
        // List response
        let mut lines = vec!["+ session list".to_string()];
        if let Some(arr) = sessions.as_array() {
            for s in arr {
                let name = s["session"].as_str().unwrap_or("?");
                let shell = s["shell"].as_str().unwrap_or("?");
                let cwd = s["cwd"].as_str().unwrap_or("?");
                let procs = s["active_processes"].as_u64().unwrap_or(0);
                lines.push(format!("  {} {} {} ({} procs)", name, shell, cwd, procs));
            }
        }
        lines.join("\n")
    } else if result.get("closed").is_some() {
        // Close response
        let session = result["session"].as_str().unwrap_or("?");
        if result["closed"].as_bool().unwrap_or(false) {
            format!("+ session close {}", session)
        } else {
            format!("! session close {} failed", session)
        }
    } else if result.get("ready").is_some() {
        // Create response
        let session = result["session"].as_str().unwrap_or("?");
        let ready = result["ready"].as_bool().unwrap_or(false);
        if ready {
            format!("+ session create {} ready", session)
        } else {
            format!("~ session create {} not ready", session)
        }
    } else if result.get("total_commands").is_some() {
        // Audit summary response — keep as JSON
        serde_json::to_string_pretty(result).unwrap_or_default()
    } else if result.get("entries").is_some() {
        // Audit entries response — keep as JSON
        serde_json::to_string_pretty(result).unwrap_or_default()
    } else {
        // Fallback
        serde_json::to_string_pretty(result).unwrap_or_default()
    }
}

/// Format sh_help result as text reference card.
fn format_sh_help(result: &serde_json::Value) -> String {
    let mut lines = Vec::new();

    lines.push("# mish reference card".to_string());

    // Tools
    if let Some(tools) = result["tools"].as_array() {
        lines.push(String::new());
        lines.push("## tools".to_string());
        for tool in tools {
            let name = tool["name"].as_str().unwrap_or("?");
            lines.push(format!("  {}", name));
            if let Some(params) = tool["params"].as_array() {
                for p in params {
                    let pname = p["name"].as_str().unwrap_or("?");
                    let ptype = p["type"].as_str().unwrap_or("?");
                    let req = if p["required"].as_bool().unwrap_or(false) {
                        "*"
                    } else {
                        ""
                    };
                    let default_str = p["default"]
                        .as_str()
                        .map(|d| format!(" ={}", d))
                        .unwrap_or_default();
                    let desc = p["description"].as_str().unwrap_or("");
                    lines.push(format!(
                        "    {}{}: {}{} \u{2014} {}",
                        pname, req, ptype, default_str, desc
                    ));
                }
            }
        }
    }

    // Watch presets
    if let Some(presets) = result["watch_presets"].as_object() {
        if !presets.is_empty() {
            lines.push(String::new());
            lines.push("## watch presets".to_string());
            for (k, v) in presets {
                lines.push(format!("  @{} = {}", k, v.as_str().unwrap_or("?")));
            }
        }
    }

    // Squasher defaults
    if let Some(sq) = result.get("squasher_defaults") {
        lines.push(String::new());
        lines.push(format!(
            "## squasher max_lines:{} oreo:{}/{} max_bytes:{}",
            sq["max_lines"].as_u64().unwrap_or(0),
            sq["oreo_head"].as_u64().unwrap_or(0),
            sq["oreo_tail"].as_u64().unwrap_or(0),
            sq["max_bytes"].as_u64().unwrap_or(0),
        ));
    }

    // Resource limits + usage
    if let Some(limits) = result.get("resource_limits") {
        if let Some(usage) = result.get("resource_usage") {
            lines.push(format!(
                "## resources sessions:{}/{} processes:{}/{}",
                usage["active_sessions"].as_u64().unwrap_or(0),
                limits["max_sessions"].as_u64().unwrap_or(0),
                usage["active_processes"].as_u64().unwrap_or(0),
                limits["max_processes"].as_u64().unwrap_or(0),
            ));
        }
    }

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── format_digest ───────────────────────────────────────────────────

    #[test]
    fn digest_empty() {
        assert_eq!(format_digest(&[]), "");
    }

    #[test]
    fn digest_single_process() {
        let digest = vec![ProcessDigestEntry {
            alias: "server".to_string(),
            session: "main".to_string(),
            state: "running".to_string(),
            pid: 123,
            exit_code: None,
            signal: None,
            elapsed_ms: 30000,
            duration_ms: None,
            prompt_tail: None,
            last_match: None,
            match_count: None,
            handoff_id: None,
            output_summary: None,
            error_tail: None,
            notify_operator: None,
        }];

        let text = format_digest(&digest);
        assert_eq!(text, "[procs] server:running:30s");
    }

    #[test]
    fn digest_multiple_processes() {
        let digest = vec![
            ProcessDigestEntry {
                alias: "server".to_string(),
                session: "main".to_string(),
                state: "running".to_string(),
                pid: 123,
                exit_code: None,
                signal: None,
                elapsed_ms: 30000,
                duration_ms: None,
                prompt_tail: None,
                last_match: None,
                match_count: None,
                handoff_id: None,
                output_summary: None,
                error_tail: None,
                notify_operator: None,
            },
            ProcessDigestEntry {
                alias: "watcher".to_string(),
                session: "main".to_string(),
                state: "running".to_string(),
                pid: 456,
                exit_code: None,
                signal: None,
                elapsed_ms: 15000,
                duration_ms: None,
                prompt_tail: None,
                last_match: None,
                match_count: None,
                handoff_id: None,
                output_summary: None,
                error_tail: None,
                notify_operator: None,
            },
        ];

        let text = format_digest(&digest);
        assert_eq!(text, "[procs] server:running:30s watcher:running:15s");
    }

    // ── format_sh_run ───────────────────────────────────────────────────

    #[test]
    fn sh_run_success_condense() {
        let result = json!({
            "exit_code": 0,
            "duration_ms": 150,
            "cwd": "/tmp",
            "category": "condense",
            "output": "Compiling mish v0.1.0\n    Finished dev target(s) in 12.34s",
            "lines": { "total": 47, "shown": 2 }
        });

        let text = format_sh_run(&result);
        assert!(text.starts_with("+ exit:0"), "header: {}", text);
        assert!(text.contains("150ms"), "elapsed: {}", text);
        assert!(!text.contains("condense"), "category should not appear: {}", text);
        assert!(text.contains("(47\u{2192}2)"), "ratio: {}", text);
        assert!(text.contains("Compiling mish"), "body: {}", text);
    }

    #[test]
    fn sh_run_failure_with_enrichment() {
        let result = json!({
            "exit_code": 1,
            "duration_ms": 340,
            "cwd": "/tmp",
            "category": "condense",
            "output": "error[E0308]: mismatched types",
            "lines": { "total": 23, "shown": 1 },
            "enrichment": [
                { "kind": "file_exists", "message": "src/main.rs (modified 2m ago)" }
            ]
        });

        let text = format_sh_run(&result);
        assert!(text.starts_with("! exit:1"), "header: {}", text);
        assert!(text.contains("~ file_exists src/main.rs"), "enrichment: {}", text);
    }

    #[test]
    fn sh_run_with_recommendations() {
        let result = json!({
            "exit_code": 0,
            "duration_ms": 5200,
            "cwd": "/tmp",
            "category": "condense",
            "output": "installed 147 packages",
            "lines": { "total": 1000, "shown": 1 },
            "recommendations": [
                { "flag": "--prefer-offline", "reason": "quieter output" }
            ]
        });

        let text = format_sh_run(&result);
        assert!(text.contains("\u{2192} prefer: --prefer-offline"), "rec: {}", text);
    }

    // ── format_sh_spawn ─────────────────────────────────────────────────

    #[test]
    fn sh_spawn_matched() {
        let result = json!({
            "alias": "server",
            "pid": 12345,
            "session": "default",
            "state": "running",
            "wait_matched": true,
            "match_line": "Server listening on port 3000",
            "duration_to_match_ms": 1200
        });

        let text = format_sh_spawn(&result);
        assert!(text.starts_with("+ spawned server"), "header: {}", text);
        assert!(text.contains("pid:12345"), "pid: {}", text);
        assert!(text.contains("session:default"), "session: {}", text);
        assert!(text.contains("matched:1.2s"), "matched: {}", text);
        assert!(text.contains("Server listening"), "match line: {}", text);
    }

    #[test]
    fn sh_spawn_timeout() {
        let result = json!({
            "alias": "server",
            "pid": 12345,
            "session": "default",
            "state": "running",
            "wait_matched": false,
            "duration_to_match_ms": 2000
        });

        let text = format_sh_spawn(&result);
        assert!(text.starts_with("~ spawned server"), "header: {}", text);
        assert!(text.contains("timeout:2s"), "timeout: {}", text);
    }

    // ── format_sh_interact ──────────────────────────────────────────────

    #[test]
    fn sh_interact_read_tail() {
        let result = json!({
            "alias": "server",
            "action": "read_tail",
            "output": "line1\nline2",
            "lines_returned": 2,
            "state": "running"
        });

        let text = format_sh_interact(&result);
        assert!(text.starts_with("+ server read_tail 2 lines running"), "header: {}", text);
        assert!(text.contains("line1\nline2"), "output: {}", text);
    }

    #[test]
    fn sh_interact_kill() {
        let result = json!({
            "alias": "server",
            "action": "kill",
            "state": "exited"
        });

        let text = format_sh_interact(&result);
        assert_eq!(text, "- server killed exited");
    }

    #[test]
    fn sh_interact_kill_state_killed_no_repeat() {
        let result = json!({
            "alias": "server",
            "action": "kill",
            "state": "killed"
        });

        let text = format_sh_interact(&result);
        assert_eq!(text, "- server killed");
    }

    #[test]
    fn sh_interact_status() {
        let result = json!({
            "alias": "server",
            "action": "status",
            "session": "default",
            "state": "running",
            "pid": 12345,
            "elapsed_ms": 30000
        });

        let text = format_sh_interact(&result);
        assert!(text.contains("+ server status running pid:12345 30s session:default"), "status: {}", text);
    }

    #[test]
    fn sh_interact_send_input() {
        let result = json!({
            "alias": "app",
            "action": "send_input",
            "bytes_written": 5,
            "state": "running"
        });

        let text = format_sh_interact(&result);
        assert_eq!(text, "+ app send_input 5B running");
    }

    #[test]
    fn sh_interact_send_input_interpreter_mode() {
        let result = json!({
            "alias": "py",
            "action": "send_input",
            "output": "4",
            "exit_code": 0,
            "elapsed_ms": 150,
            "state": "running"
        });

        let text = format_sh_interact(&result);
        assert!(text.starts_with("+ py send_input"), "header: {}", text);
        assert!(text.contains("150ms"), "elapsed: {}", text);
        assert!(text.contains("4"), "output: {}", text);
    }

    #[test]
    fn sh_interact_send_input_interpreter_empty_output() {
        let result = json!({
            "alias": "py",
            "action": "send_input",
            "output": "",
            "exit_code": 0,
            "elapsed_ms": 50,
            "state": "running"
        });

        let text = format_sh_interact(&result);
        assert!(text.starts_with("+ py send_input"), "header: {}", text);
        assert!(!text.contains('\n'), "empty output should not add newline: {}", text);
    }

    #[test]
    fn sh_interact_send_input_background_mode() {
        let result = json!({
            "alias": "py",
            "action": "send_input",
            "bytes_written": 42,
            "background": true,
            "state": "running"
        });

        let text = format_sh_interact(&result);
        assert_eq!(text, "+ py send_input(bg) 42B running");
    }

    #[test]
    fn sh_interact_signal() {
        let result = json!({
            "alias": "app",
            "action": "signal",
            "signal_sent": "SIGTERM",
            "state": "running"
        });

        let text = format_sh_interact(&result);
        assert_eq!(text, "+ app signal SIGTERM running");
    }

    // ── format_sh_session ───────────────────────────────────────────────

    #[test]
    fn sh_session_list() {
        let result = json!({
            "sessions": [
                {
                    "session": "main",
                    "shell": "/bin/zsh",
                    "cwd": "/Users/scott/projects/mish",
                    "active_processes": 2
                }
            ]
        });

        let text = format_sh_session(&result);
        assert!(text.starts_with("+ session list"), "header: {}", text);
        assert!(text.contains("main /bin/zsh"), "session info: {}", text);
        assert!(text.contains("(2 procs)"), "proc count: {}", text);
    }

    #[test]
    fn sh_session_create() {
        let result = json!({
            "session": "test-session",
            "shell": "/bin/bash",
            "cwd": "/tmp",
            "ready": true
        });

        let text = format_sh_session(&result);
        assert_eq!(text, "+ session create test-session ready");
    }

    #[test]
    fn sh_session_close() {
        let result = json!({
            "session": "test-session",
            "closed": true
        });

        let text = format_sh_session(&result);
        assert_eq!(text, "+ session close test-session");
    }

    // ── format_sh_help ──────────────────────────────────────────────────

    #[test]
    fn sh_help_reference_card() {
        let result = json!({
            "tools": [
                {
                    "name": "sh_run",
                    "params": [
                        { "name": "cmd", "type": "string", "required": true, "description": "Command to execute" }
                    ]
                }
            ],
            "watch_presets": { "errors": "(?i)(error|fail)" },
            "squasher_defaults": { "max_lines": 200, "oreo_head": 80, "oreo_tail": 40, "max_bytes": 32768 },
            "resource_limits": { "max_sessions": 4, "max_processes": 16 },
            "resource_usage": { "active_sessions": 1, "active_processes": 0 }
        });

        let text = format_sh_help(&result);
        assert!(text.contains("# mish reference card"), "title: {}", text);
        assert!(text.contains("## tools"), "tools section: {}", text);
        assert!(text.contains("sh_run"), "tool name: {}", text);
        assert!(text.contains("cmd*: string"), "required param: {}", text);
        assert!(text.contains("@errors"), "preset: {}", text);
        assert!(text.contains("max_lines:200"), "squasher: {}", text);
        assert!(text.contains("sessions:1/4"), "resources: {}", text);
    }

    // ── format_tool_response ────────────────────────────────────────────

    #[test]
    fn tool_response_with_digest() {
        let result = json!({
            "alias": "server",
            "action": "kill",
            "state": "exited"
        });
        let digest = vec![ProcessDigestEntry {
            alias: "bg".to_string(),
            session: "main".to_string(),
            state: "running".to_string(),
            pid: 999,
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
        }];

        let text = format_tool_response("sh_interact", &result, &digest);
        assert!(text.contains("- server killed exited"), "body: {}", text);
        assert!(text.contains("[procs] bg:running:5s"), "digest: {}", text);
    }

    #[test]
    fn tool_response_empty_digest() {
        let result = json!({
            "alias": "server",
            "action": "kill",
            "state": "exited"
        });

        let text = format_tool_response("sh_interact", &result, &[]);
        assert_eq!(text, "- server killed exited");
        assert!(!text.contains("[procs]"));
    }

    #[test]
    fn unknown_tool_falls_back_to_json() {
        let result = json!({ "foo": "bar" });
        let text = format_tool_response("sh_unknown", &result, &[]);
        assert!(text.contains("\"foo\""));
        assert!(text.contains("\"bar\""));
    }

    // ── llm_hint ──────────────────────────────────────────────────────

}
