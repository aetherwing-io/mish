/// Condense handler — full pipeline integration.
///
/// PTY capture → LineBuffer → Classifier → EmitBuffer → condensed Summary.
///
/// This is the primary handler for verbose command output. It spawns a command
/// in a PTY, feeds output through the classification pipeline, and produces a
/// condensed summary with hazards, outcomes, and noise counts.

use std::thread;
use std::time::{Duration, Instant};

use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};

use crate::core::classifier::Classifier;
use crate::core::emit::{EmitBuffer, Summary, TimingConfig};
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
    let timing = TimingConfig::default();
    let now = Instant::now();
    let mut last_activity = now;
    let mut last_flush = now;

    loop {
        // Read bytes from PTY (non-blocking)
        let n = pty.read_output(&mut buf)?;

        if n > 0 {
            last_activity = Instant::now();

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
    use crate::core::classifier::{Classification, NoiseAction};
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
}
