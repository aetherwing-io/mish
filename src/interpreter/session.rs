//! Interpreter session — PTY-backed REPL with sentinel-based boundary detection.
//!
//! Extracted from `cli/session.rs` so both CLI sessions and MCP interpreter
//! mode share the same core logic.

use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::core::line_buffer::LineBuffer;
use crate::core::pty::PtyCapture;
use crate::squasher::pipeline::{Pipeline, PipelineConfig};
use crate::squasher::vte_strip::VteStripper;

// ---------------------------------------------------------------------------
// Interpreter kind + sentinel
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterpreterKind {
    Python,
    Node,
    Generic,
}

impl InterpreterKind {
    pub fn detect(cmd: &str) -> Self {
        let basename = cmd.rsplit('/').next().unwrap_or(cmd);
        if basename.contains("python") {
            InterpreterKind::Python
        } else if basename.contains("node") {
            InterpreterKind::Node
        } else {
            InterpreterKind::Generic
        }
    }

    pub fn sentinel_cmd(&self, token: &str) -> String {
        match self {
            InterpreterKind::Python => format!("print(\"{token}\")"),
            InterpreterKind::Node => format!("console.log(\"{token}\")"),
            InterpreterKind::Generic => format!("echo {token}"),
        }
    }
}

/// Strip interpreter prompt prefix from a line.
/// Handles Python (`>>> `, `... `), Node (`> `), and generic shells.
pub fn strip_prompt(line: &str) -> &str {
    for prefix in &[">>> ", "... ", "> "] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return rest;
        }
    }
    line
}

pub fn make_sentinel() -> (String, String) {
    let id = Uuid::new_v4().simple().to_string();
    let token = format!("__MISH_{id}__");
    (token.clone(), token)
}

// ---------------------------------------------------------------------------
// InterpreterSession — raw PTY + sentinel
// ---------------------------------------------------------------------------

pub struct InterpreterSession {
    pty: PtyCapture,
    kind: InterpreterKind,
    created_at: Instant,
}

impl InterpreterSession {
    pub fn spawn(cmd: &str) -> Result<Self, String> {
        let kind = InterpreterKind::detect(cmd);

        // Build argv: for python use -i for interactive, for node use -i
        let args: Vec<String> = match kind {
            InterpreterKind::Python => vec![cmd.to_string(), "-i".to_string()],
            InterpreterKind::Node => vec![cmd.to_string(), "-i".to_string()],
            InterpreterKind::Generic => vec![cmd.to_string()],
        };

        let pty = PtyCapture::spawn(&args).map_err(|e| format!("spawn failed: {e}"))?;

        // Wait briefly for initial prompt
        std::thread::sleep(Duration::from_millis(500));

        // Drain any initial banner/prompt output
        let mut buf = [0u8; 8192];
        loop {
            match pty.read_output(&mut buf) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(_) => break,
            }
        }

        Ok(InterpreterSession {
            pty,
            kind,
            created_at: Instant::now(),
        })
    }

    pub fn execute(&self, input: &str, timeout: Duration) -> Result<ExecuteResult, String> {
        let start = Instant::now();
        let (sentinel_token, _) = make_sentinel();
        let sentinel_cmd = self.kind.sentinel_cmd(&sentinel_token);

        // Write user input + sentinel.
        // Extra blank line before sentinel closes any open blocks in Python
        // (e.g., `for`, `if`, `def` blocks need a blank line to terminate).
        let separator = match self.kind {
            InterpreterKind::Python => "\n\n",
            _ => "\n",
        };
        let payload = format!("{input}{separator}{sentinel_cmd}\n");
        self.pty
            .write_stdin(payload.as_bytes())
            .map_err(|e| format!("write failed: {e}"))?;

        // Read until sentinel appears on its own line (not inside the echoed command).
        // The PTY echoes the sentinel_cmd, which contains the token. We need to see
        // the token on a line that doesn't also contain the sentinel command text.
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        let deadline = Instant::now() + timeout;

        loop {
            if Instant::now() > deadline {
                return Err("timeout waiting for output".into());
            }

            match self.pty.read_output(&mut buf) {
                Ok(0) => {
                    // Check if interpreter died
                    if let Ok(Some(_status)) = self.pty.try_wait() {
                        return Err("interpreter exited unexpectedly".into());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(n) => {
                    raw.extend_from_slice(&buf[..n]);
                    // Check if sentinel appeared on its own output line.
                    // The echoed command line contains both sentinel_cmd text AND
                    // sentinel_token, so we must distinguish echo from real output.
                    let text = String::from_utf8_lossy(&raw);
                    let found_standalone = text.lines().any(|line| {
                        let clean = line.trim().trim_end_matches('\r');
                        clean.contains(&sentinel_token)
                            && !clean.contains(&sentinel_cmd)
                    });
                    if found_standalone {
                        break;
                    }
                }
                Err(e) => return Err(format!("read error: {e}")),
            }
        }

        // Drain any remaining PTY data (next prompt, etc.) so it doesn't
        // leak into the next command's output.
        std::thread::sleep(Duration::from_millis(50));
        let mut drain_buf = [0u8; 4096];
        loop {
            match self.pty.read_output(&mut drain_buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => continue,
            }
        }

        let elapsed_ms = start.elapsed().as_millis() as u64;

        // Process: VTE strip the raw output, then extract between echo and sentinel
        let raw_text = String::from_utf8_lossy(&raw).to_string();
        let clean = self.strip_echo_and_sentinel(&raw_text, input, &sentinel_token, &sentinel_cmd);

        // Run through squasher pipeline
        let squashed = squash_session_output(&clean);

        Ok(ExecuteResult {
            output: squashed,
            exit_code: 0,
            elapsed_ms,
        })
    }

    /// Strip echoed input, sentinel command, and sentinel output from raw PTY output.
    ///
    /// Uses content-based filtering rather than positional skipping because PTY
    /// echo and interpreter output are interleaved (Python echoes input, then
    /// produces output, then prints a new prompt before echoing the next line).
    pub fn strip_echo_and_sentinel(
        &self,
        raw: &str,
        input: &str,
        sentinel_token: &str,
        sentinel_cmd: &str,
    ) -> String {
        // VTE strip line-by-line (preserving newline structure).
        // Also strip \r from PTY output.
        let lines: Vec<String> = raw
            .lines()
            .map(|l| {
                let stripped = VteStripper::strip(l.as_bytes()).clean_text;
                stripped.trim_end_matches('\r').to_string()
            })
            .collect();

        let input_lines: Vec<&str> = input.lines().collect();

        let mut result: Vec<&str> = Vec::new();
        for line in &lines {
            let trimmed = line.trim();

            // Skip empty lines and prompt-only lines
            if trimmed.is_empty()
                || trimmed == ">>>"
                || trimmed == ">"
                || trimmed == "..."
            {
                continue;
            }

            // Skip lines containing the sentinel command (echoed sentinel)
            if line.contains(sentinel_cmd) {
                continue;
            }

            // Skip standalone sentinel token output
            let without_prompt = strip_prompt(trimmed);
            if without_prompt == sentinel_token {
                continue;
            }

            // Skip echoed input lines: strip prompt prefix, then exact-match
            // against each input line. This avoids false positives where
            // output text happens to contain the input as a substring.
            let is_echo = input_lines.iter().any(|il| {
                let il = il.trim();
                !il.is_empty() && without_prompt == il
            });
            if is_echo {
                continue;
            }

            result.push(line.as_str());
        }

        result.join("\n")
    }

    /// Write raw bytes to the PTY stdin without sentinel wrapping.
    /// Returns the number of bytes written.
    pub fn write_raw(&self, input: &str) -> Result<usize, String> {
        self.pty
            .write_stdin(input.as_bytes())
            .map_err(|e| format!("write_raw failed: {e}"))
    }

    /// Non-blocking read from PTY. Returns 0 if no data available.
    pub fn read_pty_nonblocking(&self, buf: &mut [u8]) -> Result<usize, String> {
        match self.pty.read_output(buf) {
            Ok(n) => Ok(n),
            Err(e) => Err(format!("read error: {e}")),
        }
    }

    /// Drain background output and establish a clean fence for the next fg command.
    ///
    /// Sends a fence sentinel to the interpreter. Since interpreters are single-threaded,
    /// the fence cannot execute until all prior statements complete. Everything before
    /// the fence output is background output; after the fence the PTY is clean.
    pub fn drain_and_fence(&self, timeout: Duration) -> Result<String, String> {
        let (fence_token, _) = make_sentinel();
        let fence_cmd = self.kind.sentinel_cmd(&fence_token);

        // Extra newlines to close any open blocks (Python for/if/def)
        let payload = format!("\n\n{fence_cmd}\n");
        self.pty
            .write_stdin(payload.as_bytes())
            .map_err(|e| format!("fence write failed: {e}"))?;

        // Read until fence token appears (same pattern as execute())
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        let deadline = Instant::now() + timeout;

        loop {
            if Instant::now() > deadline {
                return Err("timeout waiting for fence".into());
            }

            match self.pty.read_output(&mut buf) {
                Ok(0) => {
                    if let Ok(Some(_)) = self.pty.try_wait() {
                        return Err("interpreter exited during fence".into());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(n) => {
                    raw.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&raw);
                    let found = text.lines().any(|line| {
                        let clean = line.trim().trim_end_matches('\r');
                        clean.contains(&fence_token) && !clean.contains(&fence_cmd)
                    });
                    if found {
                        break;
                    }
                }
                Err(e) => return Err(format!("fence read error: {e}")),
            }
        }

        // Drain remaining PTY data after fence
        std::thread::sleep(Duration::from_millis(50));
        let mut drain_buf = [0u8; 4096];
        loop {
            match self.pty.read_output(&mut drain_buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => continue,
            }
        }

        // Extract pre-fence output: VTE strip, skip empty/prompt/fence lines
        let raw_text = String::from_utf8_lossy(&raw).to_string();
        let lines: Vec<String> = raw_text
            .lines()
            .map(|l| {
                let stripped = VteStripper::strip(l.as_bytes()).clean_text;
                stripped.trim_end_matches('\r').to_string()
            })
            .collect();

        let mut result: Vec<&str> = Vec::new();
        for line in &lines {
            let trimmed = line.trim();

            // Skip empty/prompt-only lines
            if trimmed.is_empty()
                || trimmed == ">>>"
                || trimmed == ">"
                || trimmed == "..."
            {
                continue;
            }

            // Skip fence command echo and fence token output
            if line.contains(&fence_cmd) {
                continue;
            }
            let without_prompt = strip_prompt(trimmed);
            if without_prompt == fence_token {
                break; // Everything after fence token is discarded
            }

            result.push(line.as_str());
        }

        let pre_fence = result.join("\n");
        Ok(squash_session_output(&pre_fence))
    }

    pub fn is_alive(&self) -> bool {
        matches!(self.pty.try_wait(), Ok(None))
    }

    pub fn uptime_secs(&self) -> u64 {
        self.created_at.elapsed().as_secs()
    }

    pub fn pid(&self) -> u32 {
        self.pty.pid().as_raw() as u32
    }

    pub fn kill(&self) {
        let _ = self.pty.signal(nix::sys::signal::Signal::SIGKILL);
    }
}

pub struct ExecuteResult {
    pub output: String,
    pub exit_code: i32,
    pub elapsed_ms: u64,
}

/// Run output through the squasher pipeline (VTE strip + dedup + truncation).
pub fn squash_session_output(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    let mut lb = LineBuffer::new();
    let lines = lb.finalize(text.as_bytes());

    let mut pipeline = Pipeline::new(PipelineConfig::default());
    for line in lines {
        pipeline.feed(line);
    }
    let result = pipeline.finalize();
    result.join("\n")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn test_interpreter_kind_detection() {
        assert_eq!(InterpreterKind::detect("python3"), InterpreterKind::Python);
        assert_eq!(
            InterpreterKind::detect("/usr/bin/python3"),
            InterpreterKind::Python
        );
        assert_eq!(InterpreterKind::detect("python"), InterpreterKind::Python);
        assert_eq!(InterpreterKind::detect("node"), InterpreterKind::Node);
        assert_eq!(
            InterpreterKind::detect("/usr/local/bin/node"),
            InterpreterKind::Node
        );
        assert_eq!(InterpreterKind::detect("bash"), InterpreterKind::Generic);
        assert_eq!(InterpreterKind::detect("zsh"), InterpreterKind::Generic);
        assert_eq!(InterpreterKind::detect("ruby"), InterpreterKind::Generic);
    }

    #[test]
    fn test_sentinel_cmd_python() {
        let cmd = InterpreterKind::Python.sentinel_cmd("__MISH_abc123__");
        assert_eq!(cmd, "print(\"__MISH_abc123__\")");
    }

    #[test]
    fn test_sentinel_cmd_node() {
        let cmd = InterpreterKind::Node.sentinel_cmd("__MISH_abc123__");
        assert_eq!(cmd, "console.log(\"__MISH_abc123__\")");
    }

    #[test]
    fn test_sentinel_cmd_generic() {
        let cmd = InterpreterKind::Generic.sentinel_cmd("__MISH_abc123__");
        assert_eq!(cmd, "echo __MISH_abc123__");
    }

    #[test]
    fn test_make_sentinel_format() {
        let (token, _) = make_sentinel();
        assert!(token.starts_with("__MISH_"));
        assert!(token.ends_with("__"));
        assert!(token.len() > 10);
    }

    #[test]
    fn test_make_sentinel_unique() {
        let (t1, _) = make_sentinel();
        let (t2, _) = make_sentinel();
        assert_ne!(t1, t2);
    }

    #[test]
    fn test_squash_session_output_empty() {
        assert_eq!(squash_session_output(""), "");
    }

    #[test]
    fn test_squash_session_output_simple() {
        let output = squash_session_output("hello world");
        assert!(output.contains("hello world"));
    }

    #[test]
    fn test_strip_prompt_helper() {
        assert_eq!(strip_prompt(">>> print(x)"), "print(x)");
        assert_eq!(strip_prompt("... print(x)"), "print(x)");
        assert_eq!(strip_prompt("> console.log(x)"), "console.log(x)");
        assert_eq!(strip_prompt("hello"), "hello");
        assert_eq!(strip_prompt(">>>"), ">>>");
    }

    fn make_test_session() -> InterpreterSession {
        InterpreterSession {
            pty: PtyCapture::spawn(&["/bin/echo".into()]).unwrap(),
            kind: InterpreterKind::Python,
            created_at: Instant::now(),
        }
    }

    #[test]
    #[serial(pty)]
    fn test_strip_consecutive_echo_then_output() {
        let input = "print(\"hello\")";
        let sentinel_token = "__MISH_test123__";
        let sentinel_cmd = "print(\"__MISH_test123__\")";
        let raw = format!(
            ">>> {input}\n>>> {sentinel_cmd}\nhello\n{sentinel_token}\n>>> "
        );

        let session = make_test_session();
        let result = session.strip_echo_and_sentinel(&raw, input, sentinel_token, sentinel_cmd);
        assert_eq!(result, "hello");
    }

    #[test]
    #[serial(pty)]
    fn test_strip_interleaved_echo_and_output() {
        let input = "print(x)";
        let sentinel_token = "__MISH_abc123__";
        let sentinel_cmd = "print(\"__MISH_abc123__\")";
        let raw = format!(
            "{input}\r\n42\r\n>>> {sentinel_cmd}\r\n{sentinel_token}\r\n"
        );

        let session = make_test_session();
        let result = session.strip_echo_and_sentinel(&raw, input, sentinel_token, sentinel_cmd);
        assert_eq!(result, "42");
    }

    #[test]
    #[serial(pty)]
    fn test_strip_no_output_command() {
        let input = "x = 42";
        let sentinel_token = "__MISH_def456__";
        let sentinel_cmd = "print(\"__MISH_def456__\")";
        let raw = format!(
            ">>> {input}\r\n>>> {sentinel_cmd}\r\n{sentinel_token}\r\n"
        );

        let session = make_test_session();
        let result = session.strip_echo_and_sentinel(&raw, input, sentinel_token, sentinel_cmd);
        assert_eq!(result, "");
    }

    #[test]
    #[serial(pty)]
    fn test_strip_multiline_output() {
        let input = "for i in range(3): print(i)";
        let sentinel_token = "__MISH_ghi789__";
        let sentinel_cmd = "print(\"__MISH_ghi789__\")";
        let raw = format!(
            ">>> {input}\r\n0\r\n1\r\n2\r\n>>> {sentinel_cmd}\r\n{sentinel_token}\r\n>>> "
        );

        let session = make_test_session();
        let result = session.strip_echo_and_sentinel(&raw, input, sentinel_token, sentinel_cmd);
        assert_eq!(result, "0\n1\n2");
    }
}
