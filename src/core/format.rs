/// Output formatting.
///
/// Modes: human (default), json, passthrough, context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Human,
    Json,
    Passthrough,
    Context,
}

impl Default for OutputMode {
    fn default() -> Self {
        OutputMode::Human
    }
}
