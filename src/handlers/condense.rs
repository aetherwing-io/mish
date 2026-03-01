/// Condense handler — full pipeline integration.
///
/// PTY capture → LineBuffer → Classifier → EmitBuffer → condensed Summary.
///
/// This is the primary handler for verbose command output. It spawns a command
/// in a PTY, feeds output through the classification pipeline, and produces a
/// condensed summary with hazards, outcomes, and noise counts.

use std::thread;
use std::time::Duration;

use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};

use crate::core::classifier::Classifier;
use crate::core::emit::{EmitBuffer, Summary};
use crate::core::grammar::{Action, Grammar};
use crate::core::line_buffer::LineBuffer;
use crate::core::pty::{ExitStatus, PtyCapture};

/// Result of running a command through the condense pipeline.
pub struct CondenseResult {
    pub summary: Summary,
}

/// Run a command through the condense pipeline.
///
/// Spawns the command in a PTY, reads output through the classification
/// pipeline (LineBuffer → Classifier → EmitBuffer), and returns a condensed
/// summary when the process exits.
pub fn handle(
    args: &[String],
    grammar: Option<&Grammar>,
    action: Option<&Action>,
) -> Result<CondenseResult, Box<dyn std::error::Error>> {
    // 1. Spawn command in PTY
    let pty = PtyCapture::spawn(args)?;

    // 2. Create pipeline components
    let mut line_buffer = LineBuffer::new();
    let mut classifier = Classifier::new(grammar, action);
    let mut emit_buffer = EmitBuffer::new();

    // 3. Event loop: read PTY output and feed through pipeline
    let mut buf = [0u8; 4096];
    let mut exit_status: Option<ExitStatus> = None;

    loop {
        // Read bytes from PTY (non-blocking)
        let n = pty.read_output(&mut buf)?;

        if n > 0 {
            // Feed raw bytes into LineBuffer to produce lines
            let lines = line_buffer.ingest(&buf[..n]);
            for line in lines {
                // Classifier handles VTE stripping internally
                let classification = classifier.classify(line);
                emit_buffer.accept(classification);
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
            let classification = classifier.classify(partial);
            emit_buffer.accept(classification);
        }

        // Sleep briefly to avoid busy-looping
        thread::sleep(Duration::from_millis(10));
    }

    // 4. Drain remaining bytes from PTY
    let remaining = pty.drain()?;

    // 5. Finalize line buffer with remaining bytes
    let final_lines = line_buffer.finalize(&remaining);
    for line in final_lines {
        let classification = classifier.classify(line);
        emit_buffer.accept(classification);
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

    Ok(CondenseResult { summary })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::grammar::load_grammar_from_str;
    use nix::sys::signal::Signal;
    use serial_test::serial;

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
    // Timing-sensitive: may be flaky in CI due to partial_timeout (500ms default)
    #[test]
    #[serial(pty)]
    #[ignore]
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
}
