/// Error enrichment on failure.
///
/// Pre-fetches diagnostics the LLM would request next: path walks, stat, permissions.
/// Budget: <100ms total, read-only, non-speculative.
pub struct Enricher;
