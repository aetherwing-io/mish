/// Emit buffer — accumulates classified lines and flushes on triggers.
///
/// Flush triggers: process exit, hazard detected, prompt detected, silence, periodic timer.
pub struct EmitBuffer;
