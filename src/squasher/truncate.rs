/// Oreo truncation with enriched markers.
///
/// Keeps first N and last M lines, inserts a marker with suppressed line count.

/// Configuration for Oreo-style truncation.
#[derive(Debug, Clone)]
pub struct TruncateConfig {
    /// Lines to keep from the start
    pub head: usize,
    /// Lines to keep from the end
    pub tail: usize,
}

impl Default for TruncateConfig {
    fn default() -> Self {
        Self { head: 5, tail: 5 }
    }
}

/// Truncates output using Oreo pattern: head + marker + tail.
pub struct Truncator {
    config: TruncateConfig,
}

impl Truncator {
    pub fn new(config: TruncateConfig) -> Self {
        Self { config }
    }

    /// Truncate lines using Oreo pattern: head lines + marker + tail lines.
    ///
    /// If input fits within head+tail, returns unchanged.
    pub fn truncate(&self, lines: &[String]) -> Vec<String> {
        let total = lines.len();
        let budget = self.config.head + self.config.tail;

        if total <= budget {
            return lines.to_vec();
        }

        let suppressed = total - budget;
        let mut result = Vec::with_capacity(budget + 1);

        // Head
        result.extend_from_slice(&lines[..self.config.head]);

        // Marker
        result.push(format!("... {} lines suppressed ...", suppressed));

        // Tail
        result.extend_from_slice(&lines[total - self.config.tail..]);

        result
    }
}

impl Default for Truncator {
    fn default() -> Self {
        Self::new(TruncateConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_input_passthrough() {
        let t = Truncator::new(TruncateConfig { head: 5, tail: 5 });
        let lines: Vec<String> = (1..=8).map(|i| format!("line {}", i)).collect();
        let result = t.truncate(&lines);
        // 8 lines < head+tail=10, so pass through unchanged
        assert_eq!(result, lines);
    }

    #[test]
    fn test_exact_fit_no_truncation() {
        let t = Truncator::new(TruncateConfig { head: 3, tail: 3 });
        let lines: Vec<String> = (1..=6).map(|i| format!("line {}", i)).collect();
        let result = t.truncate(&lines);
        assert_eq!(result, lines);
    }

    #[test]
    fn test_truncation_with_marker() {
        let t = Truncator::new(TruncateConfig { head: 2, tail: 2 });
        let lines: Vec<String> = (1..=10).map(|i| format!("line {}", i)).collect();
        let result = t.truncate(&lines);
        assert_eq!(result.len(), 5); // 2 head + 1 marker + 2 tail
        assert_eq!(result[0], "line 1");
        assert_eq!(result[1], "line 2");
        assert!(result[2].contains("6")); // "... 6 lines suppressed ..."
        assert_eq!(result[3], "line 9");
        assert_eq!(result[4], "line 10");
    }

    #[test]
    fn test_truncation_large_input() {
        let t = Truncator::new(TruncateConfig { head: 5, tail: 5 });
        let lines: Vec<String> = (1..=1000).map(|i| format!("line {}", i)).collect();
        let result = t.truncate(&lines);
        assert_eq!(result.len(), 11); // 5 + 1 marker + 5
        assert_eq!(result[0], "line 1");
        assert_eq!(result[4], "line 5");
        assert!(result[5].contains("990")); // 990 suppressed
        assert_eq!(result[6], "line 996");
        assert_eq!(result[10], "line 1000");
    }

    #[test]
    fn test_empty_input() {
        let t = Truncator::new(TruncateConfig::default());
        let result = t.truncate(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_marker_format() {
        let t = Truncator::new(TruncateConfig { head: 1, tail: 1 });
        let lines: Vec<String> = (1..=100).map(|i| format!("line {}", i)).collect();
        let result = t.truncate(&lines);
        // Marker should be descriptive
        let marker = &result[1];
        assert!(marker.contains("98"));
        assert!(marker.contains("suppressed") || marker.contains("lines"));
    }
}
