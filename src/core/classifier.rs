/// Three-tier classification engine.
///
/// Assigns a classification to each line: Hazard, Outcome, Noise, Prompt, or Unknown.
/// Evaluates in order: Tier 1 (grammar rules) → Tier 2 (universal patterns) → Tier 3 (structural).

use std::collections::HashMap;
use std::time::Instant;

use regex::Regex;

use crate::core::grammar::{Action, Grammar, Rule, RuleAction, Severity};
use crate::core::line_buffer::Line;
use crate::squasher::vte_strip::{AnsiColor, AnsiMetadata, VteStripper};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// How a noise line should be handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseAction {
    Strip,
    Dedup,
}

/// Classification result for a single line.
#[derive(Debug, Clone, PartialEq)]
pub enum Classification {
    Hazard {
        severity: Severity,
        text: String,
        captures: HashMap<String, String>,
    },
    Outcome {
        text: String,
        captures: HashMap<String, String>,
    },
    Noise {
        action: NoiseAction,
        text: String,
    },
    Prompt {
        text: String,
    },
    Unknown {
        text: String,
    },
}

impl Classification {
    /// Get the text content of this classification, if any.
    pub fn text(&self) -> Option<&str> {
        match self {
            Classification::Hazard { text, .. } => Some(text),
            Classification::Outcome { text, .. } => Some(text),
            Classification::Noise { text, .. } => Some(text),
            Classification::Prompt { text } => Some(text),
            Classification::Unknown { text } => Some(text),
        }
    }
}

/// Classifier state machine states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassifierState {
    Idle,
    Running,
    MaybePrompt,
    AwaitingInput,
    DoneSuccess,
    DoneFailure,
    DoneKilled,
    ConsumingTrace,
}

// ---------------------------------------------------------------------------
// Universal patterns (Tier 2)
// ---------------------------------------------------------------------------

struct UniversalPatterns {
    /// Error patterns anchored at line start (strongest signal)
    error_start: Vec<Regex>,
    /// Error patterns that match anywhere in line (weaker)
    error_anywhere: Vec<Regex>,
    /// Warning patterns
    warning_start: Vec<Regex>,
    warning_anywhere: Vec<Regex>,
    /// Success keywords (used with ANSI green)
    success_keywords: Vec<Regex>,
    /// Stack trace start patterns
    stack_start: Vec<Regex>,
    /// Stack trace continuation patterns
    #[allow(dead_code)]
    stack_continue: Regex,
    /// Prompt patterns (only applied to Partial lines)
    prompt_patterns: Vec<Regex>,
    /// Decorative/separator lines
    decorative: Regex,
    /// Box-drawing characters
    box_drawing: Regex,
    /// Empty/whitespace
    whitespace_only: Regex,
}

impl UniversalPatterns {
    fn new() -> Self {
        Self {
            error_start: vec![
                Regex::new(r"(?i)^(error|Error|ERROR)[:\s]").unwrap(),
                Regex::new(r"(?i)^(FAIL|FAILED|FATAL|fatal|Fatal)[:\s]").unwrap(),
                Regex::new(r"(?i)^(panic|Panic|PANIC)[:\s]").unwrap(),
                Regex::new(r"^Traceback \(most recent call last\)").unwrap(),
                Regex::new(r"^Exception").unwrap(),
                Regex::new(r"^Unhandled\s").unwrap(),
            ],
            error_anywhere: vec![
                Regex::new(r"command not found$").unwrap(),
                Regex::new(r"(?i)permission denied").unwrap(),
                Regex::new(r"No such file or directory").unwrap(),
                Regex::new(r"ENOENT|EACCES|EPERM|ECONNREFUSED").unwrap(),
                Regex::new(r"(?i)segmentation fault|SIGSEGV").unwrap(),
                Regex::new(r"(?i)killed|OOMKilled").unwrap(),
                Regex::new(r"(?i)out of memory").unwrap(),
                Regex::new(r"cannot find module").unwrap(),
                Regex::new(r"ModuleNotFoundError").unwrap(),
                Regex::new(r"ImportError").unwrap(),
                Regex::new(r"SyntaxError").unwrap(),
            ],
            warning_start: vec![
                Regex::new(r"(?i)^(warn|WARN|Warning|WARNING|DEPRECAT)[:\s!]").unwrap(),
            ],
            warning_anywhere: vec![
                Regex::new(r"\bdeprecated\b").unwrap(),
                Regex::new(r"⚠").unwrap(),
                Regex::new(r"\b(moderate|high|critical)\s+vulnerabilit").unwrap(),
            ],
            success_keywords: vec![
                Regex::new(r"(?i)\b(success|succeeded|passed|complete|done|ok|ready)\b").unwrap(),
            ],
            stack_start: vec![
                // Node.js
                Regex::new(r"^\s+at\s+\S+\s+\(.*:\d+:\d+\)").unwrap(),
                Regex::new(r"^\s+at\s+\S+\s+\(node:").unwrap(),
                // Python
                Regex::new(r#"^\s+File ".+", line \d+"#).unwrap(),
                // Go
                Regex::new(r"^goroutine \d+").unwrap(),
                Regex::new(r"^\s+.+\.go:\d+").unwrap(),
                // Rust
                Regex::new(r"^\s+\d+:\s+0x[0-9a-f]+\s+-\s+").unwrap(),
                // Java/Kotlin
                Regex::new(r"^\s+at\s+[\w.$]+\([\w.]+:\d+\)").unwrap(),
                Regex::new(r"^Caused by:").unwrap(),
                // Generic file:line:col
                Regex::new(r"^\S+:\d+:\d+:").unwrap(),
            ],
            stack_continue: Regex::new(r"^\s+").unwrap(),
            prompt_patterns: vec![
                Regex::new(r"\?\s*$").unwrap(),
                Regex::new(r"\(y/n\)|\(Y/N\)|\[y/N\]|\[Y/n\]").unwrap(),
                Regex::new(r"(?i)[Pp]assword:?\s*$").unwrap(),
                Regex::new(r"(?i)[Ee]nter\s.*:\s*$").unwrap(),
                Regex::new(r"(?i)[Pp]ress any key").unwrap(),
                Regex::new(r"\S+>\s*$").unwrap(),
                Regex::new(r":\s*$").unwrap(),
                Regex::new(r"\$\s*$").unwrap(),
            ],
            decorative: Regex::new(r"^[\s=\-─━_*#~.·•]+$").unwrap(),
            box_drawing: Regex::new(r"^[┌┐└┘├┤┬┴┼│─]+$").unwrap(),
            whitespace_only: Regex::new(r"^\s*$").unwrap(),
        }
    }
}

// ---------------------------------------------------------------------------
// Classifier
// ---------------------------------------------------------------------------

pub struct Classifier {
    state: ClassifierState,

    // Tier 1: Grammar rules (cloned from Grammar/Action)
    hazard_rules: Vec<Rule>,
    outcome_rules: Vec<Rule>,
    action_noise_rules: Vec<Rule>,
    global_noise_rules: Vec<Rule>,
    has_grammar: bool,

    // Tier 2: Universal patterns
    universal: UniversalPatterns,

    // Tier 3: Structural state
    previous_line: Option<String>,

    // Multiline hazard state
    multiline_remaining: u32,
    current_hazard_severity: Severity,
    current_hazard_text: String,
    current_hazard_captures: HashMap<String, String>,
    current_hazard_attached: Vec<String>,

    // Timing
    last_line_time: Instant,
}

impl Classifier {
    /// Create a new classifier, optionally with grammar rules for Tier 1.
    pub fn new(grammar: Option<&Grammar>, action: Option<&Action>) -> Self {
        let (hazard_rules, outcome_rules, action_noise_rules, global_noise_rules, has_grammar) =
            if let Some(g) = grammar {
                let (hr, or, nr) = if let Some(a) = action {
                    (a.hazard.clone(), a.outcome.clone(), a.noise.clone())
                } else if let Some(ref fb) = g.fallback {
                    (fb.hazard.clone(), fb.outcome.clone(), fb.noise.clone())
                } else {
                    (vec![], vec![], vec![])
                };
                (hr, or, nr, g.global_noise.clone(), true)
            } else {
                (vec![], vec![], vec![], vec![], false)
            };

        Self {
            state: ClassifierState::Idle,
            hazard_rules,
            outcome_rules,
            action_noise_rules,
            global_noise_rules,
            has_grammar,
            universal: UniversalPatterns::new(),
            previous_line: None,
            multiline_remaining: 0,
            current_hazard_severity: Severity::Error,
            current_hazard_text: String::new(),
            current_hazard_captures: HashMap::new(),
            current_hazard_attached: Vec::new(),
            last_line_time: Instant::now(),
        }
    }

    /// Get the current state of the classifier.
    pub fn state(&self) -> ClassifierState {
        self.state
    }

    /// Transition from AwaitingInput back to Running (stdin received).
    pub fn resume_from_prompt(&mut self) {
        if self.state == ClassifierState::AwaitingInput {
            self.state = ClassifierState::Running;
        }
    }

    /// Notify the classifier that the process has exited.
    pub fn notify_exit(&mut self, exit_code: i32, signaled: bool) {
        self.state = if signaled {
            ClassifierState::DoneKilled
        } else if exit_code == 0 {
            ClassifierState::DoneSuccess
        } else {
            ClassifierState::DoneFailure
        };
    }

    /// Classify a line from the line buffer.
    pub fn classify(&mut self, line: Line) -> Classification {
        // Transition from Idle to Running on first input
        if self.state == ClassifierState::Idle {
            self.state = ClassifierState::Running;
        }

        self.last_line_time = Instant::now();

        match line {
            Line::Overwrite(_) => Classification::Noise {
                action: NoiseAction::Strip,
                text: String::new(),
            },
            Line::Partial(ref text) => self.classify_partial(text),
            Line::Complete(ref text) => self.classify_complete(text),
        }
    }

    // -----------------------------------------------------------------------
    // Internal classification methods
    // -----------------------------------------------------------------------

    fn classify_partial(&mut self, raw: &str) -> Classification {
        let meta = VteStripper::strip(raw.as_bytes());
        let clean = &meta.clean_text;

        // Check prompt patterns — prompts require Partial line
        for pat in &self.universal.prompt_patterns {
            if pat.is_match(clean) {
                self.state = ClassifierState::AwaitingInput;
                return Classification::Prompt {
                    text: clean.clone(),
                };
            }
        }

        Classification::Unknown {
            text: clean.clone(),
        }
    }

    fn classify_complete(&mut self, raw: &str) -> Classification {
        let meta = VteStripper::strip(raw.as_bytes());
        let clean = &meta.clean_text;

        // If consuming a multiline hazard, attach this line
        if self.multiline_remaining > 0 {
            self.multiline_remaining -= 1;
            self.current_hazard_attached.push(clean.clone());

            // If this was the last attached line, return the full hazard
            if self.multiline_remaining == 0 {
                self.state = ClassifierState::Running;
                let text = format!(
                    "{}\n{}",
                    self.current_hazard_text,
                    self.current_hazard_attached.join("\n")
                );
                let result = Classification::Hazard {
                    severity: self.current_hazard_severity,
                    text,
                    captures: std::mem::take(&mut self.current_hazard_captures),
                };
                self.current_hazard_text.clear();
                self.current_hazard_attached.clear();
                return result;
            }

            // Still consuming — return Noise(Strip) to suppress intermediate lines
            return Classification::Noise {
                action: NoiseAction::Strip,
                text: String::new(),
            };
        }

        // Tier 1: Grammar rules (if grammar loaded)
        if self.has_grammar {
            if let Some(c) = self.classify_tier1(clean) {
                self.previous_line = Some(clean.clone());
                return c;
            }
        }

        // Tier 2: Universal patterns
        if let Some(c) = self.classify_tier2(clean, &meta) {
            self.previous_line = Some(clean.clone());
            return c;
        }

        // Tier 3: Structural heuristics
        if let Some(c) = self.classify_tier3(clean) {
            self.previous_line = Some(clean.clone());
            return c;
        }

        self.previous_line = Some(clean.clone());
        Classification::Unknown {
            text: clean.clone(),
        }
    }

    /// Tier 1: Grammar rule evaluation — hazard → outcome → noise.
    fn classify_tier1(&self, clean: &str) -> Option<Classification> {
        // 1a. Hazard rules (never suppress errors)
        for rule in &self.hazard_rules {
            if rule.pattern.is_match(clean) {
                let captures = extract_captures(&rule.pattern, clean, &rule.captures);
                let severity = rule.severity.unwrap_or(Severity::Error);

                // Check for multiline — handled by caller after we return
                // We can't set multiline_remaining here since &self is immutable
                // Instead, return the classification and let classify_complete handle it
                return Some(Classification::Hazard {
                    severity,
                    text: clean.to_string(),
                    captures,
                });
            }
        }

        // 1b. Outcome rules
        for rule in &self.outcome_rules {
            if rule.pattern.is_match(clean) {
                let captures = extract_captures(&rule.pattern, clean, &rule.captures);
                return Some(Classification::Outcome {
                    text: clean.to_string(),
                    captures,
                });
            }
        }

        // 1c. Noise rules — action-specific first, then global
        for rule in &self.action_noise_rules {
            if rule.pattern.is_match(clean) {
                let noise_action = match rule.action {
                    RuleAction::Strip => NoiseAction::Strip,
                    RuleAction::Dedup => NoiseAction::Dedup,
                    _ => NoiseAction::Strip,
                };
                return Some(Classification::Noise {
                    action: noise_action,
                    text: clean.to_string(),
                });
            }
        }

        for rule in &self.global_noise_rules {
            if rule.pattern.is_match(clean) {
                let noise_action = match rule.action {
                    RuleAction::Strip => NoiseAction::Strip,
                    RuleAction::Dedup => NoiseAction::Dedup,
                    _ => NoiseAction::Strip,
                };
                return Some(Classification::Noise {
                    action: noise_action,
                    text: clean.to_string(),
                });
            }
        }

        None
    }

    /// Tier 2: Universal pattern matching.
    fn classify_tier2(&self, clean: &str, meta: &AnsiMetadata) -> Option<Classification> {
        // 2a. ANSI color + content classification
        if let Some(c) = self.classify_with_color(clean, meta) {
            return Some(c);
        }

        // 2b. Error keywords (line-start anchored — strongest)
        for pat in &self.universal.error_start {
            if pat.is_match(clean) {
                return Some(Classification::Hazard {
                    severity: Severity::Error,
                    text: clean.to_string(),
                    captures: HashMap::new(),
                });
            }
        }

        // Error keywords (anywhere — weaker)
        for pat in &self.universal.error_anywhere {
            if pat.is_match(clean) {
                return Some(Classification::Hazard {
                    severity: Severity::Error,
                    text: clean.to_string(),
                    captures: HashMap::new(),
                });
            }
        }

        // 2c. Warning keywords
        for pat in &self.universal.warning_start {
            if pat.is_match(clean) {
                return Some(Classification::Hazard {
                    severity: Severity::Warning,
                    text: clean.to_string(),
                    captures: HashMap::new(),
                });
            }
        }

        for pat in &self.universal.warning_anywhere {
            if pat.is_match(clean) {
                return Some(Classification::Hazard {
                    severity: Severity::Warning,
                    text: clean.to_string(),
                    captures: HashMap::new(),
                });
            }
        }

        // 2d. Stack trace detection
        for pat in &self.universal.stack_start {
            if pat.is_match(clean) {
                return Some(Classification::Hazard {
                    severity: Severity::Error,
                    text: clean.to_string(),
                    captures: HashMap::new(),
                });
            }
        }

        None
    }

    /// Classify using ANSI color + content keywords.
    fn classify_with_color(&self, clean: &str, meta: &AnsiMetadata) -> Option<Classification> {
        let has_red = meta.colors.contains(&AnsiColor::Red)
            || meta.colors.contains(&AnsiColor::BrightRed);
        let has_yellow = meta.colors.contains(&AnsiColor::Yellow)
            || meta.colors.contains(&AnsiColor::BrightYellow);
        let has_green = meta.colors.contains(&AnsiColor::Green)
            || meta.colors.contains(&AnsiColor::BrightGreen);

        // Red + error-like content → Hazard(Error)
        if has_red && self.has_error_keywords(clean) {
            return Some(Classification::Hazard {
                severity: Severity::Error,
                text: clean.to_string(),
                captures: HashMap::new(),
            });
        }

        // Yellow + warning-like content → Hazard(Warning)
        if has_yellow && self.has_warning_keywords(clean) {
            return Some(Classification::Hazard {
                severity: Severity::Warning,
                text: clean.to_string(),
                captures: HashMap::new(),
            });
        }

        // Green + success-like content → Outcome
        if has_green && self.has_success_keywords(clean) {
            return Some(Classification::Outcome {
                text: clean.to_string(),
                captures: HashMap::new(),
            });
        }

        None
    }

    fn has_error_keywords(&self, text: &str) -> bool {
        self.universal
            .error_start
            .iter()
            .chain(self.universal.error_anywhere.iter())
            .any(|pat| pat.is_match(text))
    }

    fn has_warning_keywords(&self, text: &str) -> bool {
        self.universal
            .warning_start
            .iter()
            .chain(self.universal.warning_anywhere.iter())
            .any(|pat| pat.is_match(text))
    }

    fn has_success_keywords(&self, text: &str) -> bool {
        self.universal
            .success_keywords
            .iter()
            .any(|pat| pat.is_match(text))
    }

    /// Tier 3: Structural heuristic matching.
    fn classify_tier3(&self, clean: &str) -> Option<Classification> {
        // 3a. Decorative lines
        if self.universal.decorative.is_match(clean)
            || self.universal.box_drawing.is_match(clean)
            || self.universal.whitespace_only.is_match(clean)
        {
            return Some(Classification::Noise {
                action: NoiseAction::Strip,
                text: clean.to_string(),
            });
        }

        None
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn extract_captures(
    pattern: &Regex,
    text: &str,
    capture_names: &[String],
) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(caps) = pattern.captures(text) {
        for name in capture_names {
            if let Some(m) = caps.name(name) {
                map.insert(name.clone(), m.as_str().to_string());
            }
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::grammar::{
        load_grammar_from_str, Severity,
    };
    use crate::core::line_buffer::Line;

    // -----------------------------------------------------------------------
    // Line type handling
    // -----------------------------------------------------------------------

    // Test 1: Overwrite line → Noise(Strip)
    #[test]
    fn test_overwrite_line_is_noise_strip() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Overwrite("downloading... 50%".into()));
        assert!(matches!(
            result,
            Classification::Noise {
                action: NoiseAction::Strip,
                ..
            }
        ));
    }

    // Test 2: Complete line goes through classification
    #[test]
    fn test_complete_line_classified() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("hello world".into()));
        // No grammar, no universal match → Unknown
        assert!(matches!(result, Classification::Unknown { .. }));
        assert_eq!(result.text().unwrap(), "hello world");
    }

    // -----------------------------------------------------------------------
    // Tier 2: Universal patterns — Error keywords
    // -----------------------------------------------------------------------

    // Test 3: "error:" at line start
    #[test]
    fn test_error_keyword_error_colon() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("error: something broke".into()));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }

    // Test 4: "ERROR:" at line start
    #[test]
    fn test_error_keyword_error_upper() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("ERROR: fatal crash".into()));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }

    // Test 5: "FAIL:" at line start
    #[test]
    fn test_error_keyword_fail() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("FAIL: test_something".into()));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }

    // Test 6: "FATAL:" at line start
    #[test]
    fn test_error_keyword_fatal() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("FATAL: database unreachable".into()));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }

    // Test 7: "panic:" at line start
    #[test]
    fn test_error_keyword_panic() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("panic: runtime error".into()));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }

    // Test 8: "command not found" anywhere
    #[test]
    fn test_error_keyword_command_not_found() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("zsh: foobar: command not found".into()));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }

    // Test 9: "permission denied" anywhere
    #[test]
    fn test_error_keyword_permission_denied() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("cp: /root/file: permission denied".into()));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }

    // Test 10: "No such file or directory" anywhere
    #[test]
    fn test_error_keyword_no_such_file() {
        let mut c = Classifier::new(None, None);
        let result =
            c.classify(Line::Complete("cat: /tmp/missing: No such file or directory".into()));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }

    // Test 11: "Traceback" at line start (Python)
    #[test]
    fn test_error_keyword_traceback() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete(
            "Traceback (most recent call last):".into(),
        ));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }

    // -----------------------------------------------------------------------
    // Tier 2: Universal patterns — Warning keywords
    // -----------------------------------------------------------------------

    // Test 12: "warn:" at line start
    #[test]
    fn test_warning_keyword_warn() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("warn: package outdated".into()));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Warning,
                ..
            }
        ));
    }

    // Test 13: "WARNING:" at line start
    #[test]
    fn test_warning_keyword_warning_upper() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("WARNING: deprecated API".into()));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Warning,
                ..
            }
        ));
    }

    // Test 14: "deprecated" anywhere
    #[test]
    fn test_warning_keyword_deprecated() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete(
            "npm warn deprecated: inflight@1.0.6".into(),
        ));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Warning,
                ..
            }
        ));
    }

    // Test 15: vulnerability severity
    #[test]
    fn test_warning_keyword_vulnerability() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("2 high vulnerabilities found".into()));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Warning,
                ..
            }
        ));
    }

    // -----------------------------------------------------------------------
    // Tier 2: ANSI color classification
    // -----------------------------------------------------------------------

    // Test 16: ANSI red + error content → Hazard(Error)
    #[test]
    fn test_ansi_red_error() {
        let mut c = Classifier::new(None, None);
        // \x1b[31m = red, \x1b[0m = reset
        let result = c.classify(Line::Complete("\x1b[31merror: compile failed\x1b[0m".into()));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }

    // Test 17: ANSI yellow + warning content → Hazard(Warning)
    #[test]
    fn test_ansi_yellow_warning() {
        let mut c = Classifier::new(None, None);
        let result =
            c.classify(Line::Complete("\x1b[33mwarning: unused variable\x1b[0m".into()));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Warning,
                ..
            }
        ));
    }

    // Test 18: ANSI green + success content → Outcome
    #[test]
    fn test_ansi_green_success() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("\x1b[32mBuild succeeded\x1b[0m".into()));
        assert!(matches!(result, Classification::Outcome { .. }));
    }

    // Test 19: ANSI red without error keywords → not classified by color alone
    #[test]
    fn test_ansi_red_no_keywords_no_hazard() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("\x1b[31mhello world\x1b[0m".into()));
        // Red alone doesn't classify — needs error keywords
        assert!(!matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }

    // -----------------------------------------------------------------------
    // Tier 2: Stack trace detection
    // -----------------------------------------------------------------------

    // Test 20: Node.js stack trace start
    #[test]
    fn test_stack_trace_nodejs() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete(
            "    at UserService.getUser (src/user.ts:42:5)".into(),
        ));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }

    // Test 21: Python stack trace
    #[test]
    fn test_stack_trace_python() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete(
            "  File \"/app/main.py\", line 42".into(),
        ));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }

    // Test 22: Java "Caused by:"
    #[test]
    fn test_stack_trace_java_caused_by() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete(
            "Caused by: java.lang.NullPointerException".into(),
        ));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }

    // Test 23: Generic file:line:col pattern
    #[test]
    fn test_stack_trace_generic_file_line_col() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("src/main.rs:42:5: error message".into()));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }

    // -----------------------------------------------------------------------
    // Tier 2: Prompt detection
    // -----------------------------------------------------------------------

    // Test 24: Prompt detected on Partial line ending with ?
    #[test]
    fn test_prompt_partial_question_mark() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Partial("Continue?".into()));
        assert!(matches!(result, Classification::Prompt { .. }));
        assert_eq!(c.state(), ClassifierState::AwaitingInput);
    }

    // Test 25: Prompt (y/n) on Partial line
    #[test]
    fn test_prompt_partial_yn() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Partial("Proceed (y/n)".into()));
        assert!(matches!(result, Classification::Prompt { .. }));
    }

    // Test 26: Prompt Password: on Partial line
    #[test]
    fn test_prompt_partial_password() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Partial("Password:".into()));
        assert!(matches!(result, Classification::Prompt { .. }));
    }

    // Test 27: "?" on Complete line is NOT a prompt
    #[test]
    fn test_question_on_complete_not_prompt() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("Are you sure?".into()));
        // Complete lines don't trigger prompt detection
        assert!(!matches!(result, Classification::Prompt { .. }));
    }

    // -----------------------------------------------------------------------
    // Tier 1: Grammar rules
    // -----------------------------------------------------------------------

    fn make_npm_grammar() -> Grammar {
        let toml_str = r#"
[tool]
name = "npm"

[[global_noise]]
pattern = '^npm (timing|http|sill|verb)'
action = "strip"

[actions.install]
detect = ["install", "i", "add", "ci"]

[[actions.install.hazard]]
pattern = 'ERESOLVE'
severity = "error"
action = "keep"

[[actions.install.outcome]]
pattern = '^added (?P<count>\d+) packages? in (?P<time>.+)'
action = "promote"
captures = ["count", "time"]

[[actions.install.noise]]
pattern = '^(idealTree|reify|resolv)'
action = "strip"

[actions.install.summary]
success = "+ {count} packages installed ({time})"
failure = "! npm install failed (exit {exit_code})"
"#;
        load_grammar_from_str(toml_str).unwrap()
    }

    // Test 28: Grammar hazard rule matches
    #[test]
    fn test_tier1_hazard_rule() {
        let grammar = make_npm_grammar();
        let action = grammar.actions.get("install").unwrap();
        let mut c = Classifier::new(Some(&grammar), Some(action));
        let result = c.classify(Line::Complete("npm ERR! ERESOLVE unable to resolve".into()));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }

    // Test 29: Grammar outcome rule matches with captures
    #[test]
    fn test_tier1_outcome_rule_with_captures() {
        let grammar = make_npm_grammar();
        let action = grammar.actions.get("install").unwrap();
        let mut c = Classifier::new(Some(&grammar), Some(action));
        let result = c.classify(Line::Complete("added 147 packages in 12.3s".into()));
        match result {
            Classification::Outcome { captures, .. } => {
                assert_eq!(captures.get("count").unwrap(), "147");
                assert_eq!(captures.get("time").unwrap(), "12.3s");
            }
            other => panic!("expected Outcome, got: {:?}", other),
        }
    }

    // Test 30: Grammar action noise rule matches
    #[test]
    fn test_tier1_action_noise_strip() {
        let grammar = make_npm_grammar();
        let action = grammar.actions.get("install").unwrap();
        let mut c = Classifier::new(Some(&grammar), Some(action));
        let result = c.classify(Line::Complete("idealTree: building".into()));
        assert!(matches!(
            result,
            Classification::Noise {
                action: NoiseAction::Strip,
                ..
            }
        ));
    }

    // Test 31: Grammar global noise rule matches
    #[test]
    fn test_tier1_global_noise_rule() {
        let grammar = make_npm_grammar();
        let action = grammar.actions.get("install").unwrap();
        let mut c = Classifier::new(Some(&grammar), Some(action));
        let result = c.classify(Line::Complete("npm timing setup Completed in 2ms".into()));
        assert!(matches!(
            result,
            Classification::Noise {
                action: NoiseAction::Strip,
                ..
            }
        ));
    }

    // Test 32: Hazard rules evaluated before noise (hazard wins)
    #[test]
    fn test_tier1_hazard_before_noise() {
        // Create grammar where both hazard and noise could match
        let toml_str = r#"
[tool]
name = "test"

[actions.build]
detect = ["build"]

[[actions.build.hazard]]
pattern = 'critical error'
severity = "error"
action = "keep"

[[actions.build.noise]]
pattern = 'critical'
action = "strip"

[actions.build.summary]
success = "ok"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let action = grammar.actions.get("build").unwrap();
        let mut c = Classifier::new(Some(&grammar), Some(action));
        let result = c.classify(Line::Complete("critical error in module".into()));
        // Hazard should win because it's evaluated first
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }

    // Test 33: No grammar match falls through to Tier 2
    #[test]
    fn test_tier1_no_match_falls_to_tier2() {
        let grammar = make_npm_grammar();
        let action = grammar.actions.get("install").unwrap();
        let mut c = Classifier::new(Some(&grammar), Some(action));
        // This doesn't match any grammar rule, but matches Tier 2 error pattern
        let result = c.classify(Line::Complete("error: system failure".into()));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }

    // -----------------------------------------------------------------------
    // Tier 3: Structural heuristics
    // -----------------------------------------------------------------------

    // Test 34: Decorative line (=====) → Noise(Strip)
    #[test]
    fn test_tier3_decorative_equals() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("================".into()));
        assert!(matches!(
            result,
            Classification::Noise {
                action: NoiseAction::Strip,
                ..
            }
        ));
    }

    // Test 35: Decorative line (-----) → Noise(Strip)
    #[test]
    fn test_tier3_decorative_dashes() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("----------------".into()));
        assert!(matches!(
            result,
            Classification::Noise {
                action: NoiseAction::Strip,
                ..
            }
        ));
    }

    // Test 36: Empty/whitespace line → Noise(Strip)
    #[test]
    fn test_tier3_whitespace_only() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("   ".into()));
        assert!(matches!(
            result,
            Classification::Noise {
                action: NoiseAction::Strip,
                ..
            }
        ));
    }

    // Test 37: Empty line → Noise(Strip)
    #[test]
    fn test_tier3_empty_line() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("".into()));
        assert!(matches!(
            result,
            Classification::Noise {
                action: NoiseAction::Strip,
                ..
            }
        ));
    }

    // -----------------------------------------------------------------------
    // State machine
    // -----------------------------------------------------------------------

    // Test 38: Initial state is Idle
    #[test]
    fn test_state_initial_idle() {
        let c = Classifier::new(None, None);
        assert_eq!(c.state(), ClassifierState::Idle);
    }

    // Test 39: First classify transitions to Running
    #[test]
    fn test_state_idle_to_running() {
        let mut c = Classifier::new(None, None);
        c.classify(Line::Complete("hello".into()));
        assert_eq!(c.state(), ClassifierState::Running);
    }

    // Test 40: Prompt detection transitions to AwaitingInput
    #[test]
    fn test_state_to_awaiting_input() {
        let mut c = Classifier::new(None, None);
        c.classify(Line::Partial("Password:".into()));
        assert_eq!(c.state(), ClassifierState::AwaitingInput);
    }

    // Test 41: resume_from_prompt transitions AwaitingInput → Running
    #[test]
    fn test_state_resume_from_prompt() {
        let mut c = Classifier::new(None, None);
        c.classify(Line::Partial("Password:".into()));
        assert_eq!(c.state(), ClassifierState::AwaitingInput);
        c.resume_from_prompt();
        assert_eq!(c.state(), ClassifierState::Running);
    }

    // Test 42: notify_exit with exit code 0 → DoneSuccess
    #[test]
    fn test_state_done_success() {
        let mut c = Classifier::new(None, None);
        c.classify(Line::Complete("hello".into()));
        c.notify_exit(0, false);
        assert_eq!(c.state(), ClassifierState::DoneSuccess);
    }

    // Test 43: notify_exit with exit code != 0 → DoneFailure
    #[test]
    fn test_state_done_failure() {
        let mut c = Classifier::new(None, None);
        c.classify(Line::Complete("hello".into()));
        c.notify_exit(1, false);
        assert_eq!(c.state(), ClassifierState::DoneFailure);
    }

    // Test 44: notify_exit with signal → DoneKilled
    #[test]
    fn test_state_done_killed() {
        let mut c = Classifier::new(None, None);
        c.classify(Line::Complete("hello".into()));
        c.notify_exit(0, true);
        assert_eq!(c.state(), ClassifierState::DoneKilled);
    }

    // -----------------------------------------------------------------------
    // Classification::text() accessor
    // -----------------------------------------------------------------------

    // Test 45: text() returns content for all variants
    #[test]
    fn test_classification_text_accessor() {
        let h = Classification::Hazard {
            severity: Severity::Error,
            text: "err".into(),
            captures: HashMap::new(),
        };
        assert_eq!(h.text(), Some("err"));

        let o = Classification::Outcome {
            text: "ok".into(),
            captures: HashMap::new(),
        };
        assert_eq!(o.text(), Some("ok"));

        let n = Classification::Noise {
            action: NoiseAction::Strip,
            text: "noise".into(),
        };
        assert_eq!(n.text(), Some("noise"));

        let p = Classification::Prompt {
            text: "prompt".into(),
        };
        assert_eq!(p.text(), Some("prompt"));

        let u = Classification::Unknown {
            text: "unknown".into(),
        };
        assert_eq!(u.text(), Some("unknown"));
    }

    // -----------------------------------------------------------------------
    // Mixed scenario tests
    // -----------------------------------------------------------------------

    // Test 46: Normal output without patterns → Unknown
    #[test]
    fn test_normal_output_unknown() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("Compiling serde v1.0.195".into()));
        assert!(matches!(result, Classification::Unknown { .. }));
    }

    // Test 47: Grammar noise dedup action
    #[test]
    fn test_tier1_noise_dedup() {
        let toml_str = r#"
[tool]
name = "cargo"

[actions.build]
detect = ["build"]

[[actions.build.noise]]
pattern = '^Compiling'
action = "dedup"

[actions.build.summary]
success = "ok"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let action = grammar.actions.get("build").unwrap();
        let mut c = Classifier::new(Some(&grammar), Some(action));
        let result = c.classify(Line::Complete("Compiling serde v1.0.195".into()));
        assert!(matches!(
            result,
            Classification::Noise {
                action: NoiseAction::Dedup,
                ..
            }
        ));
    }

    // Test 48: SyntaxError anywhere in line
    #[test]
    fn test_error_keyword_syntax_error() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete(
            "  SyntaxError: Unexpected token '}'".into(),
        ));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }

    // Test 49: Go goroutine stack trace
    #[test]
    fn test_stack_trace_go_goroutine() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("goroutine 1 [running]:".into()));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }
}
