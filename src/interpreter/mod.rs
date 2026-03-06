//! Shared interpreter module for PTY-backed REPL sessions.
//!
//! Used by both CLI sessions (`mish session`) and MCP interpreter mode
//! (`sh_spawn` + `sh_interact` for REPLs).

pub mod dedicated;
pub mod managed;
pub mod managed_process;
pub mod session;

pub use dedicated::DedicatedPtyProcess;
pub use managed::ManagedInterpreter;
pub use managed_process::ManagedProcess;
pub use session::{
    ExecuteResult, InterpreterKind, InterpreterSession, make_sentinel, squash_session_output,
    strip_prompt,
};

/// Known REPL basenames.
const REPL_BASENAMES: &[&str] = &[
    "python", "python3",
    "node",
    "psql", "mysql", "sqlite3",
    "irb", "ghci", "lua", "bc",
    "ruby", "php", "R", "julia",
];

/// Detect if a command string is a bare REPL invocation.
///
/// Returns true for commands like `python3`, `python3 -i`, `node`.
/// Returns false for `python3 script.py`, `node app.js`, `echo | python3`,
/// `cargo build`, etc.
pub fn is_repl_command(cmd: &str) -> bool {
    // Reject if pipes, redirects, or backgrounding present
    if cmd.contains('|') || cmd.contains('>') || cmd.contains('<') || cmd.contains('&') {
        return false;
    }

    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if parts.is_empty() {
        return false;
    }

    // Extract basename of the command
    let basename = parts[0].rsplit('/').next().unwrap_or(parts[0]);

    // Check for known REPL basenames (exact match or python version variants)
    let is_known = REPL_BASENAMES.contains(&basename)
        || basename.starts_with("python3.");

    if !is_known {
        return false;
    }

    // Remaining args: flags (starting with -) are OK, positional args are not
    for arg in &parts[1..] {
        if !arg.starts_with('-') {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_repl_python3() {
        assert!(is_repl_command("python3"));
    }

    #[test]
    fn is_repl_python3_interactive() {
        assert!(is_repl_command("python3 -i"));
    }

    #[test]
    fn is_repl_python3_script_not_repl() {
        assert!(!is_repl_command("python3 script.py"));
    }

    #[test]
    fn is_repl_node() {
        assert!(is_repl_command("node"));
    }

    #[test]
    fn is_repl_node_app_not_repl() {
        assert!(!is_repl_command("node app.js"));
    }

    #[test]
    fn is_repl_cargo_build_not_repl() {
        assert!(!is_repl_command("cargo build"));
    }

    #[test]
    fn is_repl_piped_python_not_repl() {
        assert!(!is_repl_command("echo | python3"));
    }

    #[test]
    fn is_repl_python_with_path() {
        assert!(is_repl_command("/usr/bin/python3"));
    }

    #[test]
    fn is_repl_python_version_variant() {
        assert!(is_repl_command("python3.11"));
        assert!(is_repl_command("python3.12"));
    }

    #[test]
    fn is_repl_psql() {
        assert!(is_repl_command("psql"));
    }

    #[test]
    fn is_repl_sqlite3() {
        assert!(is_repl_command("sqlite3"));
    }

    #[test]
    fn is_repl_irb() {
        assert!(is_repl_command("irb"));
    }

    #[test]
    fn is_repl_empty_not_repl() {
        assert!(!is_repl_command(""));
    }

    #[test]
    fn is_repl_redirect_not_repl() {
        assert!(!is_repl_command("python3 > out.txt"));
    }

    #[test]
    fn is_repl_background_not_repl() {
        assert!(!is_repl_command("python3 &"));
    }
}
