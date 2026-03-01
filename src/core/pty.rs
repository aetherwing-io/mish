/// PTY allocation and management.
///
/// Spawns child processes in a pseudoterminal via `nix::pty::forkpty()`.
pub struct PtyCapture;
