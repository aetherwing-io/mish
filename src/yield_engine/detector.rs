//! Core yield detection logic.
//!
//! The yield detector monitors process output for signs that a process is
//! waiting for input. It uses a **silence + prompt heuristic**:
//!
//! 1. Track output bytes — reset a silence timer on each byte received.
//! 2. When silence exceeds `silence_timeout_ms`:
//!    a. Check last `prompt_window_bytes` bytes against configurable `prompt_patterns`.
//!    b. If match -> YIELD detected.
//!    c. If no match -> continue monitoring.
//! 3. On YIELD, evaluate policy via `policy::matcher::check_yield`.

use std::time::Instant;

use regex::Regex;

use crate::policy::config::CompiledPolicy;
use crate::policy::matcher::{check_yield, PolicyDecision};

// ---------------------------------------------------------------------------
// YieldConfig
// ---------------------------------------------------------------------------

/// Configuration for the yield detector.
#[derive(Debug, Clone)]
pub struct YieldConfig {
    /// Silence timeout in milliseconds before checking for prompts. Default: 2500.
    pub silence_timeout_ms: u64,
    /// Maximum bytes from the end of output to check for prompt patterns. Default: 256.
    pub prompt_window_bytes: usize,
    /// Regex patterns that indicate a prompt (process waiting for input).
    pub prompt_patterns: Vec<Regex>,
}

impl Default for YieldConfig {
    fn default() -> Self {
        let patterns = [
            r"\?\s*$",
            r":\s*$",
            r">\s*$",
            r"(?i)password",
            r"(?i)passphrase",
            r"\[y/N\]|\[Y/n\]|\[yes/no\]",
        ];
        Self {
            silence_timeout_ms: 2500,
            prompt_window_bytes: 256,
            prompt_patterns: patterns
                .iter()
                .map(|p| Regex::new(p).expect("default prompt pattern must compile"))
                .collect(),
        }
    }
}

impl YieldConfig {
    /// Build a YieldConfig from the config-level `crate::config::YieldConfig`.
    ///
    /// Compiles prompt pattern strings into regexes. Invalid patterns are
    /// skipped with a warning to stderr.
    pub fn from_config(cfg: &crate::config::YieldConfig) -> Self {
        let mut patterns = Vec::new();
        for pat_str in &cfg.prompt_patterns {
            match Regex::new(pat_str) {
                Ok(re) => patterns.push(re),
                Err(e) => {
                    eprintln!("mish: warning: invalid yield prompt pattern {pat_str:?}: {e}");
                }
            }
        }
        Self {
            silence_timeout_ms: cfg.silence_timeout_ms,
            prompt_window_bytes: 256,
            prompt_patterns: patterns,
        }
    }
}

// ---------------------------------------------------------------------------
// YieldDetection
// ---------------------------------------------------------------------------

/// Result of yield detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YieldDetection {
    /// Process is waiting for input, prompt text matched.
    Yielded {
        prompt_text: String,
        policy_decision: PolicyDecision,
    },
    /// No yield detected (silence timeout not reached, or no prompt match).
    NotYielded,
}

// ---------------------------------------------------------------------------
// YieldDetector
// ---------------------------------------------------------------------------

/// State tracking for yield detection on a single process.
#[derive(Debug)]
pub struct YieldDetector {
    config: YieldConfig,
    /// Circular buffer of last N bytes of output.
    tail_buffer: Vec<u8>,
    /// Timestamp of last byte received.
    last_byte_at: Option<Instant>,
    /// Whether we've received any output at all (prevents false yield on slow startup).
    has_received_output: bool,
    /// Whether yield has been detected (prevents re-triggering).
    yielded: bool,
}

impl YieldDetector {
    /// Create a new detector with the given config.
    pub fn new(config: YieldConfig) -> Self {
        Self {
            config,
            tail_buffer: Vec::new(),
            last_byte_at: None,
            has_received_output: false,
            yielded: false,
        }
    }

    /// Feed output bytes from the process. Resets silence timer.
    pub fn feed(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        self.has_received_output = true;
        self.last_byte_at = Some(Instant::now());

        // Append bytes to tail buffer, then truncate from the front if it
        // exceeds prompt_window_bytes.
        self.tail_buffer.extend_from_slice(bytes);
        let window = self.config.prompt_window_bytes;
        if self.tail_buffer.len() > window {
            let excess = self.tail_buffer.len() - window;
            self.tail_buffer.drain(..excess);
        }
    }

    /// Check if the process has yielded. Call periodically (e.g., every 100ms).
    ///
    /// `command` is the original command string (for scope matching).
    /// `policy` is the compiled policy (for auto_confirm/yield_to_operator lookup).
    pub fn check(&mut self, command: &str, policy: &CompiledPolicy) -> YieldDetection {
        // Re-trigger prevention: once yielded, return NotYielded until reset().
        if self.yielded {
            return YieldDetection::NotYielded;
        }

        // Slow startup guard: no output received yet → no yield.
        if !self.has_received_output {
            return YieldDetection::NotYielded;
        }

        // Check if silence timeout has elapsed.
        let last_byte = match self.last_byte_at {
            Some(t) => t,
            None => return YieldDetection::NotYielded,
        };

        let elapsed_ms = last_byte.elapsed().as_millis() as u64;
        if elapsed_ms < self.config.silence_timeout_ms {
            return YieldDetection::NotYielded;
        }

        // Empty tail buffer → no prompt to match.
        if self.tail_buffer.is_empty() {
            return YieldDetection::NotYielded;
        }

        // Convert tail buffer to string for pattern matching.
        let tail_text = String::from_utf8_lossy(&self.tail_buffer).to_string();

        // Check prompt patterns.
        let matched = self.config.prompt_patterns.iter().any(|re| re.is_match(&tail_text));
        if !matched {
            return YieldDetection::NotYielded;
        }

        // Prompt detected — evaluate policy.
        let decision = check_yield(command, &tail_text, policy);
        self.yielded = true;

        YieldDetection::Yielded {
            prompt_text: tail_text,
            policy_decision: decision,
        }
    }

    /// Reset the detector state (e.g., after input is sent to the process).
    pub fn reset(&mut self) {
        self.tail_buffer.clear();
        self.last_byte_at = None;
        self.has_received_output = false;
        self.yielded = false;
    }

    /// Get the prompt tail text (last N bytes as lossy UTF-8).
    pub fn prompt_tail(&self) -> String {
        String::from_utf8_lossy(&self.tail_buffer).to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::policy::config::CompiledPolicy;
    use std::thread;
    use std::time::Duration;

    /// Helper: compile a policy from TOML.
    fn policy_from_toml(toml_str: &str) -> CompiledPolicy {
        let cfg = config::load_config_from_str(toml_str).expect("test TOML should parse");
        CompiledPolicy::compile(&cfg).expect("policy should compile")
    }

    /// Helper: create a detector with a very short timeout for testing.
    fn fast_detector() -> YieldDetector {
        let mut config = YieldConfig::default();
        config.silence_timeout_ms = 50; // 50ms for fast tests
        YieldDetector::new(config)
    }

    /// Helper: empty policy (no rules).
    fn empty_policy() -> CompiledPolicy {
        policy_from_toml("")
    }

    // -- Test 1: Prompt after silence -> yield --

    #[test]
    fn test_prompt_after_silence_yields() {
        let mut detector = fast_detector();
        let policy = empty_policy();

        detector.feed(b"Password: ");
        thread::sleep(Duration::from_millis(80));

        match detector.check("ssh user@host", &policy) {
            YieldDetection::Yielded {
                prompt_text,
                policy_decision,
            } => {
                assert!(
                    prompt_text.contains("Password"),
                    "prompt_text should contain 'Password', got: {prompt_text}"
                );
                assert_eq!(policy_decision, PolicyDecision::FallThroughToLlm);
            }
            YieldDetection::NotYielded => {
                panic!("expected Yielded after silence timeout with password prompt");
            }
        }
    }

    // -- Test 2: Active output -> no yield --

    #[test]
    fn test_active_output_no_yield() {
        let mut detector = fast_detector();
        let policy = empty_policy();

        // Feed bytes continuously with no silence gap.
        for _ in 0..10 {
            detector.feed(b"processing data...\n");
            // No sleep — feed immediately.
        }

        // Check immediately (no silence gap).
        let result = detector.check("some-cmd", &policy);
        assert_eq!(result, YieldDetection::NotYielded);
    }

    // -- Test 3: Slow startup -> no false yield --

    #[test]
    fn test_slow_startup_no_false_yield() {
        let mut detector = fast_detector();
        let policy = empty_policy();

        // Don't feed any bytes — simulate slow startup.
        thread::sleep(Duration::from_millis(80));

        let result = detector.check("some-cmd", &policy);
        assert_eq!(
            result,
            YieldDetection::NotYielded,
            "should not yield when no output has been received"
        );
    }

    // -- Test 4: No prompt match -> no yield --

    #[test]
    fn test_no_prompt_match_no_yield() {
        let mut detector = fast_detector();
        let policy = empty_policy();

        // Feed text that doesn't match any prompt pattern.
        detector.feed(b"processing data...\nall done");
        thread::sleep(Duration::from_millis(80));

        let result = detector.check("some-cmd", &policy);
        assert_eq!(
            result,
            YieldDetection::NotYielded,
            "should not yield when output doesn't match prompt patterns"
        );
    }

    // -- Test 5: Auto-confirm match -> response in decision --

    #[test]
    fn test_auto_confirm_response() {
        let mut config = YieldConfig::default();
        config.silence_timeout_ms = 50;
        let mut detector = YieldDetector::new(config);

        let policy = policy_from_toml(
            r#"
[[policy.auto_confirm]]
match = '\[Y/n\]'
respond = "Y\n"
scope = ["apt"]
"#,
        );

        detector.feed(b"Do you want to continue? [Y/n] ");
        thread::sleep(Duration::from_millis(80));

        match detector.check("apt install vim", &policy) {
            YieldDetection::Yielded {
                policy_decision, ..
            } => {
                assert_eq!(
                    policy_decision,
                    PolicyDecision::AutoConfirm {
                        response: "Y\n".into()
                    }
                );
            }
            YieldDetection::NotYielded => {
                panic!("expected Yielded with AutoConfirm decision");
            }
        }
    }

    // -- Test 6: yield_to_operator -> handoff decision --

    #[test]
    fn test_yield_to_operator_decision() {
        let mut config = YieldConfig::default();
        config.silence_timeout_ms = 50;
        let mut detector = YieldDetector::new(config);

        let policy = policy_from_toml(
            r#"
[[policy.yield_to_operator]]
match = 'Password'
notify = true
"#,
        );

        detector.feed(b"Password:");
        thread::sleep(Duration::from_millis(80));

        match detector.check("ssh user@host", &policy) {
            YieldDetection::Yielded {
                policy_decision, ..
            } => {
                assert_eq!(
                    policy_decision,
                    PolicyDecision::YieldToOperator { notify: true }
                );
            }
            YieldDetection::NotYielded => {
                panic!("expected Yielded with YieldToOperator decision");
            }
        }
    }

    // -- Test 7: No policy match -> FallThroughToLlm --

    #[test]
    fn test_no_policy_match_falls_through() {
        let mut detector = fast_detector();
        let policy = empty_policy();

        // Feed a prompt that matches default patterns.
        detector.feed(b"Enter your name: ");
        thread::sleep(Duration::from_millis(80));

        match detector.check("some-cmd", &policy) {
            YieldDetection::Yielded {
                policy_decision, ..
            } => {
                assert_eq!(policy_decision, PolicyDecision::FallThroughToLlm);
            }
            YieldDetection::NotYielded => {
                panic!("expected Yielded with FallThroughToLlm decision");
            }
        }
    }

    // -- Test 8: Reset clears yield state --

    #[test]
    fn test_reset_clears_yield_state() {
        let mut detector = fast_detector();
        let policy = empty_policy();

        // First yield.
        detector.feed(b"Password: ");
        thread::sleep(Duration::from_millis(80));
        assert!(matches!(
            detector.check("cmd", &policy),
            YieldDetection::Yielded { .. }
        ));

        // After yield, subsequent checks return NotYielded.
        assert_eq!(
            detector.check("cmd", &policy),
            YieldDetection::NotYielded,
            "should not re-trigger without reset"
        );

        // Reset and feed new data.
        detector.reset();
        detector.feed(b"Another prompt? ");
        thread::sleep(Duration::from_millis(80));

        // Should yield again after reset.
        match detector.check("cmd", &policy) {
            YieldDetection::Yielded { prompt_text, .. } => {
                assert!(
                    prompt_text.contains("Another prompt?"),
                    "should see new prompt text after reset"
                );
            }
            YieldDetection::NotYielded => {
                panic!("expected Yielded after reset + new prompt");
            }
        }
    }

    // -- Test 9: Tail buffer rotation --

    #[test]
    fn test_tail_buffer_rotation() {
        let mut config = YieldConfig::default();
        config.prompt_window_bytes = 16; // tiny window for testing
        config.silence_timeout_ms = 50;
        let mut detector = YieldDetector::new(config);

        // Feed more than prompt_window_bytes.
        detector.feed(b"0123456789abcdef_extra_bytes_that_should_be_dropped");

        // Only the last 16 bytes should be kept.
        let tail = detector.prompt_tail();
        assert_eq!(tail.len(), 16, "tail buffer should be capped at window size");
        assert_eq!(
            tail, "hould_be_dropped",
            "should keep only last 16 bytes"
        );

        // Wait — the remaining text in tail doesn't end with ":" or "?" so
        // it should not yield.
        let policy = empty_policy();
        thread::sleep(Duration::from_millis(80));
        assert_eq!(
            detector.check("cmd", &policy),
            YieldDetection::NotYielded,
            "rotated buffer without prompt pattern should not yield"
        );
    }

    // -- Test 10: Re-trigger prevention --

    #[test]
    fn test_re_trigger_prevention() {
        let mut detector = fast_detector();
        let policy = empty_policy();

        detector.feed(b"Password: ");
        thread::sleep(Duration::from_millis(80));

        // First check → Yielded.
        assert!(matches!(
            detector.check("cmd", &policy),
            YieldDetection::Yielded { .. }
        ));

        // Subsequent checks → NotYielded (without reset).
        assert_eq!(detector.check("cmd", &policy), YieldDetection::NotYielded);
        assert_eq!(detector.check("cmd", &policy), YieldDetection::NotYielded);
    }

    // -- Test 11: Default config has sane values --

    #[test]
    fn test_default_config_sane() {
        let config = YieldConfig::default();
        assert_eq!(config.silence_timeout_ms, 2500, "default silence timeout");
        assert_eq!(config.prompt_window_bytes, 256, "default window size");
        assert!(
            !config.prompt_patterns.is_empty(),
            "default prompt patterns should be non-empty"
        );
        assert!(
            config.prompt_patterns.len() >= 6,
            "expected at least 6 default patterns, got {}",
            config.prompt_patterns.len()
        );

        // Verify patterns actually compile and match expected strings.
        let patterns_and_examples = [
            ("?", "Continue? "),
            (":", "Password: "),
            (">", "prompt> "),
            ("password", "Enter your password"),
            ("passphrase", "Enter passphrase"),
            ("[Y/n]", "Continue? [Y/n]"),
        ];
        for (i, (desc, example)) in patterns_and_examples.iter().enumerate() {
            assert!(
                config.prompt_patterns[i].is_match(example),
                "default pattern {i} ({desc}) should match {example:?}"
            );
        }
    }

    // -- Test 12: from_config builds from crate::config::YieldConfig --

    #[test]
    fn test_from_config() {
        let cfg = crate::config::YieldConfig {
            silence_timeout_ms: 5000,
            prompt_patterns: vec![
                "custom_prompt".into(),
                "another_pattern:\\s*$".into(),
            ],
        };

        let yield_config = YieldConfig::from_config(&cfg);
        assert_eq!(yield_config.silence_timeout_ms, 5000);
        assert_eq!(yield_config.prompt_patterns.len(), 2);
        assert!(yield_config.prompt_patterns[0].is_match("custom_prompt"));
        assert!(yield_config.prompt_patterns[1].is_match("another_pattern: "));
    }

    // -- Test 13: from_config skips invalid patterns --

    #[test]
    fn test_from_config_skips_invalid() {
        let cfg = crate::config::YieldConfig {
            silence_timeout_ms: 1000,
            prompt_patterns: vec![
                "valid_pattern".into(),
                "[invalid(".into(), // bad regex
                "also_valid".into(),
            ],
        };

        let yield_config = YieldConfig::from_config(&cfg);
        assert_eq!(
            yield_config.prompt_patterns.len(),
            2,
            "should skip invalid pattern and keep 2 valid ones"
        );
    }

    // -- Test 14: feed with empty bytes is a no-op --

    #[test]
    fn test_feed_empty_bytes_noop() {
        let mut detector = fast_detector();
        let policy = empty_policy();

        detector.feed(b"");

        // has_received_output should still be false.
        thread::sleep(Duration::from_millis(80));
        assert_eq!(
            detector.check("cmd", &policy),
            YieldDetection::NotYielded,
            "empty feed should not count as received output"
        );
    }

    // -- Test 15: prompt_tail returns lossy UTF-8 for binary data --

    #[test]
    fn test_prompt_tail_lossy_utf8() {
        let mut detector = fast_detector();

        // Feed some bytes with invalid UTF-8.
        detector.feed(&[0xFF, 0xFE, b'h', b'i']);

        let tail = detector.prompt_tail();
        assert!(
            tail.contains("hi"),
            "tail should contain valid portion: {tail}"
        );
        // The lossy conversion should produce replacement characters.
        assert!(
            tail.contains('\u{FFFD}'),
            "tail should contain replacement char for invalid UTF-8: {tail}"
        );
    }

    // -- Test 16: question mark at end matches --

    #[test]
    fn test_question_mark_pattern() {
        let mut detector = fast_detector();
        let policy = empty_policy();

        detector.feed(b"Are you sure?");
        thread::sleep(Duration::from_millis(80));

        assert!(
            matches!(
                detector.check("cmd", &policy),
                YieldDetection::Yielded { .. }
            ),
            "text ending with '?' should trigger yield"
        );
    }

    // -- Test 17: yes/no prompt pattern --

    #[test]
    fn test_yes_no_prompt_pattern() {
        let mut detector = fast_detector();
        let policy = empty_policy();

        detector.feed(b"Install packages? [yes/no]");
        thread::sleep(Duration::from_millis(80));

        assert!(
            matches!(
                detector.check("cmd", &policy),
                YieldDetection::Yielded { .. }
            ),
            "[yes/no] pattern should trigger yield"
        );
    }
}
