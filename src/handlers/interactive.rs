/// Interactive handler — mode-aware.
///
/// CLI: detect raw mode -> transparent passthrough -> session summary on exit.
/// MCP: return error/warning (interactive commands can't run over MCP stdio).
pub fn handle(_args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    todo!("Interactive handler not yet implemented")
}
