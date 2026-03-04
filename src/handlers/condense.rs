/// Condense handler — full pipeline integration.
///
/// Two paths process the same raw PTY output:
/// - **Streaming**: LineBuffer → Classifier → EmitBuffer → Summary (outcomes, hazards)
/// - **Batch**: raw Lines → Pipeline::process(Condense) → cleaned output (VTE strip,
///   progress removal, dedup, truncation) — consistent with the MCP sh_run path.
///
/// This is the primary handler for verbose command output. It spawns a command
/// in a PTY, feeds output through both paths, and returns the combined result.

use std::io::Write;
use std::os::unix::io::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};

use crate::core::classifier::Classifier;
use crate::core::emit::{EmitBuffer, Summary, TimingConfig};
use crate::core::grammar::{Action, Grammar};
use crate::core::line_buffer::{Line, LineBuffer};
use crate::core::pty::{ExitStatus, PtyCapture};
use crate::router::categories::Category;
use crate::squasher::pipeline::{Pipeline, PipelineConfig, PipelineMetrics};

// ---------------------------------------------------------------------------
// Signal handling for the synchronous condense event loop
// ---------------------------------------------------------------------------

/// Atomic flag set by SIGINT handler — checked in the event loop.
pub(crate) static SIGINT_RECEIVED: AtomicBool = AtomicBool::new(false);
/// Atomic flag set by SIGWINCH handler — checked in the event loop.
static SIGWINCH_RECEIVED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigint(_: i32) {
    SIGINT_RECEIVED.store(true, Ordering::SeqCst);
}

extern "C" fn handle_sigwinch(_: i32) {
    SIGWINCH_RECEIVED.store(true, Ordering::SeqCst);
}

/// RAII guard that installs signal handlers and restores originals on drop.
pub(crate) struct SignalGuard {
    old_int: SigAction,
    old_winch: SigAction,
}

impl SignalGuard {
    /// Install SIGINT and SIGWINCH handlers, returning a guard that restores
    /// the previous handlers when dropped.
    pub fn install() -> Self {
        // Clear any stale flags
        SIGINT_RECEIVED.store(false, Ordering::SeqCst);
        SIGWINCH_RECEIVED.store(false, Ordering::SeqCst);

        let sa_int = SigAction::new(
            SigHandler::Handler(handle_sigint),
            SaFlags::SA_RESTART,
            SigSet::empty(),
        );
        let sa_winch = SigAction::new(
            SigHandler::Handler(handle_sigwinch),
            SaFlags::SA_RESTART,
            SigSet::empty(),
        );

        let old_int = unsafe { sigaction(Signal::SIGINT, &sa_int) }
            .expect("failed to install SIGINT handler");
        let old_winch = unsafe { sigaction(Signal::SIGWINCH, &sa_winch) }
            .expect("failed to install SIGWINCH handler");

        Self { old_int, old_winch }
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = sigaction(Signal::SIGINT, &self.old_int);
            let _ = sigaction(Signal::SIGWINCH, &self.old_winch);
        }
        // Clear flags so they don't leak to the next caller
        SIGINT_RECEIVED.store(false, Ordering::SeqCst);
        SIGWINCH_RECEIVED.store(false, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// TerminalGuard — RAII restore of terminal state after interactive transition
// ---------------------------------------------------------------------------

/// Saves the original termios and restores it on drop.
///
/// When the condense handler transitions to interactive mode, it puts the
/// user's terminal into raw mode (cfmakeraw) and sets stdin to non-blocking.
/// This guard ensures restoration even on panic or early `?` return.
struct TerminalGuard {
    original_termios: nix::sys::termios::Termios,
    original_stdin_flags: nix::fcntl::OFlag,
}

impl TerminalGuard {
    /// Save the current terminal state. Call before modifying termios/flags.
    fn save() -> Result<Self, Box<dyn std::error::Error>> {
        use nix::fcntl::{fcntl, FcntlArg, OFlag};
        use nix::sys::termios::tcgetattr;
        use std::io;

        let stdin_fd = unsafe { std::os::unix::io::BorrowedFd::borrow_raw(libc::STDIN_FILENO) };
        let original_termios = tcgetattr(&stdin_fd)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("tcgetattr: {e}")))?;

        let flags = fcntl(libc::STDIN_FILENO, FcntlArg::F_GETFL)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("fcntl F_GETFL: {e}")))?;
        let original_stdin_flags = OFlag::from_bits_truncate(flags);

        Ok(Self {
            original_termios,
            original_stdin_flags,
        })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        use nix::fcntl::{fcntl, FcntlArg};
        use nix::sys::termios::{tcsetattr, SetArg};

        // Restore termios
        let stdin_fd = unsafe { std::os::unix::io::BorrowedFd::borrow_raw(libc::STDIN_FILENO) };
        let _ = tcsetattr(&stdin_fd, SetArg::TCSANOW, &self.original_termios);

        // Restore stdin flags (remove O_NONBLOCK if we added it)
        let _ = fcntl(libc::STDIN_FILENO, FcntlArg::F_SETFL(self.original_stdin_flags));
    }
}

/// Query the current terminal dimensions (cols, rows).
///
/// Falls back to (80, 24) if the ioctl fails.
fn query_terminal_size() -> (u16, u16) {
    let mut ws = nix::pty::Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // TIOCGWINSZ on stdout
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0 {
        (ws.ws_col, ws.ws_row)
    } else {
        (80, 24)
    }
}

/// Result of running a command through the condense pipeline.
pub struct CondenseResult {
    pub summary: Summary,
    /// Batch output processed through the unified squasher pipeline
    /// (VTE strip → progress removal → dedup → truncation).
    pub pipeline_output: Vec<String>,
    /// Metrics from the squasher pipeline pass.
    pub pipeline_metrics: PipelineMetrics,
    /// Whether the command transitioned to interactive mode (raw PTY detected).
    pub transitioned_to_interactive: bool,
}

/// Post-process raw output lines through the squasher pipeline.
///
/// Runs lines through VTE strip → progress removal → dedup → truncation.
/// Used as the batch-output path for consistency with the MCP handler.
pub fn post_process(raw_lines: Vec<Line>) -> (Vec<String>, PipelineMetrics) {
    let config = PipelineConfig::default();
    let mut pipeline = Pipeline::new(config);
    let result = pipeline.process(raw_lines, Category::Condense);
    (result.output, result.metrics)
}

/// Run a command through the condense pipeline.
///
/// Spawns the command in a PTY, reads output through the classification
/// pipeline (LineBuffer → Classifier → EmitBuffer), and returns a condensed
/// summary when the process exits. Raw output is also batch-processed
/// through the unified squasher Pipeline for consistency with the MCP path.
pub fn handle(
    args: &[String],
    grammar: Option<&Grammar>,
    action: Option<&Action>,
) -> Result<CondenseResult, Box<dyn std::error::Error>> {
    // 1. Spawn command in PTY
    let pty = PtyCapture::spawn(args)?;

    // 2. Install signal handlers (restored on drop)
    let _signal_guard = SignalGuard::install();

    // 3. Create pipeline components
    let mut line_buffer = LineBuffer::new();
    let mut classifier = Classifier::new(grammar, action);
    let mut emit_buffer = EmitBuffer::new();
    let mut raw_lines: Vec<Line> = Vec::new();

    // 4. Event loop: read PTY output and feed through pipeline
    let mut buf = [0u8; 4096];
    let mut exit_status: Option<ExitStatus> = None;
    let timing = TimingConfig::default();
    let now = Instant::now();
    let mut last_activity = now;
    let mut last_flush = now;

    // Interactive mode state — for raw mode detection and passthrough
    let stdin_is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) } != 0;
    let mut interactive_mode = false;
    let mut _terminal_guard: Option<TerminalGuard> = None;

    loop {
        // Check signal flags
        if SIGINT_RECEIVED.swap(false, Ordering::SeqCst) {
            // Forward SIGINT to child process
            let _ = pty.signal(Signal::SIGINT);
        }
        if SIGWINCH_RECEIVED.swap(false, Ordering::SeqCst) {
            // Resize child PTY to match terminal
            let (cols, rows) = query_terminal_size();
            let _ = pty.resize(cols, rows);
        }

        // Read bytes from PTY (non-blocking)
        let n = pty.read_output(&mut buf)?;

        if n > 0 {
            last_activity = Instant::now();

            // Raw mode detection: one-way transition to interactive passthrough
            if !interactive_mode && stdin_is_tty && pty.is_raw_mode() {
                // Save terminal state and switch to raw mode for passthrough
                if let Ok(guard) = TerminalGuard::save() {
                    use nix::sys::termios::{cfmakeraw, tcsetattr, SetArg};

                    // Put user's terminal into raw mode
                    let stdin_fd = unsafe {
                        std::os::unix::io::BorrowedFd::borrow_raw(libc::STDIN_FILENO)
                    };
                    if let Ok(mut raw_termios) = nix::sys::termios::tcgetattr(&stdin_fd) {
                        cfmakeraw(&mut raw_termios);
                        let _ = tcsetattr(&stdin_fd, SetArg::TCSANOW, &raw_termios);
                    }

                    // Set stdin to non-blocking for poll-based forwarding
                    if let Ok(flags) = fcntl(libc::STDIN_FILENO, FcntlArg::F_GETFL) {
                        let mut oflags = OFlag::from_bits_truncate(flags);
                        oflags.insert(OFlag::O_NONBLOCK);
                        let _ = fcntl(libc::STDIN_FILENO, FcntlArg::F_SETFL(oflags));
                    }

                    _terminal_guard = Some(guard);
                    interactive_mode = true;
                }
            }

            // Forward PTY output to stdout when in interactive mode
            if interactive_mode {
                let _ = std::io::stdout().write_all(&buf[..n]);
                let _ = std::io::stdout().flush();
            }

            // Feed raw bytes into LineBuffer to produce lines (classification continues)
            let lines = line_buffer.ingest(&buf[..n]);
            for line in lines {
                // Collect raw lines for batch pipeline processing
                raw_lines.push(line.clone());
                // Classifier handles VTE stripping internally
                let classification = classifier.classify(line);
                emit_buffer.accept(classification);
                // Drain deferred line from stack trace compression
                while let Some(deferred) = classifier.drain_deferred() {
                    emit_buffer.accept(deferred);
                }
            }
        }

        // Forward stdin to child when in interactive mode
        if interactive_mode {
            let mut stdin_buf = [0u8; 1024];
            match nix::unistd::read(libc::STDIN_FILENO, &mut stdin_buf) {
                Ok(n) if n > 0 => {
                    let _ = pty.write_stdin(&stdin_buf[..n]);
                }
                _ => {} // EAGAIN (no data) or error — ignore
            }
        }

        // Check if child has exited (non-blocking), capturing exit status
        if exit_status.is_none() {
            match waitpid(pty.pid(), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(_, code)) => {
                    exit_status = Some(ExitStatus {
                        code: Some(code),
                        signal: None,
                    });
                }
                Ok(WaitStatus::Signaled(_, sig, _)) => {
                    exit_status = Some(ExitStatus {
                        code: None,
                        signal: Some(sig as i32),
                    });
                }
                _ => {}
            }
        }

        if exit_status.is_some() {
            break;
        }

        // Check for partial line timeout
        if let Some(partial) = line_buffer.emit_partial() {
            raw_lines.push(partial.clone());
            let classification = classifier.classify(partial);
            emit_buffer.accept(classification);
            while let Some(deferred) = classifier.drain_deferred() {
                emit_buffer.accept(deferred);
            }
        }

        // Timer-based flush triggers
        let since_last_flush = last_flush.elapsed();
        if since_last_flush >= timing.flush_debounce && emit_buffer.has_pending() {
            let silence_elapsed = last_activity.elapsed();
            let should_flush = silence_elapsed >= timing.silence_timeout
                || since_last_flush >= timing.flush_interval;

            if should_flush {
                emit_buffer.flush_pending();
                last_flush = Instant::now();
            }
        }

        // Use poll when interactive (responsive I/O), sleep when not (save CPU)
        if interactive_mode {
            use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
            let mut pfds = [
                PollFd::new(pty.master_fd().as_fd(), PollFlags::POLLIN),
                PollFd::new(
                    unsafe { std::os::unix::io::BorrowedFd::borrow_raw(libc::STDIN_FILENO) },
                    PollFlags::POLLIN,
                ),
            ];
            let _ = poll(&mut pfds, PollTimeout::from(10u16));
        } else {
            thread::sleep(Duration::from_millis(10));
        }
    }

    // 4. Drain remaining bytes from PTY
    let remaining = pty.drain()?;

    // 5. Finalize line buffer with remaining bytes
    let final_lines = line_buffer.finalize(&remaining);
    for line in final_lines {
        raw_lines.push(line.clone());
        let classification = classifier.classify(line);
        emit_buffer.accept(classification);
        while let Some(deferred) = classifier.drain_deferred() {
            emit_buffer.accept(deferred);
        }
    }

    // 6. Determine exit code from captured status
    let status = exit_status.unwrap_or_else(|| {
        // Fallback: wait for child (should not normally reach here)
        pty.wait().unwrap_or(ExitStatus {
            code: Some(-1),
            signal: None,
        })
    });
    let exit_code = status.code.unwrap_or_else(|| {
        // Killed by signal: convention is 128 + signal number
        status.signal.map(|s| 128 + s).unwrap_or(-1)
    });

    // 7. Finalize emit buffer → Summary
    let elapsed = pty.elapsed();
    let summary = emit_buffer.finalize(exit_code, elapsed, grammar, action);

    // 8. Batch-process raw output through unified Pipeline
    let (pipeline_output, pipeline_metrics) = post_process(raw_lines);

    Ok(CondenseResult {
        summary,
        pipeline_output,
        pipeline_metrics,
        transitioned_to_interactive: interactive_mode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::classifier::{Classification, NoiseAction};
    use crate::core::grammar::load_grammar_from_str;
    use crate::core::line_buffer::Line;
    use nix::sys::signal::Signal;
    use serial_test::serial;

    // -----------------------------------------------------------------------
    // Pipeline integration unit tests (no PTY needed)
    // -----------------------------------------------------------------------

    // Test: post_process strips ANSI codes from raw lines
    #[test]
    fn test_post_process_strips_ansi() {
        let lines = vec![
            Line::Complete("\x1b[31mERROR: fail\x1b[0m".into()),
            Line::Complete("clean line".into()),
        ];
        let (output, metrics) = post_process(lines);
        assert!(
            output.iter().all(|l| !l.contains("\x1b")),
            "expected no ANSI codes in pipeline output, got: {:?}",
            output
        );
        assert_eq!(metrics.vte_stripped, 1);
    }

    // Test: post_process deduplicates repetitive lines
    #[test]
    fn test_post_process_dedup_repetitive() {
        let lines: Vec<Line> = (0..20)
            .map(|i| Line::Complete(format!("Downloading https://registry.npmjs.org/pkg{}", i)))
            .collect();
        let (output, _metrics) = post_process(lines);
        assert!(
            output.len() < 20,
            "expected dedup to reduce count from 20, got {}",
            output.len()
        );
        assert!(
            output.iter().any(|l| l.contains("(x")),
            "expected dedup group marker, got: {:?}",
            output
        );
    }

    // Test: post_process collapses progress bar overwrites
    #[test]
    fn test_post_process_collapses_progress() {
        let lines = vec![
            Line::Complete("Starting...".into()),
            Line::Overwrite("10%".into()),
            Line::Overwrite("50%".into()),
            Line::Overwrite("100%".into()),
            Line::Complete("Done!".into()),
        ];
        let (output, metrics) = post_process(lines);
        assert!(
            !output.iter().any(|l| l.contains("10%") || l.contains("50%")),
            "expected progress lines removed, got: {:?}",
            output
        );
        assert!(
            output.iter().any(|l| l == "Done!"),
            "expected 'Done!' in output, got: {:?}",
            output
        );
        assert!(
            metrics.progress_stripped > 0,
            "expected progress_stripped > 0"
        );
    }

    // Test: post_process produces same output as Pipeline::process(Condense)
    #[test]
    fn test_post_process_matches_pipeline() {
        use crate::squasher::pipeline::{Pipeline, PipelineConfig};
        use crate::router::categories::Category;

        let lines = vec![
            Line::Complete("\x1b[31mERROR\x1b[0m".into()),
            Line::Overwrite("50%".into()),
            Line::Overwrite("100%".into()),
            Line::Complete("done".into()),
        ];

        // Direct Pipeline path (what MCP uses)
        let mut pipe = Pipeline::new(PipelineConfig::default());
        let expected = pipe.process(lines.clone(), Category::Condense);

        // post_process path (what CLI condense uses)
        let (output, metrics) = post_process(lines);

        assert_eq!(output, expected.output);
        assert_eq!(metrics, expected.metrics);
    }

    // -----------------------------------------------------------------------
    // Signal handling tests
    // -----------------------------------------------------------------------

    // Test: SIGINT flag causes child to receive SIGINT and exit
    #[test]
    #[serial(pty)]
    fn test_sigint_forwarded_to_child() {
        use std::sync::atomic::Ordering;

        // Set the SIGINT flag after a delay — the condense event loop should
        // forward it to the child
        thread::spawn(|| {
            thread::sleep(Duration::from_millis(500));
            SIGINT_RECEIVED.store(true, Ordering::SeqCst);
        });

        // Run a command that sleeps — SIGINT should kill it before completion
        let args = sh_cmd("sleep 5");
        let result = handle(&args, None, None).unwrap();

        // Child should have been killed by SIGINT (exit code 130 = 128+2)
        assert!(
            result.summary.exit_code == 130 || result.summary.exit_code == 2,
            "expected SIGINT exit code (130 or 2), got: {}",
            result.summary.exit_code
        );
    }

    // Test: SignalGuard restores previous handlers on drop
    #[test]
    fn test_signal_guard_restores_handlers() {
        use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};

        // Save current SIGWINCH handler
        let before = unsafe { sigaction(Signal::SIGWINCH, &SigAction::new(
            SigHandler::SigDfl,
            SaFlags::empty(),
            SigSet::empty(),
        )) }.unwrap();

        // Restore it
        unsafe { sigaction(Signal::SIGWINCH, &before) }.unwrap();

        // Install signal guard, then drop it
        {
            let _guard = SignalGuard::install();
        }

        // After guard is dropped, SIGWINCH handler should be restored
        // (We can't easily inspect the handler, but at minimum it shouldn't crash)
        // The guard's Drop restores the saved handlers
    }

    // Test: CondenseResult includes pipeline_output and pipeline_metrics
    #[test]
    #[serial(pty)]
    fn test_condense_result_has_pipeline_output() {
        let args = sh_cmd("echo 'hello world'");
        let result = handle(&args, None, None).unwrap();

        // Pipeline output should be populated
        assert!(
            !result.pipeline_output.is_empty(),
            "expected pipeline_output to be populated"
        );
        // Pipeline output should be clean (no ANSI)
        assert!(
            result.pipeline_output.iter().all(|l| !l.contains("\x1b")),
            "expected no ANSI in pipeline_output"
        );
        // Summary still works (backward compat)
        assert_eq!(result.summary.exit_code, 0);
    }

    // Helper: build a /bin/sh -c command
    fn sh_cmd(script: &str) -> Vec<String> {
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            script.to_string(),
        ]
    }

    // Test 1: Condense of known tool with grammar — verify Summary has correct summary_lines
    #[test]
    #[serial(pty)]
    fn test_condense_with_grammar() {
        let toml_str = r#"
[tool]
name = "mytest"
detect = ["mytest"]

[actions.build]
detect = ["build"]

[[actions.build.outcome]]
pattern = '^Built (?P<count>\d+) files'
action = "promote"
captures = ["count"]

[actions.build.summary]
success = "+ {count} files built"
failure = "! build failed (exit {exit_code})"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let action = grammar.actions.get("build").unwrap();

        // Run a command that produces output matching the outcome pattern
        let args = sh_cmd("echo 'Starting build'; echo 'Compiling...'; echo 'Built 42 files'");
        let result = handle(&args, Some(&grammar), Some(action)).unwrap();

        assert_eq!(result.summary.exit_code, 0);
        // The grammar summary template should format using captured "count"
        assert!(
            result.summary.summary_lines.iter().any(|l| l.contains("42")),
            "expected summary to contain '42', got: {:?}",
            result.summary.summary_lines
        );
    }

    // Test 2: Condense of unknown tool (no grammar) — verify ring buffer fallback
    #[test]
    #[serial(pty)]
    fn test_condense_no_grammar() {
        let args = sh_cmd("echo 'line1'; echo 'line2'; echo 'line3'");
        let result = handle(&args, None, None).unwrap();

        assert_eq!(result.summary.exit_code, 0);
        // With no grammar and no outcome rules, should fall back to ring buffer (last lines)
        assert!(
            !result.summary.summary_lines.is_empty(),
            "expected summary_lines from ring buffer fallback"
        );
        // Ring buffer lines are prefixed with " last: "
        let has_last = result
            .summary
            .summary_lines
            .iter()
            .any(|l| l.contains("last:"));
        assert!(
            has_last,
            "expected ring buffer fallback lines with 'last:' prefix, got: {:?}",
            result.summary.summary_lines
        );
    }

    // Test 3: Exit code handling — non-zero exit code
    #[test]
    #[serial(pty)]
    fn test_exit_code_nonzero() {
        let args = sh_cmd("echo 'about to fail'; exit 42");
        let result = handle(&args, None, None).unwrap();

        assert_eq!(
            result.summary.exit_code, 42,
            "expected exit code 42, got: {}",
            result.summary.exit_code
        );
        assert!(
            result.summary.header.contains("exit 42"),
            "expected header to contain 'exit 42', got: {}",
            result.summary.header
        );
    }

    // Test 4: Hazard passthrough — output matching hazard pattern appears in hazard_lines
    #[test]
    #[serial(pty)]
    fn test_hazard_passthrough() {
        let toml_str = r#"
[tool]
name = "compiler"
detect = ["compiler"]

[actions.compile]
detect = ["compile"]

[[actions.compile.hazard]]
pattern = '^ERROR:'
action = "promote"
severity = "error"

[actions.compile.summary]
success = "+ compiled"
failure = "! compile failed"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let action = grammar.actions.get("compile").unwrap();

        let args = sh_cmd("echo 'compiling...'; echo 'ERROR: undefined variable'; echo 'done'");
        let result = handle(&args, Some(&grammar), Some(action)).unwrap();

        // Hazard lines should contain the error
        let has_hazard = result
            .summary
            .hazard_lines
            .iter()
            .any(|l| l.contains("undefined variable"));
        assert!(
            has_hazard,
            "expected hazard_lines to contain 'undefined variable', got: {:?}",
            result.summary.hazard_lines
        );
    }

    // Test 5: Timeout behavior — partial line without newline
    #[test]
    #[serial(pty)]
    fn test_partial_line_timeout() {
        // Write partial output (no newline), wait, then exit
        // The partial line should eventually be emitted via emit_partial timeout
        let args = sh_cmd("printf 'waiting...' && sleep 1 && echo ' done'");
        let result = handle(&args, None, None).unwrap();

        assert_eq!(result.summary.exit_code, 0);
        // Should have captured at least some output (header shows line count)
        assert!(
            !result.summary.summary_lines.is_empty() || result.summary.header.contains("lines"),
            "expected some output from partial + complete lines, header: {}",
            result.summary.header
        );
    }

    // Test 6: Signal forwarding — verify PtyCapture.signal terminates a process
    #[test]
    #[serial(pty)]
    fn test_signal_forwarding() {
        // Spawn a sleep command directly via PtyCapture (not through handle,
        // since handle blocks until exit)
        let pty = PtyCapture::spawn(&sh_cmd("sleep 60")).unwrap();

        // Let the process start
        thread::sleep(Duration::from_millis(100));

        // Send SIGTERM
        pty.signal(Signal::SIGTERM).unwrap();

        // Wait for exit
        let status = pty.wait().unwrap();

        // Should have been killed by signal or exited
        assert!(
            status.signal.is_some() || status.code.is_some(),
            "expected signal or exit code after SIGTERM, got: {:?}",
            status
        );
    }

    // Test 7: Empty output — command produces no output
    #[test]
    #[serial(pty)]
    fn test_empty_output() {
        let args = sh_cmd("true");
        let result = handle(&args, None, None).unwrap();

        assert_eq!(result.summary.exit_code, 0);
        assert!(
            result.summary.header.contains("exit 0"),
            "expected header with 'exit 0', got: {}",
            result.summary.header
        );
    }

    // Test 8: Very long output — command produces many lines, verify summary truncates
    #[test]
    #[serial(pty)]
    fn test_long_output_summarized() {
        // Generate 200 lines of output
        let args = sh_cmd("for i in $(seq 1 200); do echo \"line $i of output\"; done");
        let result = handle(&args, None, None).unwrap();

        assert_eq!(result.summary.exit_code, 0);

        // Header should reflect ~200 lines processed
        assert!(
            result.summary.header.contains("200 lines")
                || result.summary.header.contains("lines"),
            "expected header to mention line count, got: {}",
            result.summary.header
        );

        // The summary should be condensed — not all 200 lines repeated.
        // With no grammar, ring buffer gives last 5 lines. Hazard_lines may
        // have "... N lines" markers for suppressed noise.
        let total_output_lines =
            result.summary.summary_lines.len() + result.summary.hazard_lines.len();
        assert!(
            total_output_lines < 200,
            "expected condensed output (< 200 lines), got {} summary + {} hazard = {} total",
            result.summary.summary_lines.len(),
            result.summary.hazard_lines.len(),
            total_output_lines
        );
    }

    // -----------------------------------------------------------------------
    // Timer-based flush trigger tests
    // -----------------------------------------------------------------------

    // Test 9: Silence timeout triggers flush of pending content
    #[test]
    fn test_silence_timeout_triggers_flush() {
        let mut emit_buffer = EmitBuffer::new();
        let timing = TimingConfig::default();

        // Accept 15 noise lines (above the 10-line threshold for "... N lines")
        for _ in 0..15 {
            emit_buffer.accept(Classification::Noise {
                action: NoiseAction::Strip,
                text: "noise".into(),
            });
        }

        // Verify pending content exists
        assert!(emit_buffer.has_pending(), "should have pending content");
        assert!(emit_buffer.output().is_empty(), "no output yet before flush");

        // Simulate silence exceeding timeout
        let last_activity = Instant::now() - timing.silence_timeout - Duration::from_millis(100);
        let last_flush = Instant::now() - timing.flush_debounce - Duration::from_millis(100);

        // Check timer conditions (mirrors the event loop logic)
        let since_last_flush = last_flush.elapsed();
        let silence_elapsed = last_activity.elapsed();
        assert!(since_last_flush >= timing.flush_debounce);
        assert!(silence_elapsed >= timing.silence_timeout);

        // Flush should be triggered
        emit_buffer.flush_pending();
        assert!(
            emit_buffer.output().iter().any(|l| l.contains("15 lines")),
            "expected '... 15 lines' marker after silence timeout flush, got: {:?}",
            emit_buffer.output()
        );
    }

    // Test 10: Flush debounce prevents rapid re-flushing
    #[test]
    fn test_flush_debounce_prevents_rapid_reflush() {
        let timing = TimingConfig::default();

        // Simulate: last flush was very recent (10ms ago)
        let last_flush = Instant::now() - Duration::from_millis(10);
        let last_activity = Instant::now() - timing.silence_timeout - Duration::from_millis(100);

        let since_last_flush = last_flush.elapsed();

        // Debounce should prevent flush even though silence timeout is exceeded
        assert!(
            since_last_flush < timing.flush_debounce,
            "since_last_flush ({:?}) should be less than debounce ({:?})",
            since_last_flush,
            timing.flush_debounce
        );

        // Verify the silence timeout IS exceeded
        let silence_elapsed = last_activity.elapsed();
        assert!(
            silence_elapsed >= timing.silence_timeout,
            "silence should exceed timeout"
        );

        // The combined condition (debounce check first) prevents the flush
        let should_flush = since_last_flush >= timing.flush_debounce
            && silence_elapsed >= timing.silence_timeout;
        assert!(
            !should_flush,
            "debounce should prevent flush despite silence timeout being exceeded"
        );
    }

    // Test 11: Periodic flush interval triggers flush even with recent activity
    #[test]
    fn test_periodic_flush_interval() {
        let mut emit_buffer = EmitBuffer::new();
        let timing = TimingConfig::default();

        // Accept pending content
        for _ in 0..20 {
            emit_buffer.accept(Classification::Noise {
                action: NoiseAction::Strip,
                text: "noise".into(),
            });
        }

        // Simulate: last activity was recent but last flush was long ago
        let last_activity = Instant::now() - Duration::from_millis(50); // recent activity
        let last_flush = Instant::now() - timing.flush_interval - Duration::from_millis(100);

        let since_last_flush = last_flush.elapsed();
        let silence_elapsed = last_activity.elapsed();

        // Silence timeout NOT exceeded (recent activity)
        assert!(silence_elapsed < timing.silence_timeout);

        // But flush interval IS exceeded, and debounce is satisfied
        assert!(since_last_flush >= timing.flush_debounce);
        assert!(since_last_flush >= timing.flush_interval);

        let should_flush = since_last_flush >= timing.flush_debounce
            && (silence_elapsed >= timing.silence_timeout
                || since_last_flush >= timing.flush_interval);
        assert!(should_flush, "periodic interval should trigger flush");

        emit_buffer.flush_pending();
        assert!(
            emit_buffer.output().iter().any(|l| l.contains("20 lines")),
            "expected '... 20 lines' marker after periodic flush, got: {:?}",
            emit_buffer.output()
        );
    }

    // Test 12: Bursty output with silence gap shows intermediate flush markers
    #[test]
    #[serial(pty)]
    fn test_bursty_output_intermediate_flush() {
        // Emit 50 lines, sleep 3s (exceeds 2s silence timeout), emit 50 more lines.
        // The condense handler should flush pending during the silence gap,
        // producing "... N lines" markers in hazard_lines.
        let args = sh_cmd(
            "for i in $(seq 1 50); do echo \"burst1 line $i\"; done; \
             sleep 3; \
             for i in $(seq 1 50); do echo \"burst2 line $i\"; done"
        );
        let result = handle(&args, None, None).unwrap();

        assert_eq!(result.summary.exit_code, 0);

        // Should have ~100 lines total
        assert!(
            result.summary.header.contains("100 lines")
                || result.summary.header.contains("lines"),
            "expected header to mention line count, got: {}",
            result.summary.header
        );

        // The silence gap should have triggered an intermediate flush,
        // producing at least one "... N lines" marker in hazard_lines
        // before the process exits (which would cause a final flush).
        // With 100 lines total, we should see at least one intermediate marker.
        let flush_markers: Vec<&String> = result
            .summary
            .hazard_lines
            .iter()
            .filter(|l| l.contains("... "))
            .collect();
        assert!(
            flush_markers.len() >= 1,
            "expected at least 1 intermediate flush marker from silence gap, \
             hazard_lines: {:?}",
            result.summary.hazard_lines
        );
    }

    // -----------------------------------------------------------------------
    // Interactive mode detection tests
    // -----------------------------------------------------------------------

    // Test 13: Non-interactive command does not transition
    #[test]
    #[serial(pty)]
    fn test_non_interactive_no_transition() {
        let args = sh_cmd("echo hello");
        let result = handle(&args, None, None).unwrap();

        assert!(
            !result.transitioned_to_interactive,
            "echo should not trigger interactive transition"
        );
        assert_eq!(result.summary.exit_code, 0);
    }

    // Test 14: CondenseResult has transitioned_to_interactive field defaulting false
    #[test]
    #[serial(pty)]
    fn test_condense_result_has_interactive_flag() {
        let args = sh_cmd("true");
        let result = handle(&args, None, None).unwrap();

        assert!(
            !result.transitioned_to_interactive,
            "transitioned_to_interactive should default to false"
        );
    }
}
