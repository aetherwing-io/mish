/// Watch regex matching and presets.
///
/// Filters output by regex patterns. Presets: @errors, @npm, etc.

use regex::Regex;

/// A compiled watch pattern for filtering output.
pub struct PatternMatcher {
    patterns: Vec<Regex>,
}

/// Built-in preset patterns.
pub struct Presets;

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
}
