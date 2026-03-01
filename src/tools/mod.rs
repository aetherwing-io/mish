pub mod sh_help;
pub mod sh_interact;
pub mod sh_run;
pub mod sh_session;
pub mod sh_spawn;

use crate::mcp::types::{ERR_ALIAS_NOT_FOUND, ERR_INTERNAL, ERR_INVALID_ACTION, ERR_INVALID_PARAMS};
use crate::process::table::ProcessTableError;
use crate::session::manager::SessionError;

/// Unified error type returned from tool handlers, carrying an MCP error code.
#[derive(Debug)]
pub struct ToolError {
    pub code: i32,
    pub message: String,
}

impl ToolError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self::new(ERR_INVALID_PARAMS, msg)
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(ERR_INTERNAL, msg)
    }

    pub fn alias_not_found(alias: &str) -> Self {
        Self::new(ERR_ALIAS_NOT_FOUND, format!("process alias not found: {alias}"))
    }

    pub fn invalid_action(msg: impl Into<String>) -> Self {
        Self::new(ERR_INVALID_ACTION, msg)
    }

    pub fn from_session_error(e: SessionError) -> Self {
        Self {
            code: e.error_code(),
            message: e.to_string(),
        }
    }

    pub fn from_process_table_error(e: &ProcessTableError) -> Self {
        Self {
            code: e.error_code(),
            message: e.to_string(),
        }
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for ToolError {}

impl From<ProcessTableError> for ToolError {
    fn from(e: ProcessTableError) -> Self {
        ToolError::new(e.error_code(), e.to_string())
    }
}

impl From<SessionError> for ToolError {
    fn from(e: SessionError) -> Self {
        ToolError {
            code: e.error_code(),
            message: e.to_string(),
        }
    }
}
