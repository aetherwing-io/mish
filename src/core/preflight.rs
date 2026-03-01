/// Bidirectional argument injection.
///
/// Quiet: inject --quiet flags to reduce noise at source.
/// Verbose: inject -v flags to enrich terse commands.
/// Grammar-declared, never injects behavior-changing flags.
pub struct Preflight;
