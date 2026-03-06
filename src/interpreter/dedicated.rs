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
        self.write_raw_bytes(input.as_bytes()).await
    }

    /// Write raw bytes to the PTY stdin (takes `&[u8]` directly).
    pub async fn write_raw_bytes(&self, input: &[u8]) -> Result<usize, String> {
        let pty = self.pty.clone();
        let input = input.to_vec();

        tokio::task::spawn_blocking(move || {
            let pty = pty.lock().map_err(|e| format!("lock poisoned: {e}"))?;
            pty.write_stdin(&input)
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

    #[test]
    fn read_screen_returns_parser_contents() {
        // Directly test the parser integration without a real PTY
        let parser = vt100::Parser::new(24, 80, 0);
        let parser = Arc::new(Mutex::new(parser));

        // Feed some data
        {
            let mut p = parser.lock().unwrap();
            p.process(b"hello world\r\nline two");
        }

        let p = parser.lock().unwrap();
        let contents = p.screen().contents();
        assert!(contents.contains("hello world"), "contents: {contents}");
        assert!(contents.contains("line two"), "contents: {contents}");
    }

    #[test]
    fn parser_handles_cursor_movement() {
        let mut parser = vt100::Parser::new(24, 80, 0);

        // Write "ABCDE", move cursor back 3, overwrite with "XY"
        // Result should be "ABXYE" (not "ABCDEXY")
        parser.process(b"ABCDE\x1b[3DXY");

        let contents = parser.screen().contents();
        let first_line = contents.lines().next().unwrap_or("");
        assert_eq!(first_line.trim(), "ABXYE", "cursor overwrite: {contents}");
    }

    #[test]
    fn parser_handles_screen_clear() {
        let mut parser = vt100::Parser::new(24, 80, 0);

        parser.process(b"old content");
        parser.process(b"\x1b[2J\x1b[H"); // clear screen + home
        parser.process(b"new content");

        let contents = parser.screen().contents();
        assert!(!contents.contains("old content"), "should be cleared: {contents}");
        assert!(contents.contains("new content"), "should have new: {contents}");
    }

    #[test]
    fn parser_scrollback_preserves_history() {
        let mut parser = vt100::Parser::new(3, 80, 100);

        // Fill more lines than the screen can hold (3 rows)
        parser.process(b"line1\r\nline2\r\nline3\r\nline4\r\nline5\r\n");

        // Visible screen should have the latest lines
        let visible = parser.screen().contents();
        assert!(visible.contains("line5"), "visible should have line5: {visible}");

        // Scrollback should preserve earlier lines
        let screen = parser.screen_mut();
        screen.set_scrollback(usize::MAX);
        let scrolled = screen.contents();
        assert!(scrolled.contains("line1"), "scrollback should have line1: {scrolled}");
        screen.set_scrollback(0);
    }

    #[test]
    fn clear_and_write_replaces_spool_contents() {
        let spool = OutputSpool::new(1024);
        spool.write(b"old data");
        assert_eq!(spool.read_all(), b"old data");

        spool.clear_and_write(b"new snapshot");
        assert_eq!(spool.read_all(), b"new snapshot");
        assert!(!String::from_utf8_lossy(&spool.read_all()).contains("old data"));
    }
}
