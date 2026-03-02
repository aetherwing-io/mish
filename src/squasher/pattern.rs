/// Watch regex matching and presets.
///
/// Filters output by regex patterns. Presets: @errors, @npm, etc.
/// Provides WatchFilter for higher-level filtering with unmatched line handling.

use regex::Regex;
use std::fmt;

/// A compiled watch pattern for filtering output.
#[derive(Debug)]
pub struct PatternMatcher {
    patterns: Vec<Regex>,
}

/// Built-in preset patterns.
pub struct Presets;

/// What to do with lines that don't match any watch pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnmatchedMode {
    /// Count unmatched lines and produce a summary (default).
    Squash,
    /// Discard unmatched lines entirely.
    Drop,
}

impl Default for UnmatchedMode {
    fn default() -> Self {
        UnmatchedMode::Squash
    }
}

impl UnmatchedMode {
    /// Parse from string. Returns None for invalid values.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "squash" => Some(UnmatchedMode::Squash),
            "drop" => Some(UnmatchedMode::Drop),
            _ => None,
        }
    }
}

/// Result of filtering lines through a WatchFilter.
#[derive(Debug, Clone)]
pub struct FilterResult {
    /// Lines that matched the watch patterns.
    pub matched: Vec<String>,
    /// Summary of unmatched lines (None if no unmatched lines or mode is Drop).
    pub summary: Option<String>,
}

/// Error type for WatchFilter construction.
#[derive(Debug, Clone)]
pub struct WatchError {
    pub message: String,
}

impl fmt::Display for WatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for WatchError {}

impl From<regex::Error> for WatchError {
    fn from(e: regex::Error) -> Self {
        WatchError {
            message: format!("invalid watch regex: {}", e),
        }
    }
}

/// Higher-level watch filter that resolves presets, compiles patterns,
/// and provides filtering with unmatched line handling.
#[derive(Debug)]
pub struct WatchFilter {
    matcher: PatternMatcher,
    unmatched: UnmatchedMode,
}

impl WatchFilter {
    /// Create a new WatchFilter from the `watch` parameter string and unmatched mode.
    ///
    /// The `watch` string is treated as a preset name if and only if it exactly
    /// matches `@[a-z_]+`. All other values are treated as regex patterns
    /// (pipe-separated, compiled case-insensitive).
    pub fn new(watch: &str, unmatched: UnmatchedMode) -> Result<Self, WatchError> {
        let patterns = if is_preset(watch) {
            Presets::expand(watch)
        } else {
            // Custom regex: split on pipe, compile each as case-insensitive
            watch
                .split('|')
                .filter(|s| !s.is_empty())
                .map(|s| format!("(?i){}", s))
                .collect()
        };

        let pattern_refs: Vec<&str> = patterns.iter().map(|s| s.as_str()).collect();
        let matcher = PatternMatcher::new(&pattern_refs)?;

        Ok(Self { matcher, unmatched })
    }

    /// Filter a list of lines. Returns matched lines and an optional summary
    /// describing unmatched lines.
    pub fn filter(&self, lines: &[String]) -> FilterResult {
        let mut matched = Vec::new();
        let mut unmatched_count: usize = 0;

        for line in lines {
            if self.matcher.matches(line) {
                matched.push(line.clone());
            } else {
                unmatched_count += 1;
            }
        }

        let summary = match self.unmatched {
            UnmatchedMode::Squash if unmatched_count > 0 => {
                Some(format!("... {} lines (no match)", unmatched_count))
            }
            _ => None,
        };

        FilterResult { matched, summary }
    }
}

/// Check if a watch value is a preset name: exactly matches `@[a-z_]+`.
fn is_preset(value: &str) -> bool {
    if !value.starts_with('@') {
        return false;
    }
    let name = &value[1..];
    !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '_')
}

impl PatternMatcher {
    /// Create a new pattern matcher from a list of regex strings.
    pub fn new(patterns: &[&str]) -> Result<Self, regex::Error> {
        let compiled = patterns
            .iter()
            .map(|p| Regex::new(p))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { patterns: compiled })
    }

    /// Check if any pattern matches the given line.
    pub fn matches(&self, line: &str) -> bool {
        self.patterns.iter().any(|p| p.is_match(line))
    }
}

impl Presets {
    /// Expand a preset name into a list of regex pattern strings.
    /// Unknown presets return the name (minus @) as a literal pattern.
    pub fn expand(preset: &str) -> Vec<String> {
        match preset {
            "@errors" => vec![
                r"(?i)\berror\b".to_string(),
                r"(?i)\bfatal\b".to_string(),
                r"(?i)\bfailed\b".to_string(),
                r"(?i)\bpanic\b".to_string(),
                r"error\[E\d+\]".to_string(),
            ],
            "@warnings" => vec![
                r"(?i)\bwarn(ing)?\b".to_string(),
                r"(?i)\bdeprecated\b".to_string(),
            ],
            "@npm" => vec![
                r"(?i)\bnpm\s+(warn|ERR!)".to_string(),
                r"(?i)\bvuln(erabilit)".to_string(),
            ],
            "@test_results" => vec![
                r"(?i)passed".to_string(),
                r"(?i)failed".to_string(),
                r"(?i)\berror\b".to_string(),
                r"(?i)skip".to_string(),
                r"===.*===".to_string(),
                r"(?i)TOTAL".to_string(),
            ],
            other => {
                // Strip @ prefix, use as literal
                let name = other.strip_prefix('@').unwrap_or(other);
                vec![name.to_string()]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Existing tests (8) ──────────────────────────────────────────

    #[test]
    fn test_simple_regex_match() {
        let pm = PatternMatcher::new(&["error"]).unwrap();
        assert!(pm.matches("error: something broke"));
        assert!(!pm.matches("all good"));
    }

    #[test]
    fn test_multiple_patterns_or_match() {
        let pm = PatternMatcher::new(&["error", "warning"]).unwrap();
        assert!(pm.matches("error: bad"));
        assert!(pm.matches("warning: careful"));
        assert!(!pm.matches("info: ok"));
    }

    #[test]
    fn test_regex_pattern() {
        let pm = PatternMatcher::new(&[r"^\d+ errors?"]).unwrap();
        assert!(pm.matches("1 error found"));
        assert!(pm.matches("42 errors found"));
        assert!(!pm.matches("no errors found"));
    }

    #[test]
    fn test_preset_errors_expansion() {
        let patterns = Presets::expand("@errors");
        assert!(!patterns.is_empty());
        let pm = PatternMatcher::new(&patterns.iter().map(|s| s.as_str()).collect::<Vec<_>>()).unwrap();
        assert!(pm.matches("ERROR: something"));
        assert!(pm.matches("error[E0308]: mismatched types"));
        assert!(pm.matches("FATAL: crash"));
    }

    #[test]
    fn test_preset_unknown_returns_literal() {
        let patterns = Presets::expand("@unknown_preset");
        // Unknown preset treated as literal pattern (minus @)
        assert_eq!(patterns, vec!["unknown_preset"]);
    }

    #[test]
    fn test_no_patterns_matches_nothing() {
        let pm = PatternMatcher::new(&[]).unwrap();
        assert!(!pm.matches("anything"));
    }

    #[test]
    fn test_case_insensitive_preset() {
        let patterns = Presets::expand("@errors");
        let pm = PatternMatcher::new(&patterns.iter().map(|s| s.as_str()).collect::<Vec<_>>()).unwrap();
        // Should match case-insensitively for error patterns
        assert!(pm.matches("Error: something"));
    }

    #[test]
    fn test_invalid_regex_returns_error() {
        let result = PatternMatcher::new(&["[invalid"]);
        assert!(result.is_err());
    }

    // ── New tests ───────────────────────────────────────────────────

    // Preset resolution

    #[test]
    fn test_preset_test_results_expansion() {
        let patterns = Presets::expand("@test_results");
        assert!(!patterns.is_empty());
        let pm = PatternMatcher::new(&patterns.iter().map(|s| s.as_str()).collect::<Vec<_>>()).unwrap();
        assert!(pm.matches("47 passed, 2 failed"));
        assert!(pm.matches("Tests PASSED"));
        assert!(pm.matches("FAILED: some test"));
        assert!(pm.matches("error in test_foo"));
        assert!(pm.matches("1 skip"));
        assert!(pm.matches("=== RESULTS ==="));
        assert!(pm.matches("TOTAL: 50"));
        assert!(!pm.matches("compiling crate foo"));
    }

    #[test]
    fn test_preset_warnings_expansion() {
        let patterns = Presets::expand("@warnings");
        let pm = PatternMatcher::new(&patterns.iter().map(|s| s.as_str()).collect::<Vec<_>>()).unwrap();
        assert!(pm.matches("warning: unused variable"));
        assert!(pm.matches("WARN: something"));
        assert!(pm.matches("deprecated function called"));
        assert!(!pm.matches("info: all good"));
    }

    #[test]
    fn test_preset_npm_expansion() {
        let patterns = Presets::expand("@npm");
        let pm = PatternMatcher::new(&patterns.iter().map(|s| s.as_str()).collect::<Vec<_>>()).unwrap();
        assert!(pm.matches("npm WARN deprecated"));
        assert!(pm.matches("npm ERR! code ENOENT"));
        assert!(pm.matches("5 vulnerabilities found"));
        assert!(!pm.matches("added 142 packages"));
    }

    // is_preset detection

    #[test]
    fn test_is_preset_valid() {
        assert!(is_preset("@errors"));
        assert!(is_preset("@test_results"));
        assert!(is_preset("@npm"));
        assert!(is_preset("@warnings"));
        assert!(is_preset("@unknown_thing"));
    }

    #[test]
    fn test_is_preset_invalid() {
        // Not a preset: contains uppercase, digits, special chars, or no @
        assert!(!is_preset("error"));
        assert!(!is_preset("@Error"));
        assert!(!is_preset("@test-results"));
        assert!(!is_preset("@foo123"));
        assert!(!is_preset("@"));
        assert!(!is_preset("error|warning"));
        assert!(!is_preset(""));
    }

    // Custom regex — case-insensitive by default

    #[test]
    fn test_watch_filter_custom_regex_case_insensitive() {
        let wf = WatchFilter::new("error", UnmatchedMode::Drop).unwrap();
        assert!(wf.matcher.matches("ERROR: something"));
        assert!(wf.matcher.matches("Error: something"));
        assert!(wf.matcher.matches("error: something"));
    }

    #[test]
    fn test_watch_filter_custom_regex_pipe_separated() {
        let wf = WatchFilter::new("error|warning|fail", UnmatchedMode::Drop).unwrap();
        assert!(wf.matcher.matches("ERROR: bad"));
        assert!(wf.matcher.matches("Warning: careful"));
        assert!(wf.matcher.matches("test FAILED"));
        assert!(!wf.matcher.matches("info: ok"));
    }

    // Unmatched modes

    #[test]
    fn test_unmatched_squash_produces_summary() {
        let wf = WatchFilter::new("error", UnmatchedMode::Squash).unwrap();
        let lines: Vec<String> = vec![
            "info: starting".into(),
            "error: something broke".into(),
            "info: continuing".into(),
            "info: done".into(),
        ];
        let result = wf.filter(&lines);
        assert_eq!(result.matched.len(), 1);
        assert_eq!(result.matched[0], "error: something broke");
        assert_eq!(result.summary, Some("... 3 lines (no match)".to_string()));
    }

    #[test]
    fn test_unmatched_squash_847_lines() {
        let wf = WatchFilter::new("MATCH", UnmatchedMode::Squash).unwrap();
        let mut lines: Vec<String> = (0..848).map(|i| format!("line {}", i)).collect();
        lines.push("MATCH this one".into());
        let result = wf.filter(&lines);
        assert_eq!(result.matched.len(), 1);
        assert_eq!(result.matched[0], "MATCH this one");
        assert_eq!(result.summary, Some("... 848 lines (no match)".to_string()));
    }

    #[test]
    fn test_unmatched_drop_no_summary() {
        let wf = WatchFilter::new("error", UnmatchedMode::Drop).unwrap();
        let lines: Vec<String> = vec![
            "info: starting".into(),
            "error: something broke".into(),
            "info: done".into(),
        ];
        let result = wf.filter(&lines);
        assert_eq!(result.matched.len(), 1);
        assert_eq!(result.matched[0], "error: something broke");
        assert!(result.summary.is_none());
    }

    #[test]
    fn test_unmatched_squash_no_unmatched_lines() {
        let wf = WatchFilter::new("line", UnmatchedMode::Squash).unwrap();
        let lines: Vec<String> = vec!["line 1".into(), "line 2".into()];
        let result = wf.filter(&lines);
        assert_eq!(result.matched.len(), 2);
        assert!(result.summary.is_none());
    }

    // WatchFilter with presets

    #[test]
    fn test_watch_filter_with_preset() {
        let wf = WatchFilter::new("@errors", UnmatchedMode::Squash).unwrap();
        let lines: Vec<String> = vec![
            "Compiling foo v0.1.0".into(),
            "Compiling bar v0.2.0".into(),
            "error[E0308]: mismatched types".into(),
            "  --> src/main.rs:10:5".into(),
            "Finished dev".into(),
        ];
        let result = wf.filter(&lines);
        assert_eq!(result.matched.len(), 1);
        assert_eq!(result.matched[0], "error[E0308]: mismatched types");
        assert_eq!(result.summary, Some("... 4 lines (no match)".to_string()));
    }

    #[test]
    fn test_watch_filter_with_test_results_preset() {
        let wf = WatchFilter::new("@test_results", UnmatchedMode::Drop).unwrap();
        let lines: Vec<String> = vec![
            "running 5 tests".into(),
            "test foo ... ok".into(),
            "test bar ... FAILED".into(),
            "  thread panicked at ...".into(),
            "=== SUMMARY ===".into(),
            "TOTAL: 5".into(),
        ];
        let result = wf.filter(&lines);
        // "FAILED" matches, "=== SUMMARY ===" matches, "TOTAL: 5" matches
        // "test foo ... ok" does NOT match (no "passed", "failed", "error", "skip" pattern)
        assert!(result.matched.contains(&"test bar ... FAILED".to_string()));
        assert!(result.matched.contains(&"=== SUMMARY ===".to_string()));
        assert!(result.matched.contains(&"TOTAL: 5".to_string()));
        assert!(result.summary.is_none()); // Drop mode
    }

    // Invalid regex → WatchError (not panic)

    #[test]
    fn test_watch_filter_invalid_regex_returns_error() {
        let result = WatchFilter::new("[invalid", UnmatchedMode::Drop);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("invalid watch regex"));
    }

    #[test]
    fn test_watch_filter_invalid_regex_in_pipe() {
        let result = WatchFilter::new("good|[bad", UnmatchedMode::Drop);
        assert!(result.is_err());
    }

    // Unknown preset → treated as literal

    #[test]
    fn test_watch_filter_unknown_preset_literal() {
        let wf = WatchFilter::new("@foobar", UnmatchedMode::Drop).unwrap();
        // @foobar is a valid preset name format, so Presets::expand returns "foobar" as literal
        assert!(wf.matcher.matches("found foobar here"));
        assert!(!wf.matcher.matches("nothing relevant"));
    }

    // UnmatchedMode parsing

    #[test]
    fn test_unmatched_mode_from_str() {
        assert_eq!(UnmatchedMode::from_str("squash"), Some(UnmatchedMode::Squash));
        assert_eq!(UnmatchedMode::from_str("drop"), Some(UnmatchedMode::Drop));
        assert_eq!(UnmatchedMode::from_str("keep"), None);
        assert_eq!(UnmatchedMode::from_str("invalid"), None);
        assert_eq!(UnmatchedMode::from_str(""), None);
    }

    #[test]
    fn test_unmatched_mode_default_is_squash() {
        assert_eq!(UnmatchedMode::default(), UnmatchedMode::Squash);
    }

    // Integration: filter a Vec<String> of mixed output

    #[test]
    fn test_integration_npm_install_filter() {
        let wf = WatchFilter::new("@npm", UnmatchedMode::Squash).unwrap();
        let lines: Vec<String> = vec![
            "npm warn deprecated inflight@1.0.6".into(),
            "added 142 packages in 3s".into(),
            "npm warn deprecated rimraf@3.0.2".into(),
            "5 packages are looking for funding".into(),
            "npm ERR! code ENOENT".into(),
            "some other output".into(),
        ];
        let result = wf.filter(&lines);
        assert_eq!(result.matched.len(), 3);
        assert!(result.matched.contains(&"npm warn deprecated inflight@1.0.6".to_string()));
        assert!(result.matched.contains(&"npm warn deprecated rimraf@3.0.2".to_string()));
        assert!(result.matched.contains(&"npm ERR! code ENOENT".to_string()));
        assert_eq!(result.summary, Some("... 3 lines (no match)".to_string()));
    }

    #[test]
    fn test_integration_mixed_output_with_drop() {
        let wf = WatchFilter::new("error|warning", UnmatchedMode::Drop).unwrap();
        let lines: Vec<String> = (0..1000)
            .map(|i| {
                if i == 500 {
                    "ERROR: disk full".to_string()
                } else if i == 750 {
                    "Warning: low memory".to_string()
                } else {
                    format!("log line {}", i)
                }
            })
            .collect();
        let result = wf.filter(&lines);
        assert_eq!(result.matched.len(), 2);
        assert_eq!(result.matched[0], "ERROR: disk full");
        assert_eq!(result.matched[1], "Warning: low memory");
        assert!(result.summary.is_none());
    }

    #[test]
    fn test_filter_empty_input() {
        let wf = WatchFilter::new("error", UnmatchedMode::Squash).unwrap();
        let result = wf.filter(&[]);
        assert!(result.matched.is_empty());
        assert!(result.summary.is_none());
    }

    #[test]
    fn test_filter_all_lines_match() {
        let wf = WatchFilter::new("line", UnmatchedMode::Squash).unwrap();
        let lines: Vec<String> = vec!["line 1".into(), "line 2".into(), "line 3".into()];
        let result = wf.filter(&lines);
        assert_eq!(result.matched.len(), 3);
        assert!(result.summary.is_none());
    }
}
