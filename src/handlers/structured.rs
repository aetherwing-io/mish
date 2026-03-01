/// Structured handler — execute, parse machine-readable output, format.
///
/// Handles git status, docker ps, etc. May inject --porcelain/--format json.
pub fn handle(_args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    todo!("Structured handler not yet implemented")
}
