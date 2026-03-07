//! sh_lock — coordination primitive for agent orchestration.

use serde::Deserialize;

use crate::mcp::types::{ERR_INVALID_PARAMS, ProcessDigestEntry};
use crate::process::state::ProcessState;
use crate::process::table::{ProcessTable, DigestMode};
use super::ToolError;

#[derive(Debug, Deserialize)]
pub struct ShLockParams {
    pub action: String,
    pub name: String,
    #[serde(default)]
    pub timeout: Option<u64>,
}

pub async fn handle(
    params: ShLockParams,
    process_table: &tokio::sync::RwLock<ProcessTable>,
) -> Result<(serde_json::Value, Vec<ProcessDigestEntry>), ToolError> {
    match params.action.as_str() {
        "create" => {
            let mut pt = process_table.write().await;
            pt.register(&params.name, "lock", 0, None)
                .map_err(|e| ToolError::from_process_table_error(&e))?;
            let digest = pt.digest(DigestMode::Changed);
            Ok((serde_json::json!({"lock": params.name, "status": "created"}), digest))
        }
        "release" => {
            let mut pt = process_table.write().await;
            pt.update_state(&params.name, ProcessState::Completed)
                .map_err(|e| ToolError::from_process_table_error(&e))?;
            let digest = pt.digest(DigestMode::Changed);
            Ok((serde_json::json!({"lock": params.name, "status": "released"}), digest))
        }
        "watch" => {
            let timeout_secs = params.timeout.unwrap_or(300);
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
            loop {
                {
                    let mut pt = process_table.write().await;
                    let state = pt.get(&params.name).map(|e| (e.state, e.state.is_terminal()));
                    match state {
                        Some((s, true)) => {
                            let digest = pt.digest(DigestMode::Changed);
                            return Ok((serde_json::json!({"lock": params.name, "status": "released", "final_state": s.as_str()}), digest));
                        }
                        None => {
                            let digest = pt.digest(DigestMode::Changed);
                            return Ok((serde_json::json!({"lock": params.name, "status": "not_found"}), digest));
                        }
                        _ => {} // still running, keep waiting
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    let mut pt = process_table.write().await;
                    let digest = pt.digest(DigestMode::Changed);
                    return Ok((serde_json::json!({"lock": params.name, "status": "timeout"}), digest));
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
        "status" => {
            let mut pt = process_table.write().await;
            let state_str = pt.get(&params.name)
                .map(|e| e.state.as_str().to_string())
                .unwrap_or_else(|| "not_found".to_string());
            let digest = pt.digest(DigestMode::Changed);
            Ok((serde_json::json!({"lock": params.name, "state": state_str}), digest))
        }
        _ => Err(ToolError::new(ERR_INVALID_PARAMS, format!("Unknown lock action: {}", params.action))),
    }
}
