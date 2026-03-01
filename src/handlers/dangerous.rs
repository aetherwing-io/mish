/// Dangerous handler — mode-aware.
///
/// CLI: warn on terminal -> prompt human -> maybe execute.
/// MCP: return structured warning -> policy engine -> LLM decides or escalates.
pub fn handle(_args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    todo!("Dangerous handler not yet implemented")
}
