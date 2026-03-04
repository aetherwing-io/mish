//! Oreo truncation with enriched markers.
//!
//! Keeps first N and last M lines, inserts a marker with suppressed line count
//! and hazard summary from the truncated middle section.

/// Max output bytes hard limit (64KB).
pub const DEFAULT_MAX_BYTES: usize = 65536;

/// Hazard counts detected in the truncated (hidden) middle region.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HazardCounts {
    /// Number of error-level lines in the hidden region
    pub errors: usize,
    /// Number of warning-level lines in the hidden region
    pub warnings: usize,
}

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
        Self { head: 50, tail: 150 }
    }
}

/// Truncates output using Oreo pattern: head + marker + tail.
pub struct Truncator {
    config: TruncateConfig,
    max_bytes: usize,
}

impl Truncator {
    pub fn new(config: TruncateConfig) -> Self {
        Self {
            config,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    /// Create a truncator with a custom byte limit.
    pub fn with_max_bytes(config: TruncateConfig, max_bytes: usize) -> Self {
        Self { config, max_bytes }
    }

    /// Truncate lines using Oreo pattern: head lines + marker + tail lines.
    ///
    /// If input fits within head+tail, returns unchanged (subject to byte limit).
    /// The marker uses the simple format without hazard counts.
    pub fn truncate(&self, lines: &[String]) -> Vec<String> {
        self.truncate_with_counts(lines, &HazardCounts::default())
    }

    /// Truncate lines with enriched marker that includes hazard counts.
    ///
    /// The marker format depends on whether hazards were detected in the hidden region:
    /// - With hazards: `"... [N lines truncated — 3 errors, 12 warnings in hidden region] ..."`
    /// - Without hazards: `"... [N lines truncated] ..."`
    pub fn truncate_with_counts(&self, lines: &[String], hazards: &HazardCounts) -> Vec<String> {
        if lines.is_empty() {
            return Vec::new();
        }

        let total = lines.len();
        let budget = self.config.head + self.config.tail;

        // If within line budget, just apply byte limit
        if total <= budget {
            return self.apply_byte_limit(lines.to_vec());
        }

        let suppressed = total - budget;
        let mut result = Vec::with_capacity(budget + 1);

        // Head
        result.extend_from_slice(&lines[..self.config.head]);

        // Enriched marker
        result.push(Self::format_marker(suppressed, hazards));

        // Tail
        result.extend_from_slice(&lines[total - self.config.tail..]);

        self.apply_byte_limit(result)
    }

    /// Format the truncation marker, including hazard counts if present.
    fn format_marker(suppressed: usize, hazards: &HazardCounts) -> String {
        if hazards.errors == 0 && hazards.warnings == 0 {
            format!("... [{} lines truncated] ...", suppressed)
        } else {
            let mut parts = Vec::new();
            if hazards.errors > 0 {
                parts.push(format!(
                    "{} {}",
                    hazards.errors,
                    if hazards.errors == 1 { "error" } else { "errors" }
                ));
            }
            if hazards.warnings > 0 {
                parts.push(format!(
                    "{} {}",
                    hazards.warnings,
                    if hazards.warnings == 1 {
                        "warning"
                    } else {
                        "warnings"
                    }
                ));
            }
            format!(
                "... [{} lines truncated — {} in hidden region] ...",
                suppressed,
                parts.join(", ")
            )
        }
    }

    /// Apply byte limit to output lines, removing lines from the middle if needed.
    ///
    /// Tracks cumulative bytes and stops adding lines when the limit is exceeded.
    /// Preserves head/tail structure by trimming from the middle.
    fn apply_byte_limit(&self, lines: Vec<String>) -> Vec<String> {
        let total_bytes: usize = lines.iter().map(|l| l.len() + 1).sum(); // +1 for newline
        if total_bytes <= self.max_bytes {
            return lines;
        }

        let n = lines.len();
        if n == 0 {
            return lines;
        }

        // Reserve space for the byte-limit marker line
        let marker_reserve = 80;

        let mut head_end = 0;
        let mut tail_start = n;
        let mut used_bytes: usize = 0;

        // Add lines from the head
        for (i, line) in lines.iter().enumerate().take(n) {
            let line_bytes = line.len() + 1;
            if used_bytes + line_bytes + marker_reserve > self.max_bytes {
                break;
            }
            used_bytes += line_bytes;
            head_end = i + 1;
        }

        // Add lines from the tail (going backwards)
        for i in (head_end..n).rev() {
            let line_bytes = lines[i].len() + 1;
            if used_bytes + line_bytes + marker_reserve > self.max_bytes {
                break;
            }
            used_bytes += line_bytes;
            tail_start = i;
        }

        // If nothing was trimmed, return as-is
        if head_end >= tail_start {
            return lines;
        }

        let trimmed = n - head_end - (n - tail_start);
        let mut result = Vec::with_capacity(head_end + 1 + (n - tail_start));
        result.extend_from_slice(&lines[..head_end]);
        result.push(format!(
            "... [{} lines truncated — byte limit] ...",
            trimmed
        ));
        result.extend_from_slice(&lines[tail_start..]);
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

    // --- Existing tests (preserved, compatible with new structure) ---

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
        assert!(result[2].contains("6")); // 6 lines truncated
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
        assert!(result[5].contains("990")); // 990 truncated
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
        assert!(marker.contains("truncated") || marker.contains("lines"));
    }

    // --- New tests for Phase 3 spec compliance ---

    #[test]
    fn test_default_config_values() {
        let config = TruncateConfig::default();
        assert_eq!(config.head, 50);
        assert_eq!(config.tail, 150);
    }

    #[test]
    fn test_default_max_bytes() {
        assert_eq!(DEFAULT_MAX_BYTES, 65536);
    }

    #[test]
    fn test_50_lines_no_truncation() {
        // 50-line output with default config (budget=200) -> no truncation
        let t = Truncator::new(TruncateConfig::default());
        let lines: Vec<String> = (1..=50).map(|i| format!("line {}", i)).collect();
        let result = t.truncate(&lines);
        assert_eq!(result.len(), 50);
        assert_eq!(result, lines);
    }

    #[test]
    fn test_300_lines_oreo_default() {
        // 300-line output with default config: 50 head + marker + 150 tail
        let t = Truncator::new(TruncateConfig::default());
        let lines: Vec<String> = (1..=300).map(|i| format!("line {}", i)).collect();
        let result = t.truncate(&lines);
        // 50 head + 1 marker + 150 tail = 201
        assert_eq!(result.len(), 201);
        // Check head
        assert_eq!(result[0], "line 1");
        assert_eq!(result[49], "line 50");
        // Check marker (100 lines truncated)
        assert!(result[50].contains("100"));
        assert!(result[50].contains("truncated"));
        // Check tail
        assert_eq!(result[51], "line 151");
        assert_eq!(result[200], "line 300");
    }

    #[test]
    fn test_custom_head_tail_split() {
        let t = Truncator::new(TruncateConfig { head: 10, tail: 20 });
        let lines: Vec<String> = (1..=100).map(|i| format!("line {}", i)).collect();
        let result = t.truncate(&lines);
        // 10 head + 1 marker + 20 tail = 31
        assert_eq!(result.len(), 31);
        assert_eq!(result[0], "line 1");
        assert_eq!(result[9], "line 10");
        assert!(result[10].contains("70")); // 70 lines truncated
        assert_eq!(result[11], "line 81");
        assert_eq!(result[30], "line 100");
    }

    #[test]
    fn test_enriched_marker_with_hazards() {
        let t = Truncator::new(TruncateConfig { head: 5, tail: 5 });
        let lines: Vec<String> = (1..=860).map(|i| format!("line {}", i)).collect();
        let hazards = HazardCounts {
            errors: 3,
            warnings: 12,
        };
        let result = t.truncate_with_counts(&lines, &hazards);
        let marker = &result[5];
        assert!(marker.contains("850 lines truncated"));
        assert!(marker.contains("3 errors"));
        assert!(marker.contains("12 warnings"));
        assert!(marker.contains("hidden region"));
    }

    #[test]
    fn test_enriched_marker_zero_hazards() {
        let t = Truncator::new(TruncateConfig { head: 5, tail: 5 });
        let lines: Vec<String> = (1..=860).map(|i| format!("line {}", i)).collect();
        let hazards = HazardCounts {
            errors: 0,
            warnings: 0,
        };
        let result = t.truncate_with_counts(&lines, &hazards);
        let marker = &result[5];
        // Zero hazards: no hazard counts, just line count
        assert!(marker.contains("850 lines truncated"));
        assert!(!marker.contains("error"));
        assert!(!marker.contains("warning"));
        assert!(!marker.contains("hidden region"));
    }

    #[test]
    fn test_enriched_marker_errors_only() {
        let t = Truncator::new(TruncateConfig { head: 2, tail: 2 });
        let lines: Vec<String> = (1..=20).map(|i| format!("line {}", i)).collect();
        let hazards = HazardCounts {
            errors: 5,
            warnings: 0,
        };
        let result = t.truncate_with_counts(&lines, &hazards);
        let marker = &result[2];
        assert!(marker.contains("5 errors"));
        assert!(!marker.contains("warning"));
    }

    #[test]
    fn test_enriched_marker_warnings_only() {
        let t = Truncator::new(TruncateConfig { head: 2, tail: 2 });
        let lines: Vec<String> = (1..=20).map(|i| format!("line {}", i)).collect();
        let hazards = HazardCounts {
            errors: 0,
            warnings: 7,
        };
        let result = t.truncate_with_counts(&lines, &hazards);
        let marker = &result[2];
        assert!(!marker.contains("error"));
        assert!(marker.contains("7 warnings"));
    }

    #[test]
    fn test_enriched_marker_singular_forms() {
        let t = Truncator::new(TruncateConfig { head: 2, tail: 2 });
        let lines: Vec<String> = (1..=20).map(|i| format!("line {}", i)).collect();
        let hazards = HazardCounts {
            errors: 1,
            warnings: 1,
        };
        let result = t.truncate_with_counts(&lines, &hazards);
        let marker = &result[2];
        assert!(marker.contains("1 error"));
        assert!(!marker.contains("errors"));
        assert!(marker.contains("1 warning"));
        assert!(!marker.contains("warnings"));
    }

    #[test]
    fn test_max_bytes_truncation() {
        // Create lines that exceed 64KB
        let big_line = "x".repeat(1000); // 1000 bytes per line
        let lines: Vec<String> = (0..100).map(|_| big_line.clone()).collect();
        // 100 lines * ~1001 bytes = ~100KB > 64KB
        let t = Truncator::new(TruncateConfig { head: 50, tail: 50 });
        let result = t.truncate(&lines);
        // Should be trimmed to fit 64KB
        let total_bytes: usize = result.iter().map(|l| l.len() + 1).sum();
        assert!(
            total_bytes <= DEFAULT_MAX_BYTES + 80,
            "total_bytes={} exceeds limit",
            total_bytes
        );
    }

    #[test]
    fn test_max_bytes_small_limit() {
        // Use a very small byte limit to force byte truncation
        let t = Truncator::with_max_bytes(TruncateConfig { head: 5, tail: 5 }, 100);
        let lines: Vec<String> = (1..=8)
            .map(|i| format!("this is line number {}", i))
            .collect();
        // 8 lines within line budget (5+5=10), but may exceed 100 bytes
        let total_bytes: usize = lines.iter().map(|l| l.len() + 1).sum();
        if total_bytes > 100 {
            let result = t.truncate(&lines);
            let result_bytes: usize = result.iter().map(|l| l.len() + 1).sum();
            // Result should be smaller, and contain a byte-limit marker
            assert!(result_bytes < total_bytes);
            assert!(result.iter().any(|l| l.contains("byte limit")));
        }
    }

    #[test]
    fn test_exactly_at_budget_no_truncation() {
        let t = Truncator::new(TruncateConfig { head: 10, tail: 10 });
        let lines: Vec<String> = (1..=20).map(|i| format!("line {}", i)).collect();
        let result = t.truncate(&lines);
        assert_eq!(result.len(), 20);
        assert_eq!(result, lines);
    }

    #[test]
    fn test_one_line_over_budget() {
        let t = Truncator::new(TruncateConfig { head: 10, tail: 10 });
        let lines: Vec<String> = (1..=21).map(|i| format!("line {}", i)).collect();
        let result = t.truncate(&lines);
        // 10 head + 1 marker + 10 tail = 21
        assert_eq!(result.len(), 21);
        assert_eq!(result[0], "line 1");
        assert_eq!(result[9], "line 10");
        assert!(result[10].contains("1 lines truncated") || result[10].contains("1 line"));
        assert_eq!(result[11], "line 12");
        assert_eq!(result[20], "line 21");
    }

    #[test]
    fn test_hazard_counts_default() {
        let h = HazardCounts::default();
        assert_eq!(h.errors, 0);
        assert_eq!(h.warnings, 0);
    }

    #[test]
    fn test_truncate_delegates_to_truncate_with_counts() {
        // Verify that truncate() produces same result as truncate_with_counts with zero hazards
        let t = Truncator::new(TruncateConfig { head: 3, tail: 3 });
        let lines: Vec<String> = (1..=20).map(|i| format!("line {}", i)).collect();
        let result_basic = t.truncate(&lines);
        let result_counts = t.truncate_with_counts(&lines, &HazardCounts::default());
        assert_eq!(result_basic, result_counts);
    }

    #[test]
    fn test_byte_limit_with_large_lines() {
        // Lines within Oreo line budget but exceeding byte limit
        let big_line = "x".repeat(2000);
        let t = Truncator::with_max_bytes(TruncateConfig { head: 10, tail: 10 }, 5000);
        let lines: Vec<String> = (0..50)
            .map(|i| {
                if !(5..45).contains(&i) {
                    big_line.clone()
                } else {
                    format!("short line {}", i)
                }
            })
            .collect();
        let result = t.truncate(&lines);
        let total_bytes: usize = result.iter().map(|l| l.len() + 1).sum();
        // Should respect byte limit (with marker overhead allowance)
        assert!(
            total_bytes <= 5000 + 80,
            "total_bytes={} exceeds limit",
            total_bytes
        );
    }

    #[test]
    fn test_with_max_bytes_constructor() {
        let t = Truncator::with_max_bytes(TruncateConfig { head: 5, tail: 5 }, 1024);
        // Verify it uses the custom byte limit by feeding large content
        let big_line = "y".repeat(200);
        let lines: Vec<String> = (0..8).map(|_| big_line.clone()).collect();
        // 8 lines * 201 bytes = 1608 bytes > 1024 bytes
        let result = t.truncate(&lines);
        let total_bytes: usize = result.iter().map(|l| l.len() + 1).sum();
        assert!(
            total_bytes <= 1024 + 80,
            "total_bytes={} exceeds 1024 + marker overhead",
            total_bytes
        );
    }
}
