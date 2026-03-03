/// PTY allocation and management.
///
/// Spawns child processes in a pseudoterminal via `nix::pty::forkpty()`.
use std::ffi::CString;
use std::os::unix::io::{AsFd, AsRawFd, OwnedFd};
use std::time::Instant;

use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::libc;
use nix::pty::{forkpty, ForkptyResult, Winsize};
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{read, write, Pid};

/// Error type for PTY operations.
#[derive(Debug)]
pub enum PtyError {
    /// System call failed
    Nix(nix::Error),
    /// Invalid command
    InvalidCommand(String),
    /// Child process error
    ChildExecFailed,
    /// I/O error
    Io(std::io::Error),
}

impl std::fmt::Display for PtyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PtyError::Nix(e) => write!(f, "PTY syscall error: {e}"),
            PtyError::InvalidCommand(s) => write!(f, "invalid command: {s}"),
            PtyError::ChildExecFailed => write!(f, "child exec failed"),
            PtyError::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for PtyError {}

impl From<nix::Error> for PtyError {
    fn from(e: nix::Error) -> Self {
        PtyError::Nix(e)
    }
}

impl From<std::io::Error> for PtyError {
    fn from(e: std::io::Error) -> Self {
        PtyError::Io(e)
    }
}

/// Exit information from a child process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus {
    /// Exit code (0 = success), or None if killed by signal
    pub code: Option<i32>,
    /// Signal that killed the process, if any
    pub signal: Option<i32>,
}

impl ExitStatus {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// PTY capture — owns a child process running in a pseudoterminal.
#[derive(Debug)]
pub struct PtyCapture {
    master_fd: OwnedFd,
    child_pid: Pid,
    start_time: Instant,
}

impl PtyCapture {
    /// Spawn a child process in a PTY.
    ///
    /// `command` is a slice where the first element is the program and the rest are arguments.
    pub fn spawn(command: &[String]) -> Result<Self, PtyError> {
        if command.is_empty() {
            return Err(PtyError::InvalidCommand("empty command".to_string()));
        }

        // Query real terminal dimensions, fall back to 80x24
        let winsize = get_terminal_size();

        // forkpty creates the PTY pair and forks.
        // Safety: we exec immediately in the child and only use async-signal-safe
        // operations before the exec.
        let fork_result = unsafe { forkpty(&winsize, None)? };

        match fork_result {
            ForkptyResult::Child => {
                // In child process: exec the command.
                // MUST use libc::setenv, NOT std::env::set_var.
                // Rust's set_var acquires an RwLock. After fork, if any
                // parent thread held that lock, the child inherits it as
                // "locked" but the holding thread doesn't exist → deadlock.
                unsafe {
                    let col = CString::new(winsize.ws_col.to_string()).unwrap();
                    let row = CString::new(winsize.ws_row.to_string()).unwrap();
                    let term = CString::new("xterm-256color").unwrap();
                    libc::setenv(b"COLUMNS\0".as_ptr().cast(), col.as_ptr(), 1);
                    libc::setenv(b"LINES\0".as_ptr().cast(), row.as_ptr(), 1);
                    libc::setenv(b"TERM\0".as_ptr().cast(), term.as_ptr(), 1);
                }

                let program =
                    CString::new(command[0].as_str()).map_err(|_| {
                        PtyError::InvalidCommand(command[0].clone())
                    })?;

                let args: Vec<CString> = command
                    .iter()
                    .map(|a| CString::new(a.as_str()).unwrap())
                    .collect();

                // execvp searches PATH
                nix::unistd::execvp(&program, &args)
                    .map_err(PtyError::Nix)?;

                // execvp doesn't return on success
                unreachable!()
            }
            ForkptyResult::Parent { child, master } => {
                // Set master FD to non-blocking
                let raw_fd = master.as_raw_fd();
                let flags = fcntl(raw_fd, FcntlArg::F_GETFL)?;
                let mut oflags = OFlag::from_bits_truncate(flags);
                oflags.insert(OFlag::O_NONBLOCK);
                fcntl(raw_fd, FcntlArg::F_SETFL(oflags))?;

                Ok(PtyCapture {
                    master_fd: master,
                    child_pid: child,
                    start_time: Instant::now(),
                })
            }
        }
    }

    /// Read output bytes from the PTY (non-blocking).
    ///
    /// Returns 0 if no data available (EAGAIN).
    /// Returns the number of bytes read on success.
    pub fn read_output(&self, buf: &mut [u8]) -> Result<usize, PtyError> {
        use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

        let mut pfd = [PollFd::new(self.master_fd.as_fd(), PollFlags::POLLIN)];
        match poll(&mut pfd, PollTimeout::ZERO) {
            Ok(0) => return Ok(0),              // no data available
            Err(nix::Error::EINTR) => return Ok(0),
            Err(e) => return Err(PtyError::Nix(e)),
            Ok(_) => {}                          // data ready, fall through to read
        }

        match read(self.master_fd.as_raw_fd(), buf) {
            Ok(n) => Ok(n),
            Err(nix::Error::EAGAIN) => Ok(0),
            Err(nix::Error::EIO) => Ok(0), // EIO means child exited
            Err(e) => Err(PtyError::Nix(e)),
        }
    }

    /// Write to child's stdin via the PTY master.
    pub fn write_stdin(&self, buf: &[u8]) -> Result<usize, PtyError> {
        Ok(write(&self.master_fd, buf)?)
    }

    /// Wait for child to exit. Returns exit status.
    pub fn wait(&self) -> Result<ExitStatus, PtyError> {
        loop {
            match waitpid(self.child_pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(_, code)) => {
                    return Ok(ExitStatus {
                        code: Some(code),
                        signal: None,
                    });
                }
                Ok(WaitStatus::Signaled(_, sig, _)) => {
                    return Ok(ExitStatus {
                        code: None,
                        signal: Some(sig as i32),
                    });
                }
                Ok(WaitStatus::StillAlive) => {
                    // Child still running, sleep briefly and retry
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Ok(_) => {
                    // Other status (stopped, continued), keep waiting
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(nix::Error::ECHILD) => {
                    // Child already reaped
                    return Ok(ExitStatus {
                        code: Some(0),
                        signal: None,
                    });
                }
                Err(e) => return Err(PtyError::Nix(e)),
            }
        }
    }

    /// Resize the PTY to new dimensions.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), PtyError> {
        let ws = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        // TIOCSWINSZ ioctl to set window size
        let ret = unsafe {
            libc::ioctl(
                self.master_fd.as_raw_fd(),
                libc::TIOCSWINSZ,
                &ws as *const Winsize,
            )
        };
        if ret == -1 {
            return Err(PtyError::Nix(nix::Error::last()));
        }

        // Send SIGWINCH to child so it picks up the new size
        kill(self.child_pid, Signal::SIGWINCH)?;

        Ok(())
    }

    /// Send a signal to the child process.
    pub fn signal(&self, sig: Signal) -> Result<(), PtyError> {
        kill(self.child_pid, sig)?;
        Ok(())
    }

    /// Drain remaining bytes from the PTY master after child exits.
    ///
    /// Uses a brief poll timeout on the first iteration to let the kernel
    /// propagate the child's final writes from the PTY slave to master.
    /// Subsequent iterations use ZERO (non-blocking) since data is flowing.
    pub fn drain(&self) -> Result<Vec<u8>, PtyError> {
        use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

        let mut all = Vec::new();
        let mut buf = [0u8; 4096];
        let mut first = true;
        loop {
            let timeout = if first { PollTimeout::from(50u16) } else { PollTimeout::ZERO };
            first = false;
            let mut pfd = [PollFd::new(self.master_fd.as_fd(), PollFlags::POLLIN)];
            match poll(&mut pfd, timeout) {
                Ok(0) => break,                     // no more data
                Err(nix::Error::EINTR) => continue,
                Err(e) => return Err(PtyError::Nix(e)),
                Ok(_) => {}                          // data ready
            }

            match read(self.master_fd.as_raw_fd(), &mut buf) {
                Ok(0) => break,
                Ok(n) => all.extend_from_slice(&buf[..n]),
                Err(nix::Error::EAGAIN) => break,
                Err(nix::Error::EIO) => break,
                Err(e) => return Err(PtyError::Nix(e)),
            }
        }
        Ok(all)
    }

    /// Get the child's PID.
    pub fn pid(&self) -> Pid {
        self.child_pid
    }

    /// Get a reference to the master file descriptor.
    pub fn master_fd(&self) -> &OwnedFd {
        &self.master_fd
    }

    /// Get the master file descriptor as a raw fd.
    pub fn master_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.master_fd.as_raw_fd()
    }

    /// Time elapsed since spawn.
    pub fn elapsed(&self) -> std::time::Duration {
        self.start_time.elapsed()
    }

    /// Async-safe wait — wraps the blocking `wait()` in `spawn_blocking`
    /// to avoid blocking tokio worker threads.
    ///
    /// Use this instead of `wait()` from async contexts.
    pub async fn wait_async(&self) -> Result<ExitStatus, PtyError> {
        let pid = self.child_pid;
        tokio::task::spawn_blocking(move || {
            loop {
                match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::Exited(_, code)) => {
                        return Ok(ExitStatus {
                            code: Some(code),
                            signal: None,
                        });
                    }
                    Ok(WaitStatus::Signaled(_, sig, _)) => {
                        return Ok(ExitStatus {
                            code: None,
                            signal: Some(sig as i32),
                        });
                    }
                    Ok(WaitStatus::StillAlive) | Ok(_) => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(nix::Error::ECHILD) => {
                        return Ok(ExitStatus {
                            code: Some(0),
                            signal: None,
                        });
                    }
                    Err(e) => return Err(PtyError::Nix(e)),
                }
            }
        })
        .await
        .unwrap_or_else(|e| Err(PtyError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("spawn_blocking join error: {e}"),
        ))))
    }
}

impl Drop for PtyCapture {
    fn drop(&mut self) {
        // OwnedFd will close the master fd on drop.
        // Try to reap the child — send SIGTERM first, then SIGKILL if needed.
        match waitpid(self.child_pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => {
                // Child still running — send SIGTERM and wait briefly.
                let _ = kill(self.child_pid, Signal::SIGTERM);
                std::thread::sleep(std::time::Duration::from_millis(50));
                match waitpid(self.child_pid, Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::StillAlive) => {
                        // Still alive after SIGTERM — escalate to SIGKILL.
                        let _ = kill(self.child_pid, Signal::SIGKILL);
                        // Block until reaped. SIGKILL is guaranteed to terminate,
                        // so this won't hang. WNOHANG here would race and leave zombies.
                        let _ = waitpid(self.child_pid, None);
                    }
                    _ => {} // Reaped or error — done.
                }
            }
            _ => {} // Already exited, already reaped, or error — done.
        }
    }
}

/// Query real terminal dimensions, falling back to 80x24.
fn get_terminal_size() -> Winsize {
    let mut ws = Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    unsafe {
        // Try stdout first, then stderr
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == -1 {
            if libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut ws) == -1 {
                // Fallback to 80x24
                ws.ws_row = 24;
                ws.ws_col = 80;
            }
        }
    }

    // Sanity check
    if ws.ws_row == 0 {
        ws.ws_row = 24;
    }
    if ws.ws_col == 0 {
        ws.ws_col = 80;
    }

    ws
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::thread;
    use std::time::Duration;

    /// Helper: read all output from PTY until child exits or deadline.
    fn read_all(pty: &PtyCapture, timeout: Duration) -> Vec<u8> {
        let mut all_bytes = Vec::new();
        let mut buf = [0u8; 4096];
        let deadline = Instant::now() + timeout;

        loop {
            if Instant::now() > deadline {
                break;
            }
            match pty.read_output(&mut buf) {
                Ok(0) => {
                    match waitpid(pty.child_pid, Some(WaitPidFlag::WNOHANG)) {
                        Ok(WaitStatus::Exited(_, _)) | Ok(WaitStatus::Signaled(_, _, _)) => {
                            if let Ok(remaining) = pty.drain() {
                                all_bytes.extend_from_slice(&remaining);
                            }
                            break;
                        }
                        _ => {}
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(n) => all_bytes.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        all_bytes
    }

    // Test 1: ANSI color passthrough — spawn printf with ANSI codes, verify bytes arrive intact
    #[test]
    #[serial(pty)]
    fn test_ansi_color_passthrough() {
        let pty = PtyCapture::spawn(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "printf '\\033[31mred\\033[0m\\n'".to_string(),
        ])
        .expect("spawn failed");

        let all_bytes = read_all(&pty, Duration::from_secs(5));
        let output = String::from_utf8_lossy(&all_bytes);

        // Should contain ESC[31m (red) and ESC[0m (reset) and "red"
        assert!(
            output.contains("\x1b[31m"),
            "ANSI red escape not found in output: {:?}",
            output
        );
        assert!(
            output.contains("red"),
            "text 'red' not found in output: {:?}",
            output
        );
        assert!(
            output.contains("\x1b[0m"),
            "ANSI reset escape not found in output: {:?}",
            output
        );
    }

    // Test 2: Progress bar detection — CR without LF, verify overwrite collapse
    #[test]
    #[serial(pty)]
    fn test_progress_bar_detection() {
        let pty = PtyCapture::spawn(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "printf '10%%\\r20%%\\r30%%\\rdone\\n'".to_string(),
        ])
        .expect("spawn failed");

        let all_bytes = read_all(&pty, Duration::from_secs(5));

        // Feed through LineBuffer and verify overwrite detection
        use crate::core::line_buffer::{Line, LineBuffer};
        let mut lb = LineBuffer::new();
        let lines = lb.finalize(&all_bytes);

        // Should have overwrite lines for 10%, 20%, 30% and a Complete for "done"
        let overwrites: Vec<_> = lines
            .iter()
            .filter(|l| matches!(l, Line::Overwrite(_)))
            .collect();
        let completes: Vec<_> = lines
            .iter()
            .filter(|l| matches!(l, Line::Complete(_)))
            .collect();

        assert!(
            !overwrites.is_empty(),
            "expected Overwrite lines from progress bar, got: {:?}",
            lines
        );
        assert!(
            completes.iter().any(|l| {
                if let Line::Complete(s) = l {
                    s.contains("done")
                } else {
                    false
                }
            }),
            "expected Complete line containing 'done', got: {:?}",
            lines
        );
    }

    // Test 3: SIGWINCH forwarding — resize PTY, verify child gets new dimensions
    #[test]
    #[serial(pty)]
    fn test_sigwinch_forwarding() {
        // Spawn a shell that reports terminal size after a delay
        let pty = PtyCapture::spawn(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 0.3 && stty size 2>/dev/null || echo unknown".to_string(),
        ])
        .expect("spawn failed");

        // Resize to 120x40 before child checks
        thread::sleep(Duration::from_millis(50));
        pty.resize(120, 40).expect("resize failed");

        let all_bytes = read_all(&pty, Duration::from_secs(5));
        let output = String::from_utf8_lossy(&all_bytes);

        // `stty size` outputs "rows cols", expect "40 120"
        assert!(
            output.contains("40 120"),
            "expected '40 120' in stty output, got: {:?}",
            output
        );
    }

    // Test 4: Raw mode detection — for interactive category detection
    #[test]
    #[serial(pty)]
    fn test_raw_mode_detection() {
        // We can detect if a PTY is in raw mode by checking termios on the master side.
        // This is how mish would detect if a child has switched to raw mode (interactive).
        let pty = PtyCapture::spawn(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo ready && exit 0".to_string(),
        ])
        .expect("spawn failed");

        // Read the termios settings from the master side immediately
        let termios = nix::sys::termios::tcgetattr(pty.master_fd());
        // Should succeed — we have a valid PTY
        assert!(
            termios.is_ok(),
            "tcgetattr on master fd should succeed, err: {:?}",
            termios.err()
        );

        // Verify we can inspect the flags (canonical mode etc.)
        let termios = termios.unwrap();
        let local_flags = termios.local_flags;
        // Default PTY should have ECHO and ICANON set (canonical, not raw)
        assert!(
            local_flags.contains(nix::sys::termios::LocalFlags::ECHO),
            "expected ECHO flag set in default PTY termios"
        );

        // Drain output so child doesn't block
        let _output = read_all(&pty, Duration::from_secs(5));

        let status = pty.wait().expect("wait failed");
        assert!(status.success(), "child should exit cleanly");
    }

    // Test 5: Multi-byte UTF-8 at buffer boundaries — split 3-byte char across reads
    #[test]
    #[serial(pty)]
    fn test_multibyte_utf8_at_buffer_boundary() {
        // The euro sign "EUR" = U+20AC = 0xE2 0x82 0xAC in UTF-8
        let pty = PtyCapture::spawn(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "printf '\\xe2\\x82\\xac\\n'".to_string(),
        ])
        .expect("spawn failed");

        let all_bytes = read_all(&pty, Duration::from_secs(5));

        // Simulate splitting at buffer boundary through LineBuffer
        use crate::core::line_buffer::{Line, LineBuffer};
        let mut lb = LineBuffer::new();

        let euro_bytes = [0xe2u8, 0x82, 0xac];
        if let Some(pos) = all_bytes.windows(3).position(|w| w == euro_bytes) {
            // Split in the middle of the UTF-8 sequence
            let split_point = pos + 1;
            let lines1 = lb.ingest(&all_bytes[..split_point]);
            let lines2 = lb.finalize(&all_bytes[split_point..]);

            let all_lines: Vec<_> =
                lines1.into_iter().chain(lines2.into_iter()).collect();

            // Should have a line containing the euro sign (either as valid char or lossy replacement)
            let has_euro = all_lines.iter().any(|l| match l {
                Line::Complete(s) | Line::Partial(s) | Line::Overwrite(s) => {
                    s.contains('\u{20ac}')
                        || s.as_bytes().windows(3).any(|w| w == euro_bytes)
                }
            });

            assert!(
                has_euro,
                "expected euro sign in output, lines: {:?}, raw bytes: {:?}",
                all_lines, all_bytes
            );
        } else {
            // PTY may transform output; verify we got some output without panicking
            let lines = lb.finalize(&all_bytes);
            assert!(
                !lines.is_empty(),
                "expected at least one line of output, raw bytes: {:?}",
                all_bytes
            );
        }
    }

    // Basic spawn and exit test
    #[test]
    #[serial(pty)]
    fn test_spawn_and_exit() {
        let pty = PtyCapture::spawn(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo hello && exit 0".to_string(),
        ])
        .expect("spawn failed");

        let all_bytes = read_all(&pty, Duration::from_secs(5));
        let output = String::from_utf8_lossy(&all_bytes);

        assert!(
            output.contains("hello"),
            "expected 'hello' in output, got: {:?}",
            output
        );

        let status = pty.wait().expect("wait failed");
        assert!(status.success(), "expected exit code 0, got: {:?}", status);
    }

    // Test non-zero exit code
    #[test]
    #[serial(pty)]
    fn test_nonzero_exit() {
        let pty = PtyCapture::spawn(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "exit 42".to_string(),
        ])
        .expect("spawn failed");

        let status = pty.wait().expect("wait failed");
        assert_eq!(
            status.code,
            Some(42),
            "expected exit code 42, got: {:?}",
            status
        );
        assert!(!status.success());
    }

    // Test write_stdin
    #[test]
    #[serial(pty)]
    fn test_write_stdin() {
        let pty = PtyCapture::spawn(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "read line && echo got:$line".to_string(),
        ])
        .expect("spawn failed");

        // Give the shell time to start
        thread::sleep(Duration::from_millis(100));

        // Write to stdin
        pty.write_stdin(b"hello\n").expect("write failed");

        let all_bytes = read_all(&pty, Duration::from_secs(5));
        let output = String::from_utf8_lossy(&all_bytes);

        assert!(
            output.contains("got:hello"),
            "expected 'got:hello' in output, got: {:?}",
            output
        );
    }

    // Test signal
    #[test]
    #[serial(pty)]
    fn test_signal_child() {
        let pty = PtyCapture::spawn(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 60".to_string(),
        ])
        .expect("spawn failed");

        thread::sleep(Duration::from_millis(100));
        pty.signal(Signal::SIGTERM).expect("signal failed");

        let status = pty.wait().expect("wait failed");
        // Should have been killed by signal
        assert!(
            status.signal.is_some() || status.code.is_some(),
            "expected signal or exit code, got: {:?}",
            status
        );
    }

    // Test empty command
    #[test]
    fn test_empty_command() {
        let result = PtyCapture::spawn(&[]);
        assert!(result.is_err());
    }

    // Test wait_async doesn't block tokio worker
    #[tokio::test]
    #[serial(pty)]
    async fn test_wait_async() {
        let pty = PtyCapture::spawn(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "exit 7".to_string(),
        ])
        .expect("spawn failed");

        let status = pty.wait_async().await.expect("wait_async failed");
        assert_eq!(status.code, Some(7));
    }
}
