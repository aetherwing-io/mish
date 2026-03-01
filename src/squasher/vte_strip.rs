/// VTE-based ANSI stripping.
///
/// Uses the `vte` crate state machine to parse terminal sequences and extract printable text.
pub struct VteStripper;
