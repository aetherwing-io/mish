//! ManagedProcess — enum wrapper for interpreter and dedicated PTY processes.
//!
//! Simple match dispatch — no trait objects, no async_trait dependency.
//! Both variants are Send + Sync, so `Arc<ManagedProcess>` is too.

use crate::interpreter::dedicated::DedicatedPtyProcess;
use crate::interpreter::managed::ManagedInterpreter;
use crate::interpreter::session::ExecuteResult;

use std::time::Duration;

pub enum ManagedProcess {
    Interpreter(ManagedInterpreter),
    Dedicated(DedicatedPtyProcess),
}

impl ManagedProcess {
    /// Write raw bytes to the underlying PTY.
    pub async fn write_raw(&self, input: &str) -> Result<usize, String> {
        match self {
            ManagedProcess::Interpreter(i) => i.write_raw(input).await,
            ManagedProcess::Dedicated(d) => d.write_raw(input).await,
        }
    }

    /// Drain available PTY output to the spool.
    pub async fn drain_to_spool(&self) -> Result<(), String> {
        match self {
            ManagedProcess::Interpreter(i) => i.drain_to_spool().await,
            ManagedProcess::Dedicated(d) => d.drain_to_spool().await,
        }
    }

    /// Kill the child process.
    pub fn kill(&self) {
        match self {
            ManagedProcess::Interpreter(i) => i.kill(),
            ManagedProcess::Dedicated(d) => d.kill(),
        }
    }

    /// Check if the child process is still alive.
    pub fn is_alive(&self) -> bool {
        match self {
            ManagedProcess::Interpreter(i) => i.is_alive(),
            ManagedProcess::Dedicated(d) => d.is_alive(),
        }
    }

    /// Get the child's PID.
    pub fn pid(&self) -> u32 {
        match self {
            ManagedProcess::Interpreter(i) => i.pid(),
            ManagedProcess::Dedicated(d) => d.pid(),
        }
    }

    /// Read current screen contents (dedicated PTY only).
    /// Returns None for interpreter processes (they use spool).
    pub fn read_screen(&self) -> Option<Result<String, String>> {
        match self {
            ManagedProcess::Dedicated(d) => Some(d.read_screen()),
            ManagedProcess::Interpreter(_) => None,
        }
    }

    /// Read full screen contents including scrollback (dedicated PTY only).
    pub fn read_screen_full(&self) -> Option<Result<String, String>> {
        match self {
            ManagedProcess::Dedicated(d) => Some(d.read_screen_full()),
            ManagedProcess::Interpreter(_) => None,
        }
    }

    /// Whether this process supports sentinel-wrapped `execute()`.
    /// Only interpreters do — dedicated PTY processes are raw I/O only.
    pub fn supports_execute(&self) -> bool {
        matches!(self, ManagedProcess::Interpreter(_))
    }

    /// Execute input with sentinel wrapping (interpreter mode only).
    /// Returns `Err` if called on a dedicated PTY process.
    pub async fn execute(
        &self,
        input: &str,
        timeout: Duration,
    ) -> Result<ExecuteResult, String> {
        match self {
            ManagedProcess::Interpreter(i) => i.execute(input, timeout).await,
            ManagedProcess::Dedicated(_) => {
                Err("execute() not supported on dedicated PTY processes".to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn managed_process_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Arc<ManagedProcess>>();
    }
}
