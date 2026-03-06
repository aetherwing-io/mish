//! DedicatedPtyProcess — raw PTY wrapper for interactive/TUI processes.
//!
//! Unlike ManagedInterpreter, this has no sentinel/fence logic. All I/O
//! is raw: `write_raw` sends bytes directly to the PTY, `drain_to_spool`
//! reads available output with VTE stripping only (no dedup/truncation —
//! TUI output has meaningful repeated lines).

use std::sync::{Arc, Mutex};

use crate::core::pty::PtyCapture;
use crate::process::spool::OutputSpool;
use crate::squasher::vte_strip;

pub struct DedicatedPtyProcess {
    pty: Arc<Mutex<PtyCapture>>,
    spool: Arc<OutputSpool>,
}

impl DedicatedPtyProcess {
    pub fn new(pty: PtyCapture, spool: Arc<OutputSpool>) -> Self {
        Self {
            pty: Arc::new(Mutex::new(pty)),
            spool,
        }
    }

    /// Write raw bytes to the PTY stdin.
    pub async fn write_raw(&self, input: &str) -> Result<usize, String> {
        let pty = self.pty.clone();
        let input = input.to_string();

        tokio::task::spawn_blocking(move || {
            let pty = pty.lock().map_err(|e| format!("lock poisoned: {e}"))?;
            pty.write_stdin(input.as_bytes())
                .map_err(|e| format!("PTY write error: {e}"))
        })
        .await
        .map_err(|e| format!("spawn_blocking join error: {e}"))?
    }

    /// Drain available PTY output into the spool (non-blocking).
    /// Does VTE stripping only — no dedup or truncation.
    pub async fn drain_to_spool(&self) -> Result<(), String> {
        let pty = self.pty.clone();

        let drained = tokio::task::spawn_blocking(move || {
            let pty = pty.lock().map_err(|e| format!("lock poisoned: {e}"))?;
            let mut accumulated = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match pty.read_output(&mut buf) {
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
            let text = String::from_utf8_lossy(&drained);
            let clean = vte_strip::strip_ansi(&text);
            if !clean.is_empty() {
                self.spool.write(clean.as_bytes());
            }
        }

        Ok(())
    }

    /// Kill the child process.
    pub fn kill(&self) {
        if let Ok(pty) = self.pty.lock() {
            let _ = pty.signal(nix::sys::signal::Signal::SIGKILL);
        }
    }

    /// Check if the child process is still alive.
    pub fn is_alive(&self) -> bool {
        self.pty
            .lock()
            .map(|pty| pty.try_wait().map(|s| s.is_none()).unwrap_or(false))
            .unwrap_or(false)
    }

    /// Get the child's PID.
    pub fn pid(&self) -> u32 {
        self.pty
            .lock()
            .map(|pty| pty.pid().as_raw() as u32)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedicated_pty_process_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Arc<DedicatedPtyProcess>>();
    }
}
