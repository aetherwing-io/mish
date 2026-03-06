//! DedicatedPtyProcess — raw PTY wrapper for interactive/TUI processes.
//!
//! Uses a vt100 virtual terminal parser as the primary state. Raw PTY bytes
//! feed into the parser; `read_screen()` returns the current screen contents
//! (cursor movement, partial redraws, etc. are all resolved by the parser).
//! The spool receives extracted screen content for `read_full` compatibility.

use std::sync::{Arc, Mutex};

use crate::core::pty::PtyCapture;
use crate::process::spool::OutputSpool;

pub struct DedicatedPtyProcess {
    pty: Arc<Mutex<PtyCapture>>,
    parser: Arc<Mutex<vt100::Parser>>,
    spool: Arc<OutputSpool>,
}

impl DedicatedPtyProcess {
    pub fn new(pty: PtyCapture, spool: Arc<OutputSpool>) -> Self {
        Self {
            pty: Arc::new(Mutex::new(pty)),
            parser: Arc::new(Mutex::new(vt100::Parser::new(24, 80, 1000))),
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

    /// Drain available PTY output into the vt100 parser (non-blocking).
    /// Raw bytes feed the parser; screen contents are written to spool
    /// for `read_full` compatibility and size accounting.
    pub async fn drain_to_spool(&self) -> Result<(), String> {
        let pty = self.pty.clone();
        let parser = self.parser.clone();

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

            if !accumulated.is_empty() {
                let mut parser = parser.lock().map_err(|e| format!("lock poisoned: {e}"))?;
                parser.process(&accumulated);
            }

            Ok::<Vec<u8>, String>(accumulated)
        })
        .await
        .map_err(|e| format!("spawn_blocking join error: {e}"))??;

        // Write screen contents to spool (for read_full compatibility and size accounting)
        if !drained.is_empty() {
            let parser = self.parser.lock().map_err(|e| format!("lock poisoned: {e}"))?;
            let contents = parser.screen().contents();
            if !contents.is_empty() {
                self.spool.clear_and_write(contents.as_bytes());
            }
        }

        Ok(())
    }

    /// Read current screen contents from the virtual terminal.
    /// Returns clean text with TUI chrome resolved — cursor movement,
    /// partial redraws, etc. are all handled by the vt100 parser.
    pub fn read_screen(&self) -> Result<String, String> {
        let parser = self.parser.lock().map_err(|e| format!("lock poisoned: {e}"))?;
        // contents() returns rows x cols text with trailing whitespace trimmed
        Ok(parser.screen().contents())
    }

    /// Read screen contents including scrollback buffer.
    /// Temporarily scrolls back to show all history, reads contents, then resets.
    pub fn read_screen_full(&self) -> Result<String, String> {
        let mut parser = self.parser.lock().map_err(|e| format!("lock poisoned: {e}"))?;
        let screen = parser.screen_mut();
        let prev_offset = screen.scrollback();
        // Set scrollback to maximum — set_scrollback clamps to actual scrollback length
        screen.set_scrollback(usize::MAX);
        let contents = screen.contents();
        // Reset scrollback offset
        screen.set_scrollback(prev_offset);
        Ok(contents)
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
