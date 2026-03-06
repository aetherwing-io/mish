//! ManagedInterpreter — async wrapper for InterpreterSession.
//!
//! Wraps the blocking PTY-based InterpreterSession in an Arc<Mutex> and
//! provides async methods via `spawn_blocking`. Writes output to an
//! OutputSpool after each execute call so `read_tail` works.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::interpreter::session::{ExecuteResult, InterpreterSession};
use crate::process::spool::OutputSpool;

pub struct ManagedInterpreter {
    session: Arc<Mutex<InterpreterSession>>,
    spool: Arc<OutputSpool>,
    has_bg_pending: AtomicBool,
}

impl ManagedInterpreter {
    pub fn new(session: InterpreterSession, spool: Arc<OutputSpool>) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            spool,
            has_bg_pending: AtomicBool::new(false),
        }
    }

    /// Execute input in the interpreter asynchronously.
    ///
    /// Uses `spawn_blocking` because `InterpreterSession::execute` does
    /// blocking PTY I/O. Writes output to the spool after execution.
    ///
    /// If background output is pending (`has_bg_pending`), automatically
    /// calls `drain_and_fence` first to capture bg output and clean the PTY.
    pub async fn execute(
        &self,
        input: &str,
        timeout: Duration,
    ) -> Result<ExecuteResult, String> {
        let session = self.session.clone();
        let input = input.to_string();
        let had_bg = self.has_bg_pending.swap(false, Ordering::AcqRel);
        let spool = self.spool.clone();

        let result = tokio::task::spawn_blocking(move || {
            let session = session.lock().map_err(|e| format!("lock poisoned: {e}"))?;

            // Fence: drain bg output before running fg command
            if had_bg {
                let fence_timeout = Duration::from_secs(60);
                let bg_output = session.drain_and_fence(fence_timeout)?;
                if !bg_output.is_empty() {
                    let mut data = bg_output.into_bytes();
                    data.push(b'\n');
                    spool.write(&data);
                }
            }

            session.execute(&input, timeout)
        })
        .await
        .map_err(|e| format!("spawn_blocking join error: {e}"))??;

        // Write output to spool so read_tail sees it
        if !result.output.is_empty() {
            let mut spool_data = result.output.as_bytes().to_vec();
            spool_data.push(b'\n');
            self.spool.write(&spool_data);
        }

        Ok(result)
    }

    /// Write raw input to the interpreter without sentinel wrapping.
    /// Returns the number of bytes written. Sets `has_bg_pending` flag.
    pub async fn write_raw(&self, input: &str) -> Result<usize, String> {
        let session = self.session.clone();
        let input = input.to_string();

        let bytes_written = tokio::task::spawn_blocking(move || {
            let session = session.lock().map_err(|e| format!("lock poisoned: {e}"))?;
            session.write_raw(&input)
        })
        .await
        .map_err(|e| format!("spawn_blocking join error: {e}"))??;

        self.has_bg_pending.store(true, Ordering::Release);
        Ok(bytes_written)
    }

    /// Drain available PTY output into the spool (non-blocking).
    /// Used by `read_tail`/`read_full` to capture in-flight background output.
    pub async fn drain_to_spool(&self) -> Result<(), String> {
        let session = self.session.clone();

        let drained = tokio::task::spawn_blocking(move || {
            let session = session.lock().map_err(|e| format!("lock poisoned: {e}"))?;
            let mut accumulated = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match session.read_pty_nonblocking(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => accumulated.extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
            Ok::<Vec<u8>, String>(accumulated)
        })
        .await
        .map_err(|e| format!("spawn_blocking join error: {e}"))??;

        if !drained.is_empty() {
            // VTE strip before writing to spool
            let text = String::from_utf8_lossy(&drained);
            let clean = crate::squasher::vte_strip::strip_ansi(&text);
            if !clean.is_empty() {
                self.spool.write(clean.as_bytes());
            }
        }

        Ok(())
    }

    pub fn kill(&self) {
        if let Ok(session) = self.session.lock() {
            session.kill();
        }
    }

    pub fn is_alive(&self) -> bool {
        self.session
            .lock()
            .map(|s| s.is_alive())
            .unwrap_or(false)
    }

    pub fn pid(&self) -> u32 {
        self.session
            .lock()
            .map(|s| s.pid())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::spool::SpoolManager;

    #[test]
    fn managed_interpreter_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Arc<ManagedInterpreter>>();
    }

    #[test]
    fn new_creates_instance() {
        // Can't easily test without a real PTY, but we can verify
        // the spool manager creates spools correctly
        let mut mgr = SpoolManager::new(10_000_000);
        let spool = mgr.create_spool("test", 4096).unwrap();
        // Just verify the spool works
        spool.write(b"hello");
        assert_eq!(spool.read_all(), b"hello");
    }
}
