pub mod categories;

/// Top-level category routing.
///
/// command -> grammar lookup -> categorize -> dispatch to handler
pub fn route(_command: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    todo!("Category router not yet implemented")
}
