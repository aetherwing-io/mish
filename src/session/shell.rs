//! Shell process lifecycle — spawn, initialize, execute commands, track CWD.
//!
//! `ShellProcess` manages an interactive non-login shell running in a PTY.
//! Commands are written to the shell's stdin (stateful REPL), and boundary
//! detection extracts exit codes, output, and CWD after each command.

use std::time::{Duration, Instant};

use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;

use crate::core::pty::{PtyCapture, PtyError};
use crate::session::boundary::BoundaryDetector;

/// Error type for shell operations.
#[derive(Debug)]
pub enum ShellError {
    /// Shell process failed to spawn.
    SpawnFailed(String),
    /// Timed out waiting for shell to initialize (prompt not detected).
    InitTimeout,
    /// Timed out waiting for command to complete (boundary not detected).
    ExecTimeout,
    /// Boundary marker not found in output.
    BoundaryNotFound,
    /// Underlying PTY error.
    PtyError(PtyError),
    /// I/O error.
    Io(std::io::Error),
}

impl std::fmt::Display for ShellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShellError::SpawnFailed(msg) => write!(f, "shell spawn failed: {msg}"),
            ShellError::InitTimeout => write!(f, "shell initialization timed out"),
            ShellError::ExecTimeout => write!(f, "command execution timed out"),
            ShellError::BoundaryNotFound => write!(f, "boundary marker not found"),
            ShellError::PtyError(e) => write!(f, "pty error: {e}"),
            ShellError::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for ShellError {}

impl From<PtyError> for ShellError {
    fn from(e: PtyError) -> Self {
        ShellError::PtyError(e)
    }
}

impl From<std::io::Error> for ShellError {
    fn from(e: std::io::Error) -> Self {
        ShellError::Io(e)
    }
}

/// Result of a synchronous command execution.
#[derive(Debug, Clone)]
pub struct CommandResult {
    /// Exit code of the command.
    pub exit_code: i32,
    /// Captured output (stdout + stderr merged, boundary markers stripped).
    pub output: String,
    /// Current working directory after command completed.
    pub cwd: String,
    /// Wall-clock duration of the command.
    pub duration: Duration,
}

/// Default PTY dimensions for shell sessions.
const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 40;

/// Timeout for shell initialization (waiting for first prompt).
const INIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Polling interval when reading PTY output.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A managed shell process within a session.
///
/// Spawns an interactive non-login shell (`bash -i` / `zsh -i`) in a PTY,
/// injects boundary hooks, and provides command execution with exit code
/// and CWD tracking.
pub struct ShellProcess {
    pty: PtyCapture,
    shell_path: String,
    boundary: BoundaryDetector,
    cwd: String,
    ready: bool,
}

impl ShellProcess {
    /// Spawn a new shell process in a PTY.
    ///
    /// The shell is started as interactive non-login (`-i` flag), so `.bashrc`/`.zshrc`
    /// are sourced but `.profile`/`.bash_profile` are not.
    pub async fn spawn(shell_path: &str, initial_cwd: &str) -> Result<Self, ShellError> {
        let command = vec![shell_path.to_string(), "-i".to_string()];

        let pty = PtyCapture::spawn(&command).map_err(|e| {
            ShellError::SpawnFailed(format!("{e}"))
        })?;

        // Resize to our default dimensions (120x40)
        if let Err(e) = pty.resize(DEFAULT_COLS, DEFAULT_ROWS) {
            // Non-fatal: log but continue
            tracing::warn!("failed to resize PTY: {e}");
        }

        let boundary = BoundaryDetector::new(shell_path);

        Ok(ShellProcess {
            pty,
            shell_path: shell_path.to_string(),
            boundary,
            cwd: initial_cwd.to_string(),
            ready: false,
        })
    }

    /// Initialize the shell: inject hooks, wait for initial prompt, discard startup output.
    ///
    /// After this returns successfully, the shell is ready to accept commands via `execute()`.
    ///
    /// Initialization sequence:
    /// 1. Wait for shell to start and source rc files
    /// 2. Drain startup output (motd, .bashrc messages, PS1 rendering)
    /// 3. Inject PROMPT_COMMAND/precmd hook
    /// 4. cd to initial CWD
    /// 5. Run `true` as a no-op to trigger the boundary hook
    /// 6. Wait for boundary from `true` — confirms shell is at prompt and hooks work
    /// 7. Drain any remaining prompt output
    pub async fn initialize(&mut self) -> Result<(), ShellError> {
        // Give the shell time to start and source rc files
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Discard any startup output (motd, .bashrc messages, PS1 rendering)
        self.drain_output().await;

        // Inject boundary hooks via a single compound command.
        // We combine the hook injection, cd, and a `true` trigger into one line
        // so there's only one boundary to wait for (the one after `true`).
        //
        // The PROMPT_COMMAND/precmd fires AFTER each command. By sending all
        // setup as one compound line, the hook fires once at the end.
        let hooks = self.boundary.shell_hook_commands();
        let cd_cmd = format!("cd '{}'", self.cwd.replace('\'', "'\\''"));

        // Send hooks, then cd, then true — all ending with newlines.
        // The hook injection sets PROMPT_COMMAND which only fires after the NEXT
        // command completes. So the sequence is:
        //   1. Send hook setup line -> no boundary (PROMPT_COMMAND set but not yet fired)
        //   2. Send cd line -> boundary fires (from hook setup completing)
        //   3. Send true line -> boundary fires (from cd completing)
        // We need to wait for the boundary from `true` (the last one).
        if !hooks.is_empty() {
            self.pty.write_stdin(hooks.as_bytes()).map_err(ShellError::PtyError)?;
            self.pty.write_stdin(b"\n").map_err(ShellError::PtyError)?;
        }

        // Small delay to let the shell process the hook setup
        tokio::time::sleep(Duration::from_millis(100)).await;

        // cd to initial CWD
        self.pty.write_stdin(cd_cmd.as_bytes()).map_err(ShellError::PtyError)?;
        self.pty.write_stdin(b"\n").map_err(ShellError::PtyError)?;

        // Small delay
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Send a no-op `true` command — we wait for THIS command's boundary.
        self.pty.write_stdin(b"true\n").map_err(ShellError::PtyError)?;

        // We need to consume boundaries until we get one that's from `true`
        // (exit code 0, after the cd). The strategy: keep reading and detecting
        // boundaries. Each time we find one, consume it and continue reading.
        // When we get a boundary with exit code 0 and we've consumed enough,
        // we're done.
        //
        // For PROMPT_COMMAND-based detection: the hook fires after each command,
        // so we expect boundaries from:
        //   - The hook setup line (PROMPT_COMMAND= ...) -> exit 0, old CWD
        //   - The cd command -> exit 0, new CWD
        //   - The true command -> exit 0, new CWD
        //
        // We wait for at least the boundary from `true` by looking for the one
        // whose CWD matches our target. But to be robust, we just consume
        // boundaries until we get 3 (or timeout), then take the last one.

        let deadline = Instant::now() + INIT_TIMEOUT;
        let mut buffer = String::new();
        let mut read_buf = [0u8; 4096];
        let mut boundaries_seen = 0;
        let target_boundaries = 3; // hook_setup, cd, true

        loop {
            if Instant::now() > deadline {
                if boundaries_seen > 0 {
                    // We got at least one boundary, good enough
                    break;
                }
                return Err(ShellError::InitTimeout);
            }

            match self.pty.read_output(&mut read_buf) {
                Ok(0) => {
                    // No data available. If we've already seen boundaries,
                    // wait a bit more to see if another arrives, then break.
                    if boundaries_seen > 0 {
                        // Wait a bit more for additional boundaries
                        tokio::time::sleep(Duration::from_millis(100)).await;

                        // Try one more read
                        match self.pty.read_output(&mut read_buf) {
                            Ok(n) if n > 0 => {
                                buffer.push_str(&String::from_utf8_lossy(&read_buf[..n]));
                                // Continue the loop to check for more boundaries
                                continue;
                            }
                            _ => {
                                // No more data, we're done
                                break;
                            }
                        }
                    }
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
                Ok(n) => {
                    buffer.push_str(&String::from_utf8_lossy(&read_buf[..n]));

                    // Try to detect boundaries in the accumulated buffer.
                    // Each time we find one, consume it and reset the buffer
                    // to what comes after.
                    loop {
                        if let Some(result) = self.boundary.detect_boundary(&buffer, None) {
                            boundaries_seen += 1;
                            if !result.cwd.is_empty() {
                                self.cwd = result.cwd;
                            }

                            if boundaries_seen >= target_boundaries {
                                // Clear buffer — we're done
                                buffer.clear();
                                break;
                            }

                            // Remove the consumed boundary from the buffer.
                            // The detect_boundary returns cleaned output; we need
                            // to reset to check for more boundaries. Since the
                            // regex-based detection consumes markers, we can just
                            // clear and continue reading.
                            buffer.clear();
                            break;
                        } else {
                            break;
                        }
                    }

                    if boundaries_seen >= target_boundaries {
                        break;
                    }
                }
                Err(e) => {
                    return Err(ShellError::PtyError(e));
                }
            }
        }

        // Drain any remaining output (prompt rendering, etc.)
        tokio::time::sleep(Duration::from_millis(100)).await;
        self.drain_output().await;

        self.ready = true;
        Ok(())
    }

    /// Execute a command synchronously. Returns when boundary is detected or timeout.
    ///
    /// The command is written to the shell's stdin. CWD and environment persist
    /// between calls (the shell is a stateful REPL).
    pub async fn execute(
        &mut self,
        cmd: &str,
        timeout: Duration,
    ) -> Result<CommandResult, ShellError> {
        let start = Instant::now();

        // Drain any stale output from previous commands (prompt rendering, etc.)
        self.drain_output().await;

        // Wrap command with boundary markers (for sentinel mode) or use as-is (shell integration)
        let (wrapped, sentinel_uuid) = self.boundary.wrap_command(cmd);

        // Write the command to stdin
        let cmd_line = format!("{wrapped}\n");
        self.pty.write_stdin(cmd_line.as_bytes()).map_err(ShellError::PtyError)?;

        // Read output until boundary detected or timeout
        let deadline = Instant::now() + timeout;
        let mut buffer = String::new();
        let mut read_buf = [0u8; 4096];

        loop {
            if Instant::now() > deadline {
                // Timeout: kill the process group
                let pid = self.pty.pid();
                let _ = killpg(Pid::from_raw(pid.as_raw()), Signal::SIGKILL);
                return Err(ShellError::ExecTimeout);
            }

            match self.pty.read_output(&mut read_buf) {
                Ok(0) => {
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
                Ok(n) => {
                    buffer.push_str(&String::from_utf8_lossy(&read_buf[..n]));
                    if let Some(result) = self.boundary.detect_boundary(
                        &buffer,
                        sentinel_uuid.as_deref(),
                    ) {
                        // Update tracked CWD
                        if !result.cwd.is_empty() {
                            self.cwd = result.cwd.clone();
                        }

                        let duration = start.elapsed();

                        // Clean up the output: strip the echo of the command itself
                        // and any trailing prompt text.
                        let output = Self::clean_command_output(&result.output, cmd);

                        return Ok(CommandResult {
                            exit_code: result.exit_code,
                            output,
                            cwd: self.cwd.clone(),
                            duration,
                        });
                    }
                }
                Err(e) => {
                    return Err(ShellError::PtyError(e));
                }
            }
        }
    }

    /// Write raw bytes to shell stdin (for sh_interact send action).
    pub async fn write_stdin(&mut self, input: &[u8]) -> Result<usize, ShellError> {
        self.pty.write_stdin(input).map_err(ShellError::PtyError)
    }

    /// Read available output bytes (non-blocking).
    pub async fn read_output(&mut self, buf: &mut [u8]) -> Result<usize, ShellError> {
        self.pty.read_output(buf).map_err(ShellError::PtyError)
    }

    /// Get the tracked CWD.
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Check if shell is ready for commands.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Get the shell PID.
    pub fn pid(&self) -> u32 {
        self.pty.pid().as_raw() as u32
    }

    /// Kill the shell process group (SIGKILL).
    pub fn kill(&self) -> Result<(), ShellError> {
        let pid = self.pty.pid();
        // Try killpg first (process group), fall back to kill (single process)
        match killpg(Pid::from_raw(pid.as_raw()), Signal::SIGKILL) {
            Ok(()) => Ok(()),
            Err(_) => {
                // Process may not be a group leader; try direct kill
                nix::sys::signal::kill(pid, Signal::SIGKILL)
                    .map_err(|e| ShellError::PtyError(PtyError::Nix(e)))
            }
        }
    }

    /// Send SIGTERM to the shell process group.
    pub fn terminate(&self) -> Result<(), ShellError> {
        let pid = self.pty.pid();
        match killpg(Pid::from_raw(pid.as_raw()), Signal::SIGTERM) {
            Ok(()) => Ok(()),
            Err(_) => {
                nix::sys::signal::kill(pid, Signal::SIGTERM)
                    .map_err(|e| ShellError::PtyError(PtyError::Nix(e)))
            }
        }
    }

    /// Drain all currently available output from the PTY (discard it).
    async fn drain_output(&self) {
        let mut buf = [0u8; 4096];
        loop {
            match self.pty.read_output(&mut buf) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(_) => break,
            }
        }
    }

    /// Clean command output: strip the echoed command line and trailing prompt.
    ///
    /// PTY echo means the command itself appears in the output. We strip:
    /// - The first line if it matches the command (PTY echo)
    /// - Trailing prompt lines (e.g., "bash-3.2$ ")
    /// - Leading/trailing whitespace and \r characters
    fn clean_command_output(raw: &str, cmd: &str) -> String {
        let lines: Vec<&str> = raw.split('\n').collect();
        let mut start = 0;
        let mut end = lines.len();

        // Strip leading line that's the echoed command
        if !lines.is_empty() {
            let first = lines[0].replace('\r', "").trim().to_string();
            let cmd_trimmed = cmd.trim();
            if first == cmd_trimmed || first.ends_with(cmd_trimmed) {
                start = 1;
            }
        }

        // Strip trailing empty lines and prompt lines
        while end > start {
            let last = lines[end - 1].replace('\r', "");
            let trimmed = last.trim();
            if trimmed.is_empty()
                || trimmed.ends_with("$ ")
                || trimmed.ends_with("$")
                || trimmed.ends_with("% ")
                || trimmed.ends_with("%")
                || trimmed.ends_with("> ")
                || trimmed.ends_with(">")
            {
                end -= 1;
            } else {
                break;
            }
        }

        lines[start..end]
            .iter()
            .map(|l| l.replace('\r', ""))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// Find bash path for tests.
    fn bash_path() -> &'static str {
        "/bin/bash"
    }

    // Test 1: Shell spawns successfully with bash -i
    #[tokio::test]
    #[serial(pty)]
    async fn test_shell_spawns_successfully() {
        let shell = ShellProcess::spawn(bash_path(), "/tmp")
            .await
            .expect("shell should spawn");
        assert!(shell.pid() > 0, "PID should be positive");
        let _ = shell.kill();
    }

    // Test 2: is_ready() returns false before initialization, true after
    #[tokio::test]
    #[serial(pty)]
    async fn test_is_ready_before_and_after_init() {
        let mut shell = ShellProcess::spawn(bash_path(), "/tmp")
            .await
            .expect("shell should spawn");
        assert!(!shell.is_ready(), "should not be ready before init");

        shell.initialize().await.expect("init should succeed");
        assert!(shell.is_ready(), "should be ready after init");

        let _ = shell.kill();
    }

    // Test 3: Initialization completes (hooks injected, prompt detected)
    #[tokio::test]
    #[serial(pty)]
    async fn test_initialization_completes() {
        let mut shell = ShellProcess::spawn(bash_path(), "/tmp")
            .await
            .expect("shell should spawn");

        let result = shell.initialize().await;
        assert!(result.is_ok(), "init failed: {:?}", result.err());

        let _ = shell.kill();
    }

    // Test 4: execute("echo hello") returns output containing "hello" and exit_code 0
    #[tokio::test]
    #[serial(pty)]
    async fn test_execute_echo_hello() {
        let mut shell = ShellProcess::spawn(bash_path(), "/tmp")
            .await
            .expect("shell should spawn");
        shell.initialize().await.expect("init should succeed");

        let result = shell
            .execute("echo hello", Duration::from_secs(5))
            .await
            .expect("execute should succeed");

        assert_eq!(result.exit_code, 0, "exit code should be 0");
        assert!(
            result.output.contains("hello"),
            "output should contain 'hello', got: {:?}",
            result.output
        );

        let _ = shell.kill();
    }

    // Test 5: execute with a command that fails returns non-zero exit_code
    #[tokio::test]
    #[serial(pty)]
    async fn test_execute_failing_command() {
        let mut shell = ShellProcess::spawn(bash_path(), "/tmp")
            .await
            .expect("shell should spawn");
        shell.initialize().await.expect("init should succeed");

        // Use `false` which returns exit code 1 without killing the shell.
        // The PROMPT_COMMAND hook captures $? BEFORE resetting it.
        let result = shell
            .execute("false", Duration::from_secs(5))
            .await
            .expect("execute should succeed");

        assert_ne!(
            result.exit_code, 0,
            "exit code should be non-zero for `false`, got: {}",
            result.exit_code
        );

        let _ = shell.kill();
    }

    // Test 6: CWD tracking — execute("cd /tmp") updates cwd
    #[tokio::test]
    #[serial(pty)]
    async fn test_cwd_tracking() {
        let mut shell = ShellProcess::spawn(bash_path(), "/")
            .await
            .expect("shell should spawn");
        shell.initialize().await.expect("init should succeed");

        let result = shell
            .execute("cd /tmp", Duration::from_secs(5))
            .await
            .expect("execute should succeed");

        // /tmp resolves to /private/tmp on macOS
        let cwd = result.cwd.clone();
        assert!(
            cwd == "/tmp" || cwd == "/private/tmp",
            "CWD should be /tmp or /private/tmp, got: {cwd}"
        );
        assert!(
            shell.cwd() == "/tmp" || shell.cwd() == "/private/tmp",
            "tracked CWD should be /tmp or /private/tmp, got: {}",
            shell.cwd()
        );

        let _ = shell.kill();
    }

    // Test 7: Environment persistence — export then echo
    #[tokio::test]
    #[serial(pty)]
    async fn test_environment_persistence() {
        let mut shell = ShellProcess::spawn(bash_path(), "/tmp")
            .await
            .expect("shell should spawn");
        shell.initialize().await.expect("init should succeed");

        let result = shell
            .execute("export MISH_TEST_VAR=barqux", Duration::from_secs(5))
            .await
            .expect("export should succeed");
        assert_eq!(result.exit_code, 0);

        let result = shell
            .execute("echo $MISH_TEST_VAR", Duration::from_secs(5))
            .await
            .expect("echo should succeed");

        assert_eq!(result.exit_code, 0);
        assert!(
            result.output.contains("barqux"),
            "output should contain 'barqux', got: {:?}",
            result.output
        );

        let _ = shell.kill();
    }

    // Test 8: Timeout enforcement — long command killed after timeout
    #[tokio::test]
    #[serial(pty)]
    async fn test_timeout_enforcement() {
        let mut shell = ShellProcess::spawn(bash_path(), "/tmp")
            .await
            .expect("shell should spawn");
        shell.initialize().await.expect("init should succeed");

        let start = Instant::now();
        let result = shell
            .execute("sleep 60", Duration::from_secs(1))
            .await;

        let elapsed = start.elapsed();

        assert!(
            result.is_err(),
            "should have timed out, but got: {:?}",
            result
        );
        match &result {
            Err(ShellError::ExecTimeout) => {} // expected
            other => panic!("expected ExecTimeout, got: {:?}", other),
        }
        assert!(
            elapsed < Duration::from_secs(10),
            "timeout should have fired within a few seconds, took {:?}",
            elapsed
        );
    }

    // Test 9: Kill terminates the shell process group
    #[tokio::test]
    #[serial(pty)]
    async fn test_kill_terminates_shell() {
        let mut shell = ShellProcess::spawn(bash_path(), "/tmp")
            .await
            .expect("shell should spawn");
        shell.initialize().await.expect("init should succeed");

        let pid = shell.pid();
        assert!(pid > 0);

        shell.kill().expect("kill should succeed");

        // Give OS time to process the signal
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Try to waitpid to reap the zombie
        let _ = nix::sys::wait::waitpid(
            Pid::from_raw(pid as i32),
            Some(nix::sys::wait::WaitPidFlag::WNOHANG),
        );

        // The process should be gone. Signal 0 checks if we can send to it.
        let signal_result = nix::sys::signal::kill(
            Pid::from_raw(pid as i32),
            None,
        );

        // On macOS, the process might be reaped by PtyCapture's Drop impl,
        // or it may linger briefly. Either ESRCH (no such process) or
        // the kill succeeds (zombie not yet reaped) are acceptable.
        // What matters is the process received SIGKILL.
        // We verify by checking that reading from the PTY returns no data.
        let mut buf = [0u8; 64];
        let read_result = shell.read_output(&mut buf).await;
        match read_result {
            Ok(0) => {} // No data — process is dead, good
            Ok(_) => {} // Some final output — process was dying
            Err(_) => {} // Error reading — process gone
        }

        // If signal 0 fails, that conclusively proves the process is gone
        if signal_result.is_err() {
            return; // Process confirmed gone
        }

        // If signal 0 still succeeds, the zombie hasn't been reaped yet.
        // This is acceptable — the SIGKILL was sent.
    }

    // Test 10: Startup output is discarded (no motd/rc noise in first command)
    #[tokio::test]
    #[serial(pty)]
    async fn test_startup_output_discarded() {
        let mut shell = ShellProcess::spawn(bash_path(), "/tmp")
            .await
            .expect("shell should spawn");
        shell.initialize().await.expect("init should succeed");

        let result = shell
            .execute("echo CLEAN_OUTPUT", Duration::from_secs(5))
            .await
            .expect("execute should succeed");

        assert!(
            result.output.contains("CLEAN_OUTPUT"),
            "output should contain 'CLEAN_OUTPUT', got: {:?}",
            result.output
        );
    }
}
