/// Emit buffer — accumulates classified lines and flushes on triggers.
///
/// Flush triggers: process exit, hazard detected, prompt detected, silence, periodic timer.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::core::classifier::{Classification, NoiseAction};
use crate::core::grammar::{self, Action, Grammar, Severity};
use crate::squasher::dedup::DedupEngine;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Timing configuration for the emit buffer.
#[derive(Debug, Clone)]
pub struct TimingConfig {
    /// How long to wait before declaring a partial line a prompt (default: 500ms)
    pub partial_line_timeout: Duration,
    /// How long silence must last before flushing pending output (default: 2s)
    pub silence_timeout: Duration,
    /// Periodic flush interval for long-running processes (default: 5s)
    pub flush_interval: Duration,
    /// Minimum gap between consecutive flushes — debounce (default: 200ms)
    pub flush_debounce: Duration,
}

impl Default for TimingConfig {
    fn default() -> Self {
        Self {
            partial_line_timeout: Duration::from_millis(500),
            silence_timeout: Duration::from_secs(2),
            flush_interval: Duration::from_secs(5),
            flush_debounce: Duration::from_millis(200),
        }
    }
}

/// A captured outcome from output classification.
#[derive(Debug, Clone)]
pub struct CapturedOutcome {
    pub text: String,
    pub captures: HashMap<String, String>,
}

/// A hazard that was emitted during processing.
#[derive(Debug, Clone)]
pub struct EmittedHazard {
    pub severity: Severity,
    pub text: String,
    pub attached_lines: Vec<String>,
}

/// Final summary produced when the process exits.
#[derive(Debug, Clone)]
pub struct Summary {
    pub header: String,
    pub summary_lines: Vec<String>,
    pub hazard_lines: Vec<String>,
    pub exit_code: i32,
}

// ---------------------------------------------------------------------------
// Ring buffer (last N lines)
// ---------------------------------------------------------------------------

struct RingBuffer {
    buf: VecDeque<String>,
    capacity: usize,
}

impl RingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, item: String) {
        if self.buf.len() == self.capacity {
            self.buf.pop_front();
        }
        self.buf.push_back(item);
    }

    fn iter(&self) -> impl Iterator<Item = &String> {
        self.buf.iter()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buf.len()
    }
}

// ---------------------------------------------------------------------------
// EmitBuffer
// ---------------------------------------------------------------------------

pub struct EmitBuffer {
    pending_count: u64,
    outcomes: Vec<CapturedOutcome>,
    hazards: Vec<EmittedHazard>,
    ring: RingBuffer,
    line_count: u64,
    dedup: DedupEngine,
    #[allow(dead_code)]
    start_time: Instant,
    last_emit_time: Instant,
    output: Vec<String>,
}

impl EmitBuffer {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            pending_count: 0,
            outcomes: Vec::new(),
            hazards: Vec::new(),
            ring: RingBuffer::new(5),
            line_count: 0,
            dedup: DedupEngine::new(),
            start_time: now,
            last_emit_time: now,
            output: Vec::new(),
        }
    }

    /// Accept a classified line into the buffer.
    pub fn accept(&mut self, classified: Classification) {
        self.line_count += 1;

        // Always update ring buffer with the text
        if let Some(text) = classified.text() {
            if !text.is_empty() {
                self.ring.push(text.to_string());
            }
        }

        match classified {
            Classification::Hazard {
                severity,
                text,
                captures: _,
            } => {
                // Flush pending noise count first (preserve ordering)
                self.flush_pending_count();
                self.dedup.flush_into(&mut self.output);

                // Emit immediately
                let prefix = match severity {
                    Severity::Error => "!",
                    Severity::Warning => "~",
                };
                self.output.push(format!(" {} {}", prefix, text));
                self.hazards.push(EmittedHazard {
                    severity,
                    text,
                    attached_lines: vec![],
                });
                self.last_emit_time = Instant::now();
            }

            Classification::Outcome { text, captures } => {
                self.outcomes.push(CapturedOutcome { text, captures });
            }

            Classification::Noise {
                action: NoiseAction::Strip,
                ..
            } => {
                self.pending_count += 1;
            }

            Classification::Noise {
                action: NoiseAction::Dedup,
                text,
            } => {
                self.dedup.ingest(&text);
                self.pending_count += 1;
            }

            Classification::Prompt { text } => {
                self.flush_pending_count();
                self.dedup.flush_into(&mut self.output);
                self.output.push(format!(" ? {}", text));
                self.last_emit_time = Instant::now();
            }

            Classification::Unknown { .. } => {
                self.pending_count += 1;
            }
        }
    }

    /// Flush pending noise/unknown count to output.
    pub fn flush_pending(&mut self) {
        self.flush_pending_count();
        self.dedup.flush_into(&mut self.output);
    }

    fn flush_pending_count(&mut self) {
        if self.pending_count >= 10 {
            self.output
                .push(format!(" ... {} lines", self.pending_count));
        }
        self.pending_count = 0;
    }

    /// Finalize the buffer when the process exits, producing a Summary.
    pub fn finalize(
        mut self,
        exit_code: i32,
        elapsed: Duration,
        grammar: Option<&Grammar>,
        action: Option<&Action>,
    ) -> Summary {
        // Flush remaining pending
        self.flush_pending_count();
        self.dedup.flush_into(&mut self.output);

        // Build header
        let header = format!(
            "{} lines → exit {} ({:.1}s)",
            self.line_count,
            exit_code,
            elapsed.as_secs_f64()
        );

        // Build summary lines from outcomes
        let grammar_outcomes: Vec<grammar::CapturedOutcome> = self
            .outcomes
            .iter()
            .map(|o| grammar::CapturedOutcome {
                captures: o.captures.clone(),
            })
            .collect();

        let summary_lines = if let (Some(g), _) = (grammar, action) {
            let lines = grammar::format_summary(g, action, &grammar_outcomes, exit_code);
            if lines.is_empty() && !self.outcomes.is_empty() {
                // Grammar exists but template empty — show outcomes as-is
                self.outcomes
                    .iter()
                    .map(|o| format!(" + {}", o.text))
                    .collect()
            } else {
                lines
            }
        } else if !self.outcomes.is_empty() {
            // No grammar — show outcomes as-is
            self.outcomes
                .iter()
                .map(|o| format!(" + {}", o.text))
                .collect()
        } else {
            // No grammar, no outcomes — use last lines from ring buffer
            self.ring
                .iter()
                .map(|line| format!(" last: {}", line))
                .collect()
        };

        Summary {
            header,
            summary_lines,
            hazard_lines: self.output,
            exit_code,
        }
    }

    /// Get the current line count.
    pub fn line_count(&self) -> u64 {
        self.line_count
    }

    /// Get the accumulated output lines.
    pub fn output(&self) -> &[String] {
        &self.output
    }
}

impl Default for EmitBuffer {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::classifier::{Classification, NoiseAction};
    use crate::core::grammar::{load_grammar_from_str, Severity};

    // -----------------------------------------------------------------------
    // Accept logic
    // -----------------------------------------------------------------------

    // Test 1: Accept Hazard(Error) → immediate output with "!" prefix
    #[test]
    fn test_accept_hazard_error_immediate_output() {
        let mut buf = EmitBuffer::new();
        buf.accept(Classification::Hazard {
            severity: Severity::Error,
            text: "compile failed".into(),
            captures: HashMap::new(),
        });
        assert_eq!(buf.output.len(), 1);
        assert_eq!(buf.output[0], " ! compile failed");
    }

    // Test 2: Accept Hazard(Warning) → immediate output with "~" prefix
    #[test]
    fn test_accept_hazard_warning_immediate_output() {
        let mut buf = EmitBuffer::new();
        buf.accept(Classification::Hazard {
            severity: Severity::Warning,
            text: "deprecated API".into(),
            captures: HashMap::new(),
        });
        assert_eq!(buf.output.len(), 1);
        assert_eq!(buf.output[0], " ~ deprecated API");
    }

    // Test 3: Accept Outcome → stored but not emitted
    #[test]
    fn test_accept_outcome_stored_not_emitted() {
        let mut buf = EmitBuffer::new();
        buf.accept(Classification::Outcome {
            text: "147 packages installed".into(),
            captures: HashMap::from([("count".into(), "147".into())]),
        });
        assert!(buf.output.is_empty());
        assert_eq!(buf.outcomes.len(), 1);
        assert_eq!(buf.outcomes[0].text, "147 packages installed");
    }

    // Test 4: Accept Noise(Strip) → increments pending count, no output
    #[test]
    fn test_accept_noise_strip_increments_pending() {
        let mut buf = EmitBuffer::new();
        buf.accept(Classification::Noise {
            action: NoiseAction::Strip,
            text: "noisy line".into(),
        });
        assert!(buf.output.is_empty());
        assert_eq!(buf.pending_count, 1);
    }

    // Test 5: Accept Noise(Dedup) → sent to dedup engine
    #[test]
    fn test_accept_noise_dedup() {
        let mut buf = EmitBuffer::new();
        buf.accept(Classification::Noise {
            action: NoiseAction::Dedup,
            text: "Fetching https://registry.npmjs.org/a".into(),
        });
        assert!(buf.output.is_empty());
        assert_eq!(buf.pending_count, 1);
    }

    // Test 6: Accept Prompt → immediate output with "?" prefix
    #[test]
    fn test_accept_prompt_immediate_output() {
        let mut buf = EmitBuffer::new();
        buf.accept(Classification::Prompt {
            text: "Continue?".into(),
        });
        assert_eq!(buf.output.len(), 1);
        assert_eq!(buf.output[0], " ? Continue?");
    }

    // Test 7: Accept Unknown → increments pending count
    #[test]
    fn test_accept_unknown_increments_pending() {
        let mut buf = EmitBuffer::new();
        buf.accept(Classification::Unknown {
            text: "some output".into(),
        });
        assert_eq!(buf.pending_count, 1);
        assert!(buf.output.is_empty());
    }

    // -----------------------------------------------------------------------
    // Flush behavior
    // -----------------------------------------------------------------------

    // Test 8: Flush pending count >= 10 → "... N lines"
    #[test]
    fn test_flush_pending_large_count() {
        let mut buf = EmitBuffer::new();
        for _ in 0..15 {
            buf.accept(Classification::Noise {
                action: NoiseAction::Strip,
                text: "noise".into(),
            });
        }
        buf.flush_pending();
        assert_eq!(buf.output.len(), 1);
        assert_eq!(buf.output[0], " ... 15 lines");
    }

    // Test 9: Flush pending count < 10 → no output (too noisy)
    #[test]
    fn test_flush_pending_small_count_no_output() {
        let mut buf = EmitBuffer::new();
        for _ in 0..5 {
            buf.accept(Classification::Noise {
                action: NoiseAction::Strip,
                text: "noise".into(),
            });
        }
        buf.flush_pending();
        assert!(buf.output.is_empty());
    }

    // Test 10: Hazard flushes pending count first
    #[test]
    fn test_hazard_flushes_pending_first() {
        let mut buf = EmitBuffer::new();
        // Add 15 noise lines
        for _ in 0..15 {
            buf.accept(Classification::Noise {
                action: NoiseAction::Strip,
                text: "noise".into(),
            });
        }
        // Now a hazard arrives — should flush pending count first
        buf.accept(Classification::Hazard {
            severity: Severity::Error,
            text: "error occurred".into(),
            captures: HashMap::new(),
        });
        assert_eq!(buf.output.len(), 2);
        assert_eq!(buf.output[0], " ... 15 lines");
        assert_eq!(buf.output[1], " ! error occurred");
    }

    // Test 11: Prompt flushes pending count first
    #[test]
    fn test_prompt_flushes_pending_first() {
        let mut buf = EmitBuffer::new();
        for _ in 0..12 {
            buf.accept(Classification::Unknown {
                text: "output".into(),
            });
        }
        buf.accept(Classification::Prompt {
            text: "Continue?".into(),
        });
        assert_eq!(buf.output.len(), 2);
        assert_eq!(buf.output[0], " ... 12 lines");
        assert_eq!(buf.output[1], " ? Continue?");
    }

    // -----------------------------------------------------------------------
    // Ring buffer
    // -----------------------------------------------------------------------

    // Test 12: Ring buffer stores last 5 lines
    #[test]
    fn test_ring_buffer_stores_last_5() {
        let mut buf = EmitBuffer::new();
        for i in 0..7 {
            buf.accept(Classification::Unknown {
                text: format!("line {}", i),
            });
        }
        assert_eq!(buf.ring.len(), 5);
        let lines: Vec<&String> = buf.ring.iter().collect();
        assert_eq!(lines[0], "line 2");
        assert_eq!(lines[4], "line 6");
    }

    // Test 13: Ring buffer wraps correctly
    #[test]
    fn test_ring_buffer_wraps() {
        let mut ring = RingBuffer::new(3);
        ring.push("a".into());
        ring.push("b".into());
        ring.push("c".into());
        ring.push("d".into()); // pushes out "a"
        let items: Vec<&String> = ring.iter().collect();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], "b");
        assert_eq!(items[1], "c");
        assert_eq!(items[2], "d");
    }

    // -----------------------------------------------------------------------
    // Line count
    // -----------------------------------------------------------------------

    // Test 14: line_count increments on every accept
    #[test]
    fn test_line_count_increments() {
        let mut buf = EmitBuffer::new();
        buf.accept(Classification::Unknown {
            text: "a".into(),
        });
        buf.accept(Classification::Hazard {
            severity: Severity::Error,
            text: "b".into(),
            captures: HashMap::new(),
        });
        buf.accept(Classification::Outcome {
            text: "c".into(),
            captures: HashMap::new(),
        });
        assert_eq!(buf.line_count(), 3);
    }

    // -----------------------------------------------------------------------
    // Finalize
    // -----------------------------------------------------------------------

    // Test 15: Finalize header format
    #[test]
    fn test_finalize_header_format() {
        let mut buf = EmitBuffer::new();
        for _ in 0..5 {
            buf.accept(Classification::Unknown {
                text: "line".into(),
            });
        }
        let summary = buf.finalize(0, Duration::from_secs_f64(12.3), None, None);
        assert!(summary.header.starts_with("5 lines → exit 0"));
        assert!(summary.header.contains("12.3s"));
        assert_eq!(summary.exit_code, 0);
    }

    // Test 16: Finalize without grammar, with outcomes → "+ text"
    #[test]
    fn test_finalize_no_grammar_with_outcomes() {
        let mut buf = EmitBuffer::new();
        buf.accept(Classification::Outcome {
            text: "147 packages installed".into(),
            captures: HashMap::new(),
        });
        let summary = buf.finalize(0, Duration::from_secs(5), None, None);
        assert_eq!(summary.summary_lines.len(), 1);
        assert_eq!(summary.summary_lines[0], " + 147 packages installed");
    }

    // Test 17: Finalize without grammar, no outcomes → last lines from ring
    #[test]
    fn test_finalize_no_grammar_no_outcomes_uses_ring() {
        let mut buf = EmitBuffer::new();
        buf.accept(Classification::Unknown {
            text: "final output".into(),
        });
        let summary = buf.finalize(0, Duration::from_secs(1), None, None);
        assert_eq!(summary.summary_lines.len(), 1);
        assert_eq!(summary.summary_lines[0], " last: final output");
    }

    // Test 18: Finalize with grammar and outcomes → formatted summary
    #[test]
    fn test_finalize_with_grammar_outcomes() {
        let toml_str = r#"
[tool]
name = "npm"

[actions.install]
detect = ["install"]

[[actions.install.outcome]]
pattern = '^added (?P<count>\d+) packages? in (?P<time>.+)'
action = "promote"
captures = ["count", "time"]

[actions.install.summary]
success = "+ {count} packages installed ({time})"
failure = "! npm install failed (exit {exit_code})"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let action = grammar.actions.get("install").unwrap();

        let mut buf = EmitBuffer::new();
        buf.accept(Classification::Outcome {
            text: "added 42 packages in 3.2s".into(),
            captures: HashMap::from([
                ("count".into(), "42".into()),
                ("time".into(), "3.2s".into()),
            ]),
        });

        let summary = buf.finalize(0, Duration::from_secs(3), Some(&grammar), Some(action));
        assert_eq!(summary.summary_lines.len(), 1);
        assert_eq!(
            summary.summary_lines[0],
            "+ 42 packages installed (3.2s)"
        );
    }

    // Test 19: Finalize flushes remaining pending
    #[test]
    fn test_finalize_flushes_pending() {
        let mut buf = EmitBuffer::new();
        for _ in 0..20 {
            buf.accept(Classification::Noise {
                action: NoiseAction::Strip,
                text: "noise".into(),
            });
        }
        let summary = buf.finalize(0, Duration::from_secs(1), None, None);
        assert!(summary
            .hazard_lines
            .iter()
            .any(|l| l.contains("20 lines")));
    }

    // Test 20: Hazard stored in hazards vec
    #[test]
    fn test_hazard_stored_in_hazards() {
        let mut buf = EmitBuffer::new();
        buf.accept(Classification::Hazard {
            severity: Severity::Warning,
            text: "deprecated".into(),
            captures: HashMap::new(),
        });
        assert_eq!(buf.hazards.len(), 1);
        assert_eq!(buf.hazards[0].severity, Severity::Warning);
        assert_eq!(buf.hazards[0].text, "deprecated");
    }

    // -----------------------------------------------------------------------
    // TimingConfig
    // -----------------------------------------------------------------------

    // Test 21: Default timing config values
    #[test]
    fn test_timing_config_defaults() {
        let tc = TimingConfig::default();
        assert_eq!(tc.partial_line_timeout, Duration::from_millis(500));
        assert_eq!(tc.silence_timeout, Duration::from_secs(2));
        assert_eq!(tc.flush_interval, Duration::from_secs(5));
        assert_eq!(tc.flush_debounce, Duration::from_millis(200));
    }

    // -----------------------------------------------------------------------
    // Finalize with failure exit code
    // -----------------------------------------------------------------------

    // Test 22: Finalize with exit code != 0
    #[test]
    fn test_finalize_failure_exit_code() {
        let mut buf = EmitBuffer::new();
        buf.accept(Classification::Unknown {
            text: "output".into(),
        });
        let summary = buf.finalize(1, Duration::from_secs(1), None, None);
        assert!(summary.header.contains("exit 1"));
        assert_eq!(summary.exit_code, 1);
    }

    // Test 23: Empty output finalize
    #[test]
    fn test_finalize_empty_output() {
        let buf = EmitBuffer::new();
        let summary = buf.finalize(0, Duration::from_secs(0), None, None);
        assert!(summary.header.contains("0 lines"));
        assert!(summary.summary_lines.is_empty());
        assert!(summary.hazard_lines.is_empty());
    }
}
