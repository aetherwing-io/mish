/// Three-tier classification engine.
///
/// Assigns a classification to each line: Hazard, Outcome, Noise, Prompt, or Unknown.
/// Evaluates in order: Tier 1 (grammar rules) → Tier 2 (universal patterns) → Tier 3 (structural).

use std::collections::HashMap;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use regex::Regex;

use crate::core::grammar::{Action, Grammar, Rule, RuleAction, Severity};
use crate::core::line_buffer::Line;
use crate::squasher::dedup::{DedupResult, ImplicitDedup};
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

/// Constants for structural heuristics.
/// Number of consecutive Unknown lines before volume compression kicks in.
pub const VOLUME_THRESHOLD: usize = 10;
/// Duration of silence after which next line gets noise immunity.
pub const SILENCE_THRESHOLD: Duration = Duration::from_secs(2);
/// Number of lines to keep in the ring buffer for unknown tool fallback.
pub const RING_BUFFER_SIZE: usize = 5;

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
    implicit_dedup: ImplicitDedup,
    previous_line: Option<String>,
    /// Count of consecutive Unknown lines (for volume compression)
    consecutive_unknown_count: usize,
    /// Ring buffer of last N lines (for unknown tool fallback)
    ring_buffer: VecDeque<String>,
    /// Whether an Outcome has been seen (for ring buffer summary)
    has_outcome: bool,

    // Multiline hazard state
    multiline_remaining: u32,
    current_hazard_severity: Severity,
    current_hazard_text: String,
    current_hazard_captures: HashMap<String, String>,
    current_hazard_attached: Vec<String>,

    // Stack trace consuming state
    consuming_trace_lines: Vec<String>,
    /// Deferred line from stack trace compression — caller must drain via drain_deferred()
    deferred_line: Option<Line>,

    // Timing
    last_line_time: Instant,
    /// Whether the next line should bypass structural noise (temporal boost)
    temporal_noise_bypass: bool,
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
            implicit_dedup: ImplicitDedup::new(),
            previous_line: None,
            consecutive_unknown_count: 0,
            ring_buffer: VecDeque::with_capacity(RING_BUFFER_SIZE + 1),
            has_outcome: false,
            multiline_remaining: 0,
            current_hazard_severity: Severity::Error,
            current_hazard_text: String::new(),
            current_hazard_captures: HashMap::new(),
            current_hazard_attached: Vec::new(),
            consuming_trace_lines: Vec::new(),
            deferred_line: None,
            last_line_time: Instant::now(),
            temporal_noise_bypass: false,
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

    /// Notify the classifier of silence (no output for a duration).
    /// If duration exceeds SILENCE_THRESHOLD, next line gets noise immunity.
    pub fn notify_silence(&mut self, duration: Duration) {
        if duration >= SILENCE_THRESHOLD {
            self.temporal_noise_bypass = true;
        }
        // If in Running state and silence > 500ms, transition to MaybePrompt
        if self.state == ClassifierState::Running && duration >= Duration::from_millis(500) {
            self.state = ClassifierState::MaybePrompt;
        }
    }

    /// Get the ring buffer of last lines (for unknown tool fallback on exit).
    pub fn ring_buffer(&self) -> &VecDeque<String> {
        &self.ring_buffer
    }

    /// Whether we've seen any Outcome classifications.
    pub fn has_outcome(&self) -> bool {
        self.has_outcome
    }

    /// Get count of consecutive Unknown lines (for volume compression).
    pub fn consecutive_unknown_count(&self) -> usize {
        self.consecutive_unknown_count
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

    /// Drain a deferred line produced by stack trace compression.
    ///
    /// At trace end, the compressed summary is returned from `classify()` and
    /// the terminating line is deferred. Call this after each `classify()` to
    /// retrieve it. Returns `None` if no line is deferred.
    pub fn drain_deferred(&mut self) -> Option<Classification> {
        let deferred = self.deferred_line.take()?;
        Some(self.classify(deferred))
    }

    /// Classify a line from the line buffer.
    pub fn classify(&mut self, line: Line) -> Classification {
        // Transition from Idle to Running on first input
        if self.state == ClassifierState::Idle {
            self.state = ClassifierState::Running;
        }

        // If we were in MaybePrompt and output resumes, go back to Running
        if self.state == ClassifierState::MaybePrompt {
            self.state = ClassifierState::Running;
        }

        // Check temporal: if time since last line exceeds silence threshold,
        // give this line noise immunity
        let elapsed = self.last_line_time.elapsed();
        if elapsed >= SILENCE_THRESHOLD {
            self.temporal_noise_bypass = true;
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

        // If consuming a multiline hazard, attach this line unconditionally
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

        // If consuming a stack trace, check if this line continues it
        if self.state == ClassifierState::ConsumingTrace {
            if self.is_stack_continuation(clean) {
                self.consuming_trace_lines.push(clean.clone());
                // Suppress continuation lines — they'll be emitted compressed
                return Classification::Noise {
                    action: NoiseAction::Strip,
                    text: String::new(),
                };
            } else {
                // End of stack trace — transition back to Running
                self.state = ClassifierState::Running;

                if let Some(compressed) = self.compress_trace() {
                    // Defer the terminating line for caller to drain
                    self.deferred_line = Some(Line::Complete(raw.to_string()));
                    self.consuming_trace_lines.clear();
                    return Classification::Hazard {
                        severity: Severity::Error,
                        text: compressed,
                        captures: HashMap::new(),
                    };
                }

                self.consuming_trace_lines.clear();
                // Fall through to classify this line normally
            }
        }

        // Save temporal bypass state before classification consumes it
        let has_temporal_bypass = self.temporal_noise_bypass;
        self.temporal_noise_bypass = false;

        // Tier 1: Grammar rules (if grammar loaded)
        if self.has_grammar {
            if let Some(c) = self.classify_tier1_mut(clean) {
                self.update_tracking(clean, &c);
                return c;
            }
        }

        // Tier 2: Universal patterns
        if let Some(c) = self.classify_tier2(clean, &meta) {
            // Check if this is a stack trace start — enter CONSUMING_TRACE
            if matches!(c, Classification::Hazard { .. }) && self.is_stack_trace_start(clean) {
                self.state = ClassifierState::ConsumingTrace;
                self.consuming_trace_lines.clear();
                self.consuming_trace_lines.push(clean.clone());
            }
            self.update_tracking(clean, &c);
            return c;
        }

        // Tier 3: Structural heuristics
        // If temporal bypass is active, skip structural noise classification
        if !has_temporal_bypass {
            if let Some(c) = self.classify_tier3(clean) {
                self.update_tracking(clean, &c);
                return c;
            }

            // Tier 3b: Implicit dedup — consecutive similar lines get Noise(Dedup)
            match self.implicit_dedup.check(clean) {
                DedupResult::Absorbed => {
                    let result = Classification::Noise {
                        action: NoiseAction::Dedup,
                        text: clean.clone(),
                    };
                    self.update_tracking(clean, &result);
                    return result;
                }
                DedupResult::FlushStreak { .. } => {
                    // Streak broken — current line classified as-is below
                }
                DedupResult::NotSimilar => {
                    // Not similar — fall through to Unknown
                }
            }
        }

        // No match — classify as Unknown
        let result = Classification::Unknown {
            text: clean.clone(),
        };
        self.update_tracking(clean, &result);
        result
    }

    /// Update tracking state after classification: ring buffer, volume compression, previous line.
    fn update_tracking(&mut self, clean: &str, classification: &Classification) {
        // Ring buffer: always push, maintaining max size
        self.ring_buffer.push_back(clean.to_string());
        if self.ring_buffer.len() > RING_BUFFER_SIZE {
            self.ring_buffer.pop_front();
        }

        // Track outcomes for ring buffer summary decision
        if matches!(classification, Classification::Outcome { .. }) {
            self.has_outcome = true;
        }

        // Volume compression: track consecutive Unknown count
        if matches!(classification, Classification::Unknown { .. }) {
            self.consecutive_unknown_count += 1;
        } else {
            self.consecutive_unknown_count = 0;
        }

        self.previous_line = Some(clean.to_string());
    }

    /// Tier 1: Grammar rule evaluation — hazard → outcome → noise.
    /// Uses &mut self to support multiline hazard state transitions.
    fn classify_tier1_mut(&mut self, clean: &str) -> Option<Classification> {
        // 1a. Hazard rules (never suppress errors)
        for rule in &self.hazard_rules {
            if rule.pattern.is_match(clean) {
                let captures = extract_captures(&rule.pattern, clean, &rule.captures);
                let severity = rule.severity.unwrap_or(Severity::Error);

                // Check for multiline attachment
                if let Some(n) = rule.multiline {
                    if n > 0 {
                        self.multiline_remaining = n;
                        self.current_hazard_severity = severity;
                        self.current_hazard_text = clean.to_string();
                        self.current_hazard_captures = captures;
                        self.current_hazard_attached.clear();
                        self.state = ClassifierState::ConsumingTrace;
                        // Suppress the initial line — full hazard emitted after all N lines collected
                        return Some(Classification::Noise {
                            action: NoiseAction::Strip,
                            text: String::new(),
                        });
                    }
                }

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

        // 3b. Edit-distance similarity
        if let Some(ref prev) = self.previous_line {
            if is_similar(clean, prev) {
                return Some(Classification::Noise {
                    action: NoiseAction::Dedup,
                    text: clean.to_string(),
                });
            }
        }

        None
    }

    /// Compress collected stack trace frames into a summary.
    ///
    /// Returns `None` if too few frames to compress (≤1).
    /// For 2 frames: returns just the last frame.
    /// For 3+ frames: returns `... N frames ...\n{last_frame}`.
    fn compress_trace(&self) -> Option<String> {
        let total = self.consuming_trace_lines.len();
        if total < 2 {
            return None;
        }
        let last_frame = &self.consuming_trace_lines[total - 1];
        if total == 2 {
            Some(last_frame.clone())
        } else {
            let middle_count = total - 2;
            let word = if middle_count == 1 { "frame" } else { "frames" };
            Some(format!("    ... {} {} ...\n{}", middle_count, word, last_frame))
        }
    }

    /// Check whether a line is a stack trace start (for entering CONSUMING_TRACE).
    fn is_stack_trace_start(&self, clean: &str) -> bool {
        for pat in &self.universal.stack_start {
            if pat.is_match(clean) {
                return true;
            }
        }
        false
    }

    /// Check whether a line continues an active stack trace.
    fn is_stack_continuation(&self, clean: &str) -> bool {
        // Indented lines continue a stack trace
        if self.universal.stack_continue.is_match(clean) {
            return true;
        }
        // Stack-start patterns also continue (e.g., more "at" frames)
        for pat in &self.universal.stack_start {
            if pat.is_match(clean) {
                return true;
            }
        }
        false
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

/// Compute the Levenshtein edit distance between two strings.
fn levenshtein(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let m = a_chars.len();
    let n = b_chars.len();

    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }

    // Use two-row optimization
    let mut prev = vec![0usize; n + 1];
    let mut curr = vec![0usize; n + 1];

    for j in 0..=n {
        prev[j] = j;
    }

    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a_chars[i - 1] == b_chars[j - 1] {
                0
            } else {
                1
            };
            curr[j] = (prev[j] + 1)
                .min(curr[j - 1] + 1)
                .min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[n]
}

/// Check if two lines are similar enough to dedup (normalized edit distance < 0.3).
///
/// Optimization: only computes edit distance when lines are similar length
/// (within 2x of each other) and start with the same first token.
fn is_similar(current: &str, previous: &str) -> bool {
    let cur_len = current.len();
    let prev_len = previous.len();

    // Both empty → similar
    if cur_len == 0 && prev_len == 0 {
        return true;
    }
    // One empty, one not → not similar
    if cur_len == 0 || prev_len == 0 {
        return false;
    }

    // Length check: within 2x of each other
    let max_len = cur_len.max(prev_len);
    let min_len = cur_len.min(prev_len);
    if max_len > min_len * 2 {
        return false;
    }

    // First token check
    let cur_first = current.split_whitespace().next().unwrap_or("");
    let prev_first = previous.split_whitespace().next().unwrap_or("");
    if cur_first != prev_first {
        return false;
    }

    let distance = levenshtein(current, previous);
    let normalized = distance as f64 / max_len as f64;

    normalized < 0.3
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::grammar::{
        detect_tool, load_all_grammars, load_grammar_from_str, Severity,
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

    // -----------------------------------------------------------------------
    // Phase 3 Enhancement Tests
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Tier 1: Multiline hazard consumption
    // -----------------------------------------------------------------------

    // Test 50: Multiline hazard attaches next N lines unconditionally
    #[test]
    fn test_multiline_hazard_attaches_lines() {
        let toml_str = r#"
[tool]
name = "test"

[actions.build]
detect = ["build"]

[[actions.build.hazard]]
pattern = '^ERROR:'
severity = "error"
action = "keep"
multiline = 2

[actions.build.summary]
success = "ok"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let action = grammar.actions.get("build").unwrap();
        let mut c = Classifier::new(Some(&grammar), Some(action));

        // First line triggers multiline — suppressed during collection
        let r1 = c.classify(Line::Complete("ERROR: assertion failed".into()));
        assert!(matches!(r1, Classification::Noise { .. }));
        assert_eq!(c.state(), ClassifierState::ConsumingTrace);

        // Second line (attached unconditionally)
        let r2 = c.classify(Line::Complete("  expected: 42".into()));
        assert!(matches!(r2, Classification::Noise { .. }));

        // Third line (last attached) — now the full hazard is emitted
        let r3 = c.classify(Line::Complete("  actual: 0".into()));
        match r3 {
            Classification::Hazard {
                severity, text, ..
            } => {
                assert_eq!(severity, Severity::Error);
                assert!(text.contains("ERROR: assertion failed"));
                assert!(text.contains("expected: 42"));
                assert!(text.contains("actual: 0"));
            }
            other => panic!("expected Hazard, got: {:?}", other),
        }

        // State should return to Running
        assert_eq!(c.state(), ClassifierState::Running);
    }

    // Test 51: Multiline hazard content is immune to noise rules
    #[test]
    fn test_multiline_hazard_immune_to_noise() {
        let toml_str = r#"
[tool]
name = "test"

[actions.build]
detect = ["build"]

[[actions.build.hazard]]
pattern = '^FAIL:'
severity = "error"
action = "keep"
multiline = 1

[[actions.build.noise]]
pattern = '^\s+at\s'
action = "strip"

[actions.build.summary]
success = "ok"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let action = grammar.actions.get("build").unwrap();
        let mut c = Classifier::new(Some(&grammar), Some(action));

        // Hazard line
        let _r1 = c.classify(Line::Complete("FAIL: test_foo".into()));

        // This line matches a noise rule, but multiline attachment overrides
        let r2 = c.classify(Line::Complete("  at module.test (test.js:42:5)".into()));

        // Should get the full hazard (not noise stripped)
        match r2 {
            Classification::Hazard { text, .. } => {
                assert!(text.contains("FAIL: test_foo"));
                assert!(text.contains("at module.test"));
            }
            other => panic!("expected Hazard, got: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Tier 2: Stack trace CONSUMING_TRACE state
    // -----------------------------------------------------------------------

    // Test 52: Node.js stack trace enters CONSUMING_TRACE and consumes continuations
    #[test]
    fn test_stack_trace_consuming_nodejs() {
        let mut c = Classifier::new(None, None);

        // First line: stack trace start — classified as Hazard, enters CONSUMING_TRACE
        let r1 = c.classify(Line::Complete(
            "    at UserService.getUser (src/user.ts:42:5)".into(),
        ));
        assert!(matches!(
            r1,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
        assert_eq!(c.state(), ClassifierState::ConsumingTrace);

        // Continuation frame (indented) — consumed as noise
        let r2 = c.classify(Line::Complete(
            "    at Router.dispatch (node_modules/express/lib/router.js:73:3)".into(),
        ));
        assert!(matches!(
            r2,
            Classification::Noise {
                action: NoiseAction::Strip,
                ..
            }
        ));

        // Non-indented, non-stack line → ends trace, returns compressed last frame
        let r3 = c.classify(Line::Complete("Process exited with code 1".into()));
        assert_eq!(c.state(), ClassifierState::Running);
        // With 2 frames, compression emits the last frame as Hazard
        assert!(matches!(r3, Classification::Hazard { .. }));
        if let Classification::Hazard { ref text, .. } = r3 {
            assert!(text.contains("Router.dispatch"), "expected last frame in: {}", text);
        }
        // Terminating line deferred
        let deferred = c.drain_deferred();
        assert!(deferred.is_some());
        assert!(matches!(deferred.unwrap(), Classification::Unknown { .. }));
    }

    // Test 53: Python stack trace enters CONSUMING_TRACE
    #[test]
    fn test_stack_trace_consuming_python() {
        let mut c = Classifier::new(None, None);

        let r1 = c.classify(Line::Complete(
            "  File \"/app/main.py\", line 42".into(),
        ));
        assert!(matches!(
            r1,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
        assert_eq!(c.state(), ClassifierState::ConsumingTrace);

        // Code line (indented 4 spaces)
        let r2 = c.classify(Line::Complete("    result = process(data)".into()));
        assert!(matches!(r2, Classification::Noise { .. }));
        assert_eq!(c.state(), ClassifierState::ConsumingTrace);
    }

    // Test 54: Rust backtrace enters CONSUMING_TRACE
    #[test]
    fn test_stack_trace_consuming_rust() {
        let mut c = Classifier::new(None, None);

        let r1 = c.classify(Line::Complete(
            "   0: 0x7ff612345678 - std::panicking::begin_panic".into(),
        ));
        assert!(matches!(r1, Classification::Hazard { .. }));
        assert_eq!(c.state(), ClassifierState::ConsumingTrace);

        // Continuation
        let r2 = c.classify(Line::Complete(
            "   1: 0x7ff612345679 - core::result::unwrap_failed".into(),
        ));
        assert!(matches!(r2, Classification::Noise { .. }));
    }

    // -----------------------------------------------------------------------
    // Tier 3: Edit-distance similarity
    // -----------------------------------------------------------------------

    // Test 55: Similar consecutive lines → Noise(Dedup)
    #[test]
    fn test_edit_distance_similar_lines_dedup() {
        let mut c = Classifier::new(None, None);

        let _r1 = c.classify(Line::Complete("Downloading package foo v1.0.0".into()));
        let r2 = c.classify(Line::Complete("Downloading package bar v1.0.0".into()));
        assert!(matches!(
            r2,
            Classification::Noise {
                action: NoiseAction::Dedup,
                ..
            }
        ));
    }

    // Test 56: Dissimilar lines are NOT deduped
    #[test]
    fn test_edit_distance_dissimilar_not_deduped() {
        let mut c = Classifier::new(None, None);

        let _r1 = c.classify(Line::Complete("Starting build process".into()));
        let r2 = c.classify(Line::Complete("All tests passed successfully".into()));
        assert!(!matches!(
            r2,
            Classification::Noise {
                action: NoiseAction::Dedup,
                ..
            }
        ));
    }

    // Test 57: Edit distance with different first tokens → no dedup
    #[test]
    fn test_edit_distance_different_first_token_no_dedup() {
        let mut c = Classifier::new(None, None);

        let _r1 = c.classify(Line::Complete("Building module A".into()));
        let r2 = c.classify(Line::Complete("Testing module A".into()));
        // Different first token → skip edit distance
        assert!(!matches!(
            r2,
            Classification::Noise {
                action: NoiseAction::Dedup,
                ..
            }
        ));
    }

    // -----------------------------------------------------------------------
    // Tier 3: Volume compression (consecutive unknowns)
    // -----------------------------------------------------------------------

    // Test 58: Consecutive Unknown lines tracked
    #[test]
    fn test_volume_compression_consecutive_unknowns() {
        let mut c = Classifier::new(None, None);

        // Feed 12 distinct Unknown lines (different enough to avoid dedup)
        for i in 0..12 {
            c.classify(Line::Complete(format!("uniqueline_{i}_aaaa")));
        }
        assert!(c.consecutive_unknown_count() >= VOLUME_THRESHOLD);
    }

    // Test 59: Hazard resets consecutive Unknown count
    #[test]
    fn test_volume_compression_reset_by_hazard() {
        let mut c = Classifier::new(None, None);

        // Feed some unknowns
        for i in 0..5 {
            c.classify(Line::Complete(format!("uniqueline_{i}_bbbb")));
        }
        assert_eq!(c.consecutive_unknown_count(), 5);

        // Now a hazard
        c.classify(Line::Complete("error: something broke".into()));
        assert_eq!(c.consecutive_unknown_count(), 0);

        // Resume unknowns
        c.classify(Line::Complete("uniqueline_after_hazard".into()));
        assert_eq!(c.consecutive_unknown_count(), 1);
    }

    // -----------------------------------------------------------------------
    // Tier 3: Temporal heuristics
    // -----------------------------------------------------------------------

    // Test 60: Lines after silence bypass structural noise
    #[test]
    fn test_temporal_silence_bypass() {
        let mut c = Classifier::new(None, None);

        // First line sets up a previous line for potential dedup
        c.classify(Line::Complete("Downloading package foo v1.0.0".into()));

        // Notify silence exceeding threshold
        c.notify_silence(Duration::from_secs(3));

        // This line would normally dedup with the previous, but silence gives immunity
        let r2 = c.classify(Line::Complete("Downloading package bar v1.0.0".into()));
        // Should NOT be Noise(Dedup) because temporal boost is active
        assert!(!matches!(
            r2,
            Classification::Noise {
                action: NoiseAction::Dedup,
                ..
            }
        ));
    }

    // Test 61: Silence transitions to MaybePrompt
    #[test]
    fn test_silence_to_maybe_prompt() {
        let mut c = Classifier::new(None, None);
        c.classify(Line::Complete("hello".into()));
        assert_eq!(c.state(), ClassifierState::Running);

        c.notify_silence(Duration::from_millis(600));
        assert_eq!(c.state(), ClassifierState::MaybePrompt);
    }

    // Test 62: Output resumes from MaybePrompt → Running
    #[test]
    fn test_maybe_prompt_to_running() {
        let mut c = Classifier::new(None, None);
        c.classify(Line::Complete("hello".into()));
        c.notify_silence(Duration::from_millis(600));
        assert_eq!(c.state(), ClassifierState::MaybePrompt);

        c.classify(Line::Complete("more output".into()));
        assert_eq!(c.state(), ClassifierState::Running);
    }

    // -----------------------------------------------------------------------
    // Tier 3: Ring buffer
    // -----------------------------------------------------------------------

    // Test 63: Ring buffer tracks last 5 lines
    #[test]
    fn test_ring_buffer_last_5() {
        let mut c = Classifier::new(None, None);

        for i in 0..8 {
            c.classify(Line::Complete(format!("uniqueline_{i}_cccc")));
        }

        let buf = c.ring_buffer();
        assert_eq!(buf.len(), RING_BUFFER_SIZE);
        // Should contain lines 3..8 (the last 5)
        assert!(buf[0].contains("uniqueline_3_cccc"));
        assert!(buf[4].contains("uniqueline_7_cccc"));
    }

    // Test 64: Ring buffer available on exit with no outcome
    #[test]
    fn test_ring_buffer_no_outcome() {
        let mut c = Classifier::new(None, None);

        c.classify(Line::Complete("line_a_uniqueX".into()));
        c.classify(Line::Complete("line_b_uniqueX".into()));
        c.classify(Line::Complete("line_c_uniqueX".into()));

        assert!(!c.has_outcome());
        let buf = c.ring_buffer();
        assert_eq!(buf.len(), 3);
    }

    // Test 65: Outcome tracked for ring buffer decision
    #[test]
    fn test_ring_buffer_has_outcome() {
        let mut c = Classifier::new(None, None);

        c.classify(Line::Complete("uniqueline_d_xyz".into()));
        assert!(!c.has_outcome());

        // Trigger an Outcome via green ANSI + success keyword
        c.classify(Line::Complete("\x1b[32mBuild succeeded\x1b[0m".into()));
        assert!(c.has_outcome());
    }

    // -----------------------------------------------------------------------
    // Hazards NEVER suppressed by noise (cross-tier invariant)
    // -----------------------------------------------------------------------

    // Test 66: Grammar hazard rules always evaluated before noise rules
    #[test]
    fn test_hazard_never_suppressed_by_grammar_noise() {
        // The key invariant: if a grammar DEFINES a hazard rule, it runs before noise.
        // Even when both hazard and noise patterns could match, hazard wins.
        let toml_str2 = r#"
[tool]
name = "test2"

[actions.build]
detect = ["build"]

[[actions.build.hazard]]
pattern = 'FATAL'
severity = "error"
action = "keep"

[[actions.build.noise]]
pattern = 'FATAL.*noise'
action = "strip"

[actions.build.summary]
success = "ok"
"#;
        let grammar2 = load_grammar_from_str(toml_str2).unwrap();
        let action2 = grammar2.actions.get("build").unwrap();
        let mut c = Classifier::new(Some(&grammar2), Some(action2));

        // Both hazard and noise could match — hazard wins
        let result = c.classify(Line::Complete("FATAL crash noise here".into()));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }

    // Test 67: Decorative noise cannot suppress error keyword
    #[test]
    fn test_tier2_error_not_suppressed_by_tier3() {
        let mut c = Classifier::new(None, None);

        // "error:" at start — Tier 2 should catch this before Tier 3
        let result = c.classify(Line::Complete("error: compilation failed".into()));
        assert!(matches!(
            result,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));
    }

    // -----------------------------------------------------------------------
    // Prompt detection requires Partial line type
    // -----------------------------------------------------------------------

    // Test 68: REPL prompt on Partial line
    #[test]
    fn test_prompt_repl_partial() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Partial("node> ".into()));
        assert!(matches!(result, Classification::Prompt { .. }));
        assert_eq!(c.state(), ClassifierState::AwaitingInput);
    }

    // Test 69: "Enter" prompt on Partial line
    #[test]
    fn test_prompt_enter_partial() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Partial("Enter your name: ".into()));
        assert!(matches!(result, Classification::Prompt { .. }));
    }

    // Test 70: "Press any key" on Partial line
    #[test]
    fn test_prompt_press_any_key_partial() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Partial("Press any key to continue".into()));
        assert!(matches!(result, Classification::Prompt { .. }));
    }

    // Test 71: REPL prompt on Complete line → NOT a prompt
    #[test]
    fn test_prompt_repl_complete_not_prompt() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("node> ".into()));
        // Complete lines don't trigger prompt detection
        assert!(!matches!(result, Classification::Prompt { .. }));
    }

    // -----------------------------------------------------------------------
    // State machine transitions (extended)
    // -----------------------------------------------------------------------

    // Test 72: ConsumingTrace → Running when trace ends
    #[test]
    fn test_state_consuming_trace_to_running() {
        let mut c = Classifier::new(None, None);

        // Enter CONSUMING_TRACE via stack trace
        c.classify(Line::Complete(
            "    at func (file.js:10:5)".into(),
        ));
        assert_eq!(c.state(), ClassifierState::ConsumingTrace);

        // Non-matching line ends the trace
        c.classify(Line::Complete("Done.".into()));
        assert_eq!(c.state(), ClassifierState::Running);
    }

    // Test 73: AwaitingInput → Running via resume_from_prompt
    #[test]
    fn test_state_awaiting_input_resume() {
        let mut c = Classifier::new(None, None);
        c.classify(Line::Partial("Password: ".into()));
        assert_eq!(c.state(), ClassifierState::AwaitingInput);

        c.resume_from_prompt();
        assert_eq!(c.state(), ClassifierState::Running);

        // Calling resume when not AwaitingInput does nothing
        c.resume_from_prompt();
        assert_eq!(c.state(), ClassifierState::Running);
    }

    // Test 74: Idle → Running → MaybePrompt → Running
    #[test]
    fn test_state_full_maybe_prompt_cycle() {
        let mut c = Classifier::new(None, None);
        assert_eq!(c.state(), ClassifierState::Idle);

        c.classify(Line::Complete("output".into()));
        assert_eq!(c.state(), ClassifierState::Running);

        c.notify_silence(Duration::from_millis(600));
        assert_eq!(c.state(), ClassifierState::MaybePrompt);

        c.classify(Line::Complete("more output".into()));
        assert_eq!(c.state(), ClassifierState::Running);
    }

    // -----------------------------------------------------------------------
    // Levenshtein distance unit tests
    // -----------------------------------------------------------------------

    // Test 75: Levenshtein identical strings
    #[test]
    fn test_levenshtein_identical() {
        assert_eq!(levenshtein("hello", "hello"), 0);
    }

    // Test 76: Levenshtein single edit
    #[test]
    fn test_levenshtein_single_edit() {
        assert_eq!(levenshtein("hello", "hallo"), 1);
        assert_eq!(levenshtein("cat", "cats"), 1);
    }

    // Test 77: Levenshtein empty strings
    #[test]
    fn test_levenshtein_empty() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("", "abc"), 3);
    }

    // Test 78: is_similar with very different lines
    #[test]
    fn test_is_similar_very_different() {
        assert!(!is_similar("hello world", "goodbye universe"));
    }

    // Test 79: is_similar with nearly identical lines
    #[test]
    fn test_is_similar_nearly_identical() {
        assert!(is_similar(
            "Compiling serde v1.0.195",
            "Compiling serde v1.0.196"
        ));
    }

    // -----------------------------------------------------------------------
    // Tier 1 evaluation order: action-specific noise → global noise → inherited
    // -----------------------------------------------------------------------

    // Test 80: Action noise rules checked before global noise
    #[test]
    fn test_tier1_action_noise_before_global() {
        let toml_str = r#"
[tool]
name = "test"

[[global_noise]]
pattern = '^progress'
action = "dedup"

[actions.build]
detect = ["build"]

[[actions.build.noise]]
pattern = '^progress'
action = "strip"

[actions.build.summary]
success = "ok"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let action = grammar.actions.get("build").unwrap();
        let mut c = Classifier::new(Some(&grammar), Some(action));

        let result = c.classify(Line::Complete("progress: 50%".into()));
        // Action-specific noise (strip) should win over global (dedup)
        assert!(matches!(
            result,
            Classification::Noise {
                action: NoiseAction::Strip,
                ..
            }
        ));
    }

    // -----------------------------------------------------------------------
    // Edge cases and integration
    // -----------------------------------------------------------------------

    // Test 81: Box-drawing characters → Noise(Strip)
    #[test]
    fn test_tier3_box_drawing() {
        let mut c = Classifier::new(None, None);
        let result = c.classify(Line::Complete("┌──────────────┐".into()));
        assert!(matches!(
            result,
            Classification::Noise {
                action: NoiseAction::Strip,
                ..
            }
        ));
    }

    // Test 82: Mixed scenario — grammar + universal + structural
    #[test]
    fn test_mixed_scenario_full_pipeline() {
        let grammar = make_npm_grammar();
        let action = grammar.actions.get("install").unwrap();
        let mut c = Classifier::new(Some(&grammar), Some(action));

        // Tier 1 noise
        let r1 = c.classify(Line::Complete("idealTree: loading".into()));
        assert!(matches!(r1, Classification::Noise { .. }));

        // Tier 1 outcome
        let r2 = c.classify(Line::Complete("added 42 packages in 2.1s".into()));
        assert!(matches!(r2, Classification::Outcome { .. }));

        // Tier 2 error (no grammar match)
        let r3 = c.classify(Line::Complete("FATAL: out of memory".into()));
        assert!(matches!(
            r3,
            Classification::Hazard {
                severity: Severity::Error,
                ..
            }
        ));

        // Tier 3 decorative
        let r4 = c.classify(Line::Complete("========".into()));
        assert!(matches!(
            r4,
            Classification::Noise {
                action: NoiseAction::Strip,
                ..
            }
        ));

        // Unknown
        let r5 = c.classify(Line::Complete("some random output xyzzy".into()));
        assert!(matches!(r5, Classification::Unknown { .. }));
    }

    // Test 83: Temporal heuristic - elapsed time triggers noise bypass automatically
    #[test]
    fn test_temporal_auto_bypass_via_elapsed() {
        let mut c = Classifier::new(None, None);

        // Classify first line to set a baseline
        c.classify(Line::Complete("Downloading package foo v1.0.0".into()));

        // Manually set last_line_time to the past to simulate silence
        c.last_line_time = Instant::now() - Duration::from_secs(3);

        // This line would normally dedup, but elapsed > SILENCE_THRESHOLD
        let result = c.classify(Line::Complete("Downloading package bar v1.0.0".into()));
        assert!(!matches!(
            result,
            Classification::Noise {
                action: NoiseAction::Dedup,
                ..
            }
        ));
    }

    // -----------------------------------------------------------------------
    // Tier 3b: ImplicitDedup in classifier path
    // -----------------------------------------------------------------------

    // Test 84: 5 consecutive similar lines → classified as Noise(Dedup)
    #[test]
    fn test_implicit_dedup_consecutive_similar_noise_dedup() {
        let mut c = Classifier::new(None, None);

        // First line: NotSimilar → Unknown
        let r1 = c.classify(Line::Complete("Compiling serde v1.0.195".into()));
        assert!(matches!(r1, Classification::Unknown { .. }));

        // Lines 2-5: Absorbed → Noise(Dedup)
        let r2 = c.classify(Line::Complete("Compiling tokio v1.35.0".into()));
        assert!(matches!(
            r2,
            Classification::Noise {
                action: NoiseAction::Dedup,
                ..
            }
        ));

        let r3 = c.classify(Line::Complete("Compiling regex v1.10.0".into()));
        assert!(matches!(
            r3,
            Classification::Noise {
                action: NoiseAction::Dedup,
                ..
            }
        ));

        let r4 = c.classify(Line::Complete("Compiling syn v2.0.48".into()));
        assert!(matches!(
            r4,
            Classification::Noise {
                action: NoiseAction::Dedup,
                ..
            }
        ));

        let r5 = c.classify(Line::Complete("Compiling quote v1.0.35".into()));
        assert!(matches!(
            r5,
            Classification::Noise {
                action: NoiseAction::Dedup,
                ..
            }
        ));
    }

    // Test 85: Dissimilar lines → still Unknown (not affected by ImplicitDedup)
    #[test]
    fn test_implicit_dedup_dissimilar_lines_still_unknown() {
        let mut c = Classifier::new(None, None);

        let r1 = c.classify(Line::Complete("Starting build process".into()));
        assert!(matches!(r1, Classification::Unknown { .. }));

        let r2 = c.classify(Line::Complete("Loading configuration file".into()));
        assert!(matches!(r2, Classification::Unknown { .. }));

        let r3 = c.classify(Line::Complete("Initializing database connection".into()));
        assert!(matches!(r3, Classification::Unknown { .. }));
    }

    // Test 86: Mixed — similar lines then a different line → Noise then Unknown
    #[test]
    fn test_implicit_dedup_mixed_similar_then_different() {
        let mut c = Classifier::new(None, None);

        // Similar lines
        let r1 = c.classify(Line::Complete("Compiling serde v1.0.195".into()));
        assert!(matches!(r1, Classification::Unknown { .. }));

        let r2 = c.classify(Line::Complete("Compiling tokio v1.35.0".into()));
        assert!(matches!(
            r2,
            Classification::Noise {
                action: NoiseAction::Dedup,
                ..
            }
        ));

        let r3 = c.classify(Line::Complete("Compiling regex v1.10.0".into()));
        assert!(matches!(
            r3,
            Classification::Noise {
                action: NoiseAction::Dedup,
                ..
            }
        ));

        // Break the streak with a dissimilar line
        let r4 = c.classify(Line::Complete("Finished release target in 45.2s".into()));
        assert!(matches!(r4, Classification::Unknown { .. }));
    }

    // Test 87: Grammar-matched lines still classified by Tier 1 (ImplicitDedup doesn't override)
    #[test]
    fn test_implicit_dedup_does_not_override_grammar() {
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

        // These all match the grammar noise rule (Tier 1) — ImplicitDedup should NOT interfere
        let r1 = c.classify(Line::Complete("Compiling serde v1.0.195".into()));
        assert!(matches!(
            r1,
            Classification::Noise {
                action: NoiseAction::Dedup,
                ..
            }
        ));

        let r2 = c.classify(Line::Complete("Compiling tokio v1.35.0".into()));
        assert!(matches!(
            r2,
            Classification::Noise {
                action: NoiseAction::Dedup,
                ..
            }
        ));

        // Non-matching line → should fall through to Unknown (or Tier 2/3)
        let r3 = c.classify(Line::Complete("Finished release target in 45.2s".into()));
        assert!(matches!(r3, Classification::Unknown { .. }));
    }

    // -----------------------------------------------------------------------
    // Stack trace compression (d1)
    // -----------------------------------------------------------------------

    // Test 87: Long stack trace (50 frames) compresses to summary + last frame
    #[test]
    fn test_stack_trace_compression_long() {
        let mut c = Classifier::new(None, None);

        // First frame — enters ConsumingTrace, emitted as Hazard
        let r1 = c.classify(Line::Complete(
            "    at func1 (app.js:1:1)".into(),
        ));
        assert!(matches!(r1, Classification::Hazard { .. }));
        assert_eq!(c.state(), ClassifierState::ConsumingTrace);

        // 48 continuation frames (frames 2–49)
        for i in 2..=49 {
            let r = c.classify(Line::Complete(
                format!("    at func{} (app.js:{}:1)", i, i),
            ));
            assert!(matches!(r, Classification::Noise { action: NoiseAction::Strip, .. }));
        }

        // Terminating line ends the trace
        let r_end = c.classify(Line::Complete("Process exited with code 1".into()));

        // Should return compressed Hazard (not the terminating line)
        assert!(matches!(r_end, Classification::Hazard { .. }));
        if let Classification::Hazard { ref text, .. } = r_end {
            // Should mention frame count: 49 total - first - last = 47 middle frames
            assert!(text.contains("47 frames"), "expected '47 frames' in: {}", text);
            // Should include the last frame
            assert!(text.contains("func49"), "expected last frame 'func49' in: {}", text);
        }

        // Drain deferred: should return classification for "Process exited with code 1"
        let deferred = c.drain_deferred();
        assert!(deferred.is_some(), "expected deferred classification");
        assert!(matches!(deferred.unwrap(), Classification::Unknown { .. }));
    }

    // Test 88: Two-frame trace emits last frame (no "N frames" marker)
    #[test]
    fn test_stack_trace_compression_two_frames() {
        let mut c = Classifier::new(None, None);

        let r1 = c.classify(Line::Complete(
            "    at func1 (app.js:1:1)".into(),
        ));
        assert!(matches!(r1, Classification::Hazard { .. }));

        let r2 = c.classify(Line::Complete(
            "    at func2 (app.js:2:1)".into(),
        ));
        assert!(matches!(r2, Classification::Noise { action: NoiseAction::Strip, .. }));

        // Terminating line
        let r_end = c.classify(Line::Complete("Done.".into()));

        // Should return Hazard with last frame text (no "frames" marker)
        assert!(matches!(r_end, Classification::Hazard { .. }));
        if let Classification::Hazard { ref text, .. } = r_end {
            assert!(text.contains("func2"), "expected last frame in: {}", text);
            assert!(!text.contains("frames"), "should not have frames marker for 2-frame trace: {}", text);
        }

        // Drain deferred: "Done." → Unknown
        let deferred = c.drain_deferred();
        assert!(deferred.is_some());
        assert!(matches!(deferred.unwrap(), Classification::Unknown { .. }));
    }

    // Test 89: Single frame — no compression, no deferral
    #[test]
    fn test_stack_trace_no_compression_single_frame() {
        let mut c = Classifier::new(None, None);

        let r1 = c.classify(Line::Complete(
            "    at func1 (app.js:1:1)".into(),
        ));
        assert!(matches!(r1, Classification::Hazard { .. }));
        assert_eq!(c.state(), ClassifierState::ConsumingTrace);

        // Terminating line immediately (only 1 frame collected)
        let r_end = c.classify(Line::Complete("Done.".into()));

        // Should fall through to normal classification (no compression)
        assert!(matches!(r_end, Classification::Unknown { .. }));
        assert_eq!(c.state(), ClassifierState::Running);

        // No deferred line
        let deferred = c.drain_deferred();
        assert!(deferred.is_none());
    }

    // Test 90: drain_deferred returns None when no trace ended
    #[test]
    fn test_drain_deferred_empty() {
        let mut c = Classifier::new(None, None);

        c.classify(Line::Complete("hello world".into()));
        let deferred = c.drain_deferred();
        assert!(deferred.is_none());
    }

    // Test 91: Python traceback with File+code interleaving compresses correctly
    #[test]
    fn test_stack_trace_compression_python_interleaved() {
        let mut c = Classifier::new(None, None);

        // "Traceback" header — Hazard from error_start, NOT ConsumingTrace
        let r0 = c.classify(Line::Complete(
            "Traceback (most recent call last):".into(),
        ));
        assert!(matches!(r0, Classification::Hazard { .. }));
        assert_ne!(c.state(), ClassifierState::ConsumingTrace);

        // First File frame — enters ConsumingTrace
        let r1 = c.classify(Line::Complete(
            "  File \"/app/main.py\", line 42".into(),
        ));
        assert!(matches!(r1, Classification::Hazard { .. }));
        assert_eq!(c.state(), ClassifierState::ConsumingTrace);

        // Code line (indented — continuation)
        c.classify(Line::Complete("    result = process(data)".into()));

        // 10 more File+code pairs
        for i in 0..10 {
            c.classify(Line::Complete(
                format!("  File \"/app/mod{}.py\", line {}", i, i * 10),
            ));
            c.classify(Line::Complete(
                format!("    code_line_{}", i),
            ));
        }

        // consuming_trace_lines: 1 (first) + 1 (code) + 20 (10 pairs) = 22

        // Terminating error line
        let r_end = c.classify(Line::Complete("ValueError: bad input".into()));

        // Should return compressed Hazard
        assert!(matches!(r_end, Classification::Hazard { .. }));
        if let Classification::Hazard { ref text, .. } = r_end {
            // 22 total - 2 (first + last) = 20 middle frames
            assert!(text.contains("20 frames"), "expected '20 frames' in: {}", text);
        }

        // Drain deferred: "ValueError: bad input" → Unknown (no universal pattern matches)
        let deferred = c.drain_deferred();
        assert!(deferred.is_some());
        assert!(matches!(deferred.unwrap(), Classification::Unknown { .. }));
    }

    // -----------------------------------------------------------------------
    // Grammar inheritance end-to-end through Classifier (d2)
    // -----------------------------------------------------------------------

    // Test 92: npm grammar inherits ansi-progress — progress patterns classified as Noise(Strip)
    #[test]
    fn test_grammar_inheritance_npm_ansi_progress_in_classifier() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("grammars");
        let grammars = load_all_grammars(&dir).unwrap();

        // Detect npm install
        let args: Vec<String> = vec!["npm", "install"].iter().map(|s| s.to_string()).collect();
        let (grammar, action) = detect_tool(&args, &grammars).expect("npm should detect");

        let mut c = Classifier::new(Some(grammar), action);

        // Standalone percentage — inherited from ansi-progress (pattern: '^\s*\d{1,3}%\s*$')
        let r1 = c.classify(Line::Complete("  75%  ".into()));
        assert!(
            matches!(r1, Classification::Noise { action: NoiseAction::Strip, .. }),
            "inherited ansi-progress '75%%' should be Noise(Strip), got {:?}", r1
        );

        // Progress bar shape — inherited from ansi-progress
        let r2 = c.classify(Line::Complete("[########        ]".into()));
        assert!(
            matches!(r2, Classification::Noise { action: NoiseAction::Strip, .. }),
            "inherited ansi-progress bar should be Noise(Strip), got {:?}", r2
        );

        // npm's own global_noise rule should still work
        let r3 = c.classify(Line::Complete("npm timing idealTree Completed in 123ms".into()));
        assert!(
            matches!(r3, Classification::Noise { action: NoiseAction::Strip, .. }),
            "npm's own global_noise should be Noise(Strip), got {:?}", r3
        );
    }

    // Test 93: Own rules take priority over inherited rules in classifier
    #[test]
    fn test_grammar_inheritance_own_rules_priority_in_classifier() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("grammars");
        let grammars = load_all_grammars(&dir).unwrap();

        let args: Vec<String> = vec!["npm", "install"].iter().map(|s| s.to_string()).collect();
        let (grammar, action) = detect_tool(&args, &grammars).expect("npm should detect");

        let mut c = Classifier::new(Some(grammar), action);

        // "npm warn" matches own global_noise (dedup), NOT inherited ansi-progress
        let r = c.classify(Line::Complete("npm warn deprecated package".into()));
        assert!(
            matches!(r, Classification::Noise { action: NoiseAction::Dedup, .. }),
            "npm warn should be Noise(Dedup) from own rule, got {:?}", r
        );
    }
}
