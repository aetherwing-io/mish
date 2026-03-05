//! Squasher pipeline orchestration.
//!
//! VTE strip -> progress removal -> dedup -> truncation -> output

use crate::core::grammar::BlockRule;
use crate::core::line_buffer::Line;
use crate::router::categories::Category;
use crate::squasher::block::BlockCompressor;
use crate::squasher::dedup::DedupEngine;
use crate::squasher::progress::{ProgressFilter, ProgressResult};
use crate::squasher::truncate::{HazardCounts, TruncateConfig, Truncator};
use crate::squasher::vte_strip::VteStripper;

/// Metrics collected during pipeline processing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PipelineMetrics {
    pub lines_in: u64,
    pub lines_out: u64,
    pub vte_stripped: u64,
    pub progress_stripped: u64,
    pub blocks_compressed: u64,
    pub dedup_groups: u64,
    pub dedup_absorbed: u64,
    pub oreo_suppressed: u64,
    pub binary_detected: bool,
}

/// Detected content type of the output, used to bypass dedup for high-value content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
    /// Repetitive build/install output — full pipeline
    Normal,
    /// Unified diff format — skip dedup
    Diff,
    /// >80% unique lines — skip dedup
    HighEntropy,
}

/// Configuration for the squasher pipeline.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    pub truncate: TruncateConfig,
    /// If true, run dedup on all lines (not just noise-classified ones)
    pub dedup_all: bool,
    /// Block compression rules (from grammar `[[block]]` sections).
    pub block_rules: Vec<BlockRule>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            truncate: TruncateConfig::default(),
            dedup_all: true,
            block_rules: Vec::new(),
        }
    }
}

/// Result from category-aware pipeline processing.
#[derive(Debug, Clone)]
pub struct PipelineResult {
    pub output: Vec<String>,
    pub metrics: PipelineMetrics,
    pub category: Category,
}

/// The squasher pipeline: processes raw lines into condensed output.
pub struct Pipeline {
    progress: ProgressFilter,
    block_compressor: BlockCompressor,
    dedup: DedupEngine,
    truncator: Truncator,
    config: PipelineConfig,
    /// Accumulated clean output lines
    output: Vec<String>,
    metrics: PipelineMetrics,
}

impl Pipeline {
    pub fn new(config: PipelineConfig) -> Self {
        let block_compressor = BlockCompressor::new(config.block_rules.clone());
        Self {
            progress: ProgressFilter::new(),
            block_compressor,
            dedup: DedupEngine::new(),
            truncator: Truncator::new(config.truncate.clone()),
            config,
            output: Vec::new(),
            metrics: PipelineMetrics::default(),
        }
    }

    /// Feed a line through the pipeline stages.
    pub fn feed(&mut self, line: Line) {
        self.metrics.lines_in += 1;

        // Stage 1: Progress filtering (collapse overwrites)
        let progress_results = self.progress.feed(line);

        for pr in progress_results {
            match pr {
                ProgressResult::Pass(line) => {
                    let text = match &line {
                        Line::Complete(s) | Line::Partial(s) | Line::Overwrite(s) => s.clone(),
                    };

                    // Stage 2: VTE strip (remove ANSI codes)
                    let meta = VteStripper::strip(text.as_bytes());
                    let clean = meta.clean_text;
                    if clean != text {
                        self.metrics.vte_stripped += 1;
                    }

                    // Stage 2.5: Block compression
                    let compressed_lines = self.block_compressor.feed(clean);
                    for compressed in compressed_lines {
                        // Stage 3: Dedup (if enabled)
                        if self.config.dedup_all {
                            self.dedup.ingest(&compressed);
                        } else {
                            self.output.push(compressed);
                        }
                    }
                }
                ProgressResult::Stripped => {
                    self.metrics.progress_stripped += 1;
                }
                ProgressResult::FinalState(text) => {
                    let meta = VteStripper::strip(text.as_bytes());
                    self.output.push(meta.clean_text);
                }
            }
        }
    }

    /// Finalize the pipeline and return metrics alongside output.
    pub fn finalize_with_metrics(&mut self) -> (Vec<String>, PipelineMetrics) {
        // Capture dedup stats before finalize drains the groups
        let dedup_stats = self.dedup.stats();
        self.metrics.dedup_groups = dedup_stats.groups;
        self.metrics.dedup_absorbed = dedup_stats.absorbed;

        // Flush progress and dedup (same as finalize)
        self.flush_remaining();

        // Count pre-truncation lines
        let pre_truncation = self.output.len() as u64;

        // Stage 4: Truncation with enriched hazard counts
        let output = std::mem::take(&mut self.output);
        let budget = self.config.truncate.head + self.config.truncate.tail;
        let hazards = if output.len() > budget {
            // Scan the middle section that will be hidden
            Self::scan_hazards(&output[self.config.truncate.head..output.len() - self.config.truncate.tail])
        } else {
            HazardCounts::default()
        };
        let truncated = self.truncator.truncate_with_counts(&output, &hazards);

        let mut metrics = std::mem::take(&mut self.metrics);
        metrics.lines_out = truncated.len() as u64;

        // oreo_suppressed = lines hidden by truncation (not counting the marker line)
        if truncated.len() < output.len() {
            // head + tail kept, rest suppressed. Marker line doesn't count as a kept line.
            let kept = metrics.lines_out.saturating_sub(1); // subtract the marker
            metrics.oreo_suppressed = pre_truncation.saturating_sub(kept);
        }

        (truncated, metrics)
    }

    /// Scan lines for error/warning patterns and return hazard counts.
    fn scan_hazards(lines: &[String]) -> HazardCounts {
        let mut hazards = HazardCounts::default();
        for line in lines {
            let lower = line.to_lowercase();
            if lower.contains("error:") || lower.contains("error[") || lower.starts_with("fatal:") || lower.starts_with("fatal ") {
                hazards.errors += 1;
            } else if lower.contains("warning:") || lower.contains("warning[") || lower.starts_with("warn:") || lower.starts_with("warn ") {
                hazards.warnings += 1;
            }
        }
        hazards
    }

    /// Flush remaining progress, block compressor, and dedup into output.
    fn flush_remaining(&mut self) {
        let remaining = self.progress.flush();
        for pr in remaining {
            if let ProgressResult::FinalState(text) = pr {
                let meta = VteStripper::strip(text.as_bytes());
                let compressed_lines = self.block_compressor.feed(meta.clean_text);
                for compressed in compressed_lines {
                    if self.config.dedup_all {
                        self.dedup.ingest(&compressed);
                    } else {
                        self.output.push(compressed);
                    }
                }
            }
        }
        // Flush any in-progress block at end of stream
        let block_flush = self.block_compressor.flush();
        for line in block_flush {
            if self.config.dedup_all {
                self.dedup.ingest(&line);
            } else {
                self.output.push(line);
            }
        }
        // Capture block compression metrics
        self.metrics.blocks_compressed = self.block_compressor.blocks_compressed;
        if self.config.dedup_all {
            self.dedup.flush_into(&mut self.output);
        }
    }

    /// Finalize the pipeline: flush progress, flush dedup, apply truncation.
    pub fn finalize(&mut self) -> Vec<String> {
        self.flush_remaining();
        let output = std::mem::take(&mut self.output);
        self.truncator.truncate(&output)
    }

    /// Process raw lines through category-aware dispatch.
    ///
    /// Dispatches based on category:
    /// - **Condense** — full pipeline: VTE strip -> progress removal -> dedup -> truncation
    /// - **Narrate** — VTE strip only (no dedup, no progress removal, no truncation)
    /// - **Structured** — VTE strip only
    /// - **Passthrough** — VTE strip only
    /// - **Interactive** — passthrough (no processing at all)
    /// - **Dangerous** — passthrough (no processing at all)
    ///
    /// Binary detection: if >10% of characters are U+FFFD (replacement char from
    /// invalid UTF-8), short-circuits with a `<binary output, N bytes>` marker.
    pub fn process(&mut self, raw_lines: Vec<Line>, category: Category) -> PipelineResult {
        // Binary detection: scan for high U+FFFD density
        if let Some(result) = self.check_binary(&raw_lines, category) {
            return result;
        }

        match category {
            Category::Condense => self.process_condense(raw_lines, category),
            Category::Narrate | Category::Structured | Category::Passthrough => {
                self.process_vte_strip_only(raw_lines, category)
            }
            Category::Interactive | Category::Dangerous => {
                self.process_raw_passthrough(raw_lines, category)
            }
        }
    }

    /// Check if input looks like binary data (>10% U+FFFD replacement chars).
    /// Returns a short-circuit PipelineResult if binary is detected.
    fn check_binary(&self, lines: &[Line], category: Category) -> Option<PipelineResult> {
        let mut total_chars: usize = 0;
        let mut replacement_chars: usize = 0;

        for line in lines {
            let text = match line {
                Line::Complete(s) | Line::Partial(s) | Line::Overwrite(s) => s,
            };
            for ch in text.chars() {
                total_chars += 1;
                if ch == '\u{FFFD}' {
                    replacement_chars += 1;
                }
            }
        }

        if total_chars == 0 {
            return None;
        }

        let ratio = replacement_chars as f64 / total_chars as f64;
        if ratio <= 0.10 {
            return None;
        }

        // Estimate byte count: each char is roughly 1 byte for ASCII,
        // but U+FFFD represents 1 original invalid byte each.
        let estimated_bytes = total_chars;

        Some(PipelineResult {
            output: vec![format!("<binary output, {} bytes>", estimated_bytes)],
            metrics: PipelineMetrics {
                lines_in: lines.len() as u64,
                lines_out: 1,
                binary_detected: true,
                ..Default::default()
            },
            category,
        })
    }

    /// Full condense pipeline: VTE strip -> progress -> dedup -> truncation.
    ///
    /// Detects content type first: if the output is a diff or high-entropy
    /// (>80% unique lines), dedup and truncation are skipped to avoid
    /// destroying high-value content like source code or diffs.
    fn process_condense(&mut self, raw_lines: Vec<Line>, category: Category) -> PipelineResult {
        let content_type = detect_content_type(&raw_lines);
        match content_type {
            ContentType::Normal => {
                for line in raw_lines {
                    self.feed(line);
                }
                let (output, metrics) = self.finalize_with_metrics();
                PipelineResult {
                    output,
                    metrics,
                    category,
                }
            }
            ContentType::Diff | ContentType::HighEntropy => {
                // Skip dedup and truncation — VTE strip + progress removal only
                self.process_vte_strip_only(raw_lines, category)
            }
        }
    }

    /// VTE strip only: strip ANSI codes but skip progress removal, dedup, and truncation.
    fn process_vte_strip_only(
        &mut self,
        raw_lines: Vec<Line>,
        category: Category,
    ) -> PipelineResult {
        let mut metrics = PipelineMetrics::default();
        let mut output = Vec::new();

        for line in raw_lines {
            metrics.lines_in += 1;
            let text = match line {
                Line::Complete(s) | Line::Partial(s) | Line::Overwrite(s) => s,
            };
            let meta = VteStripper::strip(text.as_bytes());
            if meta.clean_text != text {
                metrics.vte_stripped += 1;
            }
            output.push(meta.clean_text);
        }

        metrics.lines_out = output.len() as u64;

        PipelineResult {
            output,
            metrics,
            category,
        }
    }

    /// Check if this pipeline's content type would be high-entropy.
    /// Public for use by external callers that need the detection result.
    pub fn detect_content_type_for(lines: &[Line]) -> ContentType {
        detect_content_type(lines)
    }

    /// Raw passthrough: return lines unchanged, no processing at all.
    fn process_raw_passthrough(
        &mut self,
        raw_lines: Vec<Line>,
        category: Category,
    ) -> PipelineResult {
        let mut metrics = PipelineMetrics::default();
        let mut output = Vec::new();

        for line in raw_lines {
            metrics.lines_in += 1;
            let text = match line {
                Line::Complete(s) | Line::Partial(s) | Line::Overwrite(s) => s,
            };
            output.push(text);
        }

        metrics.lines_out = output.len() as u64;

        PipelineResult {
            output,
            metrics,
            category,
        }
    }
}

// ---------------------------------------------------------------------------
// Content-type detection
// ---------------------------------------------------------------------------

/// Minimum number of lines for high-entropy detection to activate.
/// Below this threshold, output is too small to reliably distinguish
/// source code from short build output.
const HIGH_ENTROPY_MIN_LINES: usize = 20;

/// Detect the content type of output lines to decide whether dedup should run.
///
/// - **Diff**: presence of `diff --git` or `--- a/` header AND >30% of lines
///   start with `+` or `-`
/// - **HighEntropy**: >80% unique templates after dedup-style normalization
///   (numbers, hashes, URLs, paths all collapsed). Only activates for 20+ lines.
/// - **Normal**: everything else (repetitive build/install output)
fn detect_content_type(lines: &[Line]) -> ContentType {
    if lines.is_empty() {
        return ContentType::Normal;
    }

    let texts: Vec<&str> = lines.iter().map(|l| match l {
        Line::Complete(s) | Line::Partial(s) | Line::Overwrite(s) => s.as_str(),
    }).collect();

    // Check for diff format
    let has_diff_header = texts.iter().any(|l|
        l.starts_with("diff --git") || l.starts_with("--- a/") || l.starts_with("--- b/")
    );
    if has_diff_header {
        let diff_lines = texts.iter().filter(|l|
            l.starts_with('+') || l.starts_with('-')
        ).count();
        if diff_lines as f64 / texts.len() as f64 > 0.30 {
            return ContentType::Diff;
        }
    }

    // Check for high entropy using template normalization.
    // This prevents false positives on repetitive build output like
    // "Downloading pkg1", "Downloading pkg2" which normalize to the same template.
    if texts.len() >= HIGH_ENTROPY_MIN_LINES {
        let dedup = DedupEngine::new();
        let mut unique_templates = std::collections::HashSet::new();
        for text in &texts {
            unique_templates.insert(dedup.tokenize(text));
        }
        if unique_templates.len() as f64 / texts.len() as f64 > 0.80 {
            return ContentType::HighEntropy;
        }
    }

    ContentType::Normal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pipeline_plain_text_passthrough() {
        let mut pipe = Pipeline::new(PipelineConfig::default());
        pipe.feed(Line::Complete("hello world".into()));
        pipe.feed(Line::Complete("goodbye world".into()));
        let result = pipe.finalize();
        assert!(result.contains(&"hello world".to_string()));
        assert!(result.contains(&"goodbye world".to_string()));
    }

    #[test]
    fn test_pipeline_strips_ansi() {
        let mut pipe = Pipeline::new(PipelineConfig::default());
        // Feed a line with ANSI color codes
        let ansi_line = "\x1b[31merror: something\x1b[0m";
        pipe.feed(Line::Complete(ansi_line.into()));
        let result = pipe.finalize();
        assert!(result.iter().any(|l| l == "error: something"));
    }

    #[test]
    fn test_pipeline_collapses_progress() {
        let mut pipe = Pipeline::new(PipelineConfig::default());
        pipe.feed(Line::Overwrite("10%".into()));
        pipe.feed(Line::Overwrite("20%".into()));
        pipe.feed(Line::Overwrite("30%".into()));
        pipe.feed(Line::Complete("done".into()));
        let result = pipe.finalize();
        // Progress overwrites should be stripped, only "done" passes through
        assert_eq!(result, vec!["done"]);
    }

    #[test]
    fn test_pipeline_dedup_repetitive_lines() {
        let mut pipe = Pipeline::new(PipelineConfig::default());
        for i in 0..10 {
            pipe.feed(Line::Complete(format!("Downloading https://registry.npmjs.org/pkg{}", i)));
        }
        let result = pipe.finalize();
        // Should be deduped — fewer output lines than input
        assert!(result.len() < 10);
        // Should contain a count marker
        assert!(result.iter().any(|l| l.contains("(x")));
    }

    #[test]
    fn test_pipeline_truncation() {
        let config = PipelineConfig {
            truncate: TruncateConfig { head: 2, tail: 2 },
            dedup_all: false, // disable dedup for this test
            ..Default::default()
        };
        let mut pipe = Pipeline::new(config);
        for i in 1..=20 {
            pipe.feed(Line::Complete(format!("unique line {}", i)));
        }
        let result = pipe.finalize();
        // Should be truncated: 2 head + marker + 2 tail = 5
        assert_eq!(result.len(), 5);
        assert_eq!(result[0], "unique line 1");
        assert_eq!(result[1], "unique line 2");
        assert!(result[2].contains("truncated"));
        assert_eq!(result[3], "unique line 19");
        assert_eq!(result[4], "unique line 20");
    }

    #[test]
    fn test_pipeline_empty_input() {
        let mut pipe = Pipeline::new(PipelineConfig::default());
        let result = pipe.finalize();
        assert!(result.is_empty());
    }

    // -----------------------------------------------------------------------
    // Metrics tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_finalize_returns_metrics() {
        let mut pipe = Pipeline::new(PipelineConfig::default());
        pipe.feed(Line::Complete("hello".into()));
        let (output, metrics) = pipe.finalize_with_metrics();
        assert!(!output.is_empty());
        assert_eq!(metrics.lines_in, 1);
        assert_eq!(metrics.lines_out, output.len() as u64);
    }

    #[test]
    fn test_metrics_vte_stripped_count() {
        let mut pipe = Pipeline::new(PipelineConfig::default());
        pipe.feed(Line::Complete("\x1b[31merror\x1b[0m".into())); // has ANSI
        pipe.feed(Line::Complete("plain text".into()));           // no ANSI
        pipe.feed(Line::Complete("\x1b[1mbold\x1b[0m".into()));   // has ANSI
        let (_, metrics) = pipe.finalize_with_metrics();
        assert_eq!(metrics.lines_in, 3);
        assert_eq!(metrics.vte_stripped, 2);
    }

    #[test]
    fn test_metrics_progress_stripped_count() {
        let mut pipe = Pipeline::new(PipelineConfig::default());
        pipe.feed(Line::Overwrite("10%".into()));
        pipe.feed(Line::Overwrite("20%".into()));
        pipe.feed(Line::Overwrite("30%".into()));
        pipe.feed(Line::Complete("done".into()));
        let (_, metrics) = pipe.finalize_with_metrics();
        assert_eq!(metrics.lines_in, 4);
        assert_eq!(metrics.progress_stripped, 3); // all overwrites return Stripped
    }

    #[test]
    fn test_metrics_dedup_counts() {
        let mut pipe = Pipeline::new(PipelineConfig::default());
        // Feed 10 similar lines → should form a dedup group
        for i in 0..10 {
            pipe.feed(Line::Complete(format!("Downloading https://registry.npmjs.org/pkg{}", i)));
        }
        // Feed 2 unique lines
        pipe.feed(Line::Complete("unique line A".into()));
        pipe.feed(Line::Complete("unique line B".into()));
        let (_, metrics) = pipe.finalize_with_metrics();
        assert_eq!(metrics.lines_in, 12);
        // The 10 similar lines should form 1 group with 9 absorbed
        assert!(metrics.dedup_groups >= 1, "expected at least 1 dedup group, got {}", metrics.dedup_groups);
        assert!(metrics.dedup_absorbed >= 1, "expected absorbed lines, got {}", metrics.dedup_absorbed);
    }

    #[test]
    fn test_metrics_oreo_suppressed() {
        let config = PipelineConfig {
            truncate: TruncateConfig { head: 2, tail: 2 },
            dedup_all: false, // disable dedup so all 20 lines hit truncation
            ..Default::default()
        };
        let mut pipe = Pipeline::new(config);
        for i in 1..=20 {
            pipe.feed(Line::Complete(format!("unique line {}", i)));
        }
        let (output, metrics) = pipe.finalize_with_metrics();
        // 20 lines in, truncated to 5 (2 head + marker + 2 tail)
        assert_eq!(metrics.lines_in, 20);
        assert_eq!(output.len(), 5);
        assert_eq!(metrics.oreo_suppressed, 16); // 20 - 4 visible = 16 lines hidden
    }

    #[test]
    fn test_metrics_no_truncation_no_suppressed() {
        let mut pipe = Pipeline::new(PipelineConfig::default()); // default head=50, tail=150
        pipe.feed(Line::Complete("small output".into()));
        let (_, metrics) = pipe.finalize_with_metrics();
        assert_eq!(metrics.oreo_suppressed, 0);
    }

    #[test]
    fn test_metrics_default_zeros() {
        let metrics = PipelineMetrics::default();
        assert_eq!(metrics.lines_in, 0);
        assert_eq!(metrics.lines_out, 0);
        assert_eq!(metrics.vte_stripped, 0);
        assert_eq!(metrics.progress_stripped, 0);
        assert_eq!(metrics.blocks_compressed, 0);
        assert_eq!(metrics.dedup_groups, 0);
        assert_eq!(metrics.dedup_absorbed, 0);
        assert_eq!(metrics.oreo_suppressed, 0);
    }

    #[test]
    fn test_pipeline_mixed_content() {
        let mut pipe = Pipeline::new(PipelineConfig::default());
        pipe.feed(Line::Complete("Starting build...".into()));
        pipe.feed(Line::Overwrite("compiling 1/10".into()));
        pipe.feed(Line::Overwrite("compiling 5/10".into()));
        pipe.feed(Line::Overwrite("compiling 10/10".into()));
        pipe.feed(Line::Complete("Build complete".into()));
        pipe.feed(Line::Complete("\x1b[33mwarning: unused variable\x1b[0m".into()));
        let result = pipe.finalize();
        // Should have: Starting build, Build complete, warning text
        assert!(result.iter().any(|l| l == "Starting build..."));
        assert!(result.iter().any(|l| l == "Build complete"));
        assert!(result.iter().any(|l| l == "warning: unused variable"));
        // Progress lines should be gone
        assert!(!result.iter().any(|l| l.contains("compiling")));
    }

    // -----------------------------------------------------------------------
    // Category-aware process() tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_process_condense_matches_existing_pipeline() {
        // process() with Condense should produce the same output as feed+finalize_with_metrics
        let lines = vec![
            Line::Complete("Starting build...".into()),
            Line::Overwrite("compiling 1/10".into()),
            Line::Overwrite("compiling 5/10".into()),
            Line::Overwrite("compiling 10/10".into()),
            Line::Complete("Build complete".into()),
            Line::Complete("\x1b[33mwarning: unused variable\x1b[0m".into()),
        ];

        // Existing pipeline path
        let mut pipe_old = Pipeline::new(PipelineConfig::default());
        for line in lines.clone() {
            pipe_old.feed(line);
        }
        let (old_output, old_metrics) = pipe_old.finalize_with_metrics();

        // New process() path
        let mut pipe_new = Pipeline::new(PipelineConfig::default());
        let result = pipe_new.process(lines, Category::Condense);

        assert_eq!(result.output, old_output);
        assert_eq!(result.metrics, old_metrics);
        assert_eq!(result.category, Category::Condense);
    }

    #[test]
    fn test_process_condense_with_dedup() {
        let mut lines = Vec::new();
        for i in 0..10 {
            lines.push(Line::Complete(format!(
                "Downloading https://registry.npmjs.org/pkg{}",
                i
            )));
        }
        lines.push(Line::Complete("unique line".into()));

        let mut pipe = Pipeline::new(PipelineConfig::default());
        let result = pipe.process(lines, Category::Condense);

        // Dedup should have kicked in — fewer output lines than input
        assert!(
            result.output.len() < 11,
            "expected dedup to reduce output, got {} lines",
            result.output.len()
        );
        assert!(result.output.iter().any(|l| l.contains("(x")));
        assert_eq!(result.category, Category::Condense);
        assert_eq!(result.metrics.lines_in, 11);
    }

    #[test]
    fn test_process_narrate_vte_strip_only() {
        let lines = vec![
            Line::Complete("\x1b[31merror: something\x1b[0m".into()),
            Line::Complete("plain text".into()),
            Line::Overwrite("progress 50%".into()),
            Line::Overwrite("progress 100%".into()),
            Line::Complete("done".into()),
        ];

        let mut pipe = Pipeline::new(PipelineConfig::default());
        let result = pipe.process(lines, Category::Narrate);

        // VTE strip should clean ANSI codes
        assert!(result.output.iter().any(|l| l == "error: something"));
        // No ANSI codes in output
        assert!(result
            .output
            .iter()
            .all(|l| !l.contains("\x1b")));
        // All lines kept (no dedup, no progress removal, no truncation)
        assert_eq!(result.output.len(), 5);
        assert_eq!(result.metrics.lines_in, 5);
        assert_eq!(result.metrics.lines_out, 5);
        assert_eq!(result.metrics.vte_stripped, 1); // only the ANSI line
        // No dedup, progress, or truncation metrics
        assert_eq!(result.metrics.dedup_groups, 0);
        assert_eq!(result.metrics.dedup_absorbed, 0);
        assert_eq!(result.metrics.progress_stripped, 0);
        assert_eq!(result.metrics.oreo_suppressed, 0);
        assert_eq!(result.category, Category::Narrate);
    }

    #[test]
    fn test_process_narrate_no_dedup() {
        // Narrate should NOT dedup, even with repetitive lines
        let mut lines = Vec::new();
        for i in 0..10 {
            lines.push(Line::Complete(format!(
                "Downloading https://registry.npmjs.org/pkg{}",
                i
            )));
        }

        let mut pipe = Pipeline::new(PipelineConfig::default());
        let result = pipe.process(lines, Category::Narrate);

        // All 10 lines should be preserved (no dedup)
        assert_eq!(result.output.len(), 10);
        assert_eq!(result.metrics.lines_out, 10);
        // No dedup marker
        assert!(!result.output.iter().any(|l| l.contains("(x")));
    }

    #[test]
    fn test_process_narrate_no_truncation() {
        // Narrate should NOT truncate, even with many lines
        let config = PipelineConfig {
            truncate: TruncateConfig { head: 2, tail: 2 },
            dedup_all: false,
            ..Default::default()
        };
        let mut lines = Vec::new();
        for i in 1..=20 {
            lines.push(Line::Complete(format!("unique line {}", i)));
        }

        let mut pipe = Pipeline::new(config);
        let result = pipe.process(lines, Category::Narrate);

        // All 20 lines should be preserved (no truncation)
        assert_eq!(result.output.len(), 20);
        assert_eq!(result.output[0], "unique line 1");
        assert_eq!(result.output[19], "unique line 20");
    }

    #[test]
    fn test_process_structured_vte_strip_only() {
        let lines = vec![
            Line::Complete("\x1b[32mM  src/main.rs\x1b[0m".into()),
            Line::Complete("?? new_file.txt".into()),
        ];

        let mut pipe = Pipeline::new(PipelineConfig::default());
        let result = pipe.process(lines, Category::Structured);

        assert_eq!(result.output.len(), 2);
        assert_eq!(result.output[0], "M  src/main.rs"); // ANSI stripped
        assert_eq!(result.output[1], "?? new_file.txt"); // plain preserved
        assert_eq!(result.metrics.vte_stripped, 1);
        assert_eq!(result.category, Category::Structured);
    }

    #[test]
    fn test_process_passthrough_vte_strip_only() {
        let lines = vec![
            Line::Complete("\x1b[1;34mheader\x1b[0m".into()),
            Line::Complete("content line 1".into()),
            Line::Complete("content line 2".into()),
        ];

        let mut pipe = Pipeline::new(PipelineConfig::default());
        let result = pipe.process(lines, Category::Passthrough);

        assert_eq!(result.output.len(), 3);
        assert_eq!(result.output[0], "header"); // ANSI stripped
        assert_eq!(result.output[1], "content line 1");
        assert_eq!(result.output[2], "content line 2");
        assert_eq!(result.metrics.vte_stripped, 1);
        assert_eq!(result.category, Category::Passthrough);
    }

    #[test]
    fn test_process_interactive_raw_passthrough() {
        // Interactive should return raw lines unchanged — no VTE strip
        let ansi_line = "\x1b[31merror: something\x1b[0m";
        let lines = vec![
            Line::Complete(ansi_line.into()),
            Line::Complete("plain text".into()),
            Line::Overwrite("progress".into()),
        ];

        let mut pipe = Pipeline::new(PipelineConfig::default());
        let result = pipe.process(lines, Category::Interactive);

        assert_eq!(result.output.len(), 3);
        // ANSI codes preserved — no stripping
        assert_eq!(result.output[0], ansi_line);
        assert_eq!(result.output[1], "plain text");
        assert_eq!(result.output[2], "progress");
        // No VTE strip metrics
        assert_eq!(result.metrics.vte_stripped, 0);
        assert_eq!(result.metrics.lines_in, 3);
        assert_eq!(result.metrics.lines_out, 3);
        assert_eq!(result.category, Category::Interactive);
    }

    #[test]
    fn test_process_dangerous_raw_passthrough() {
        // Dangerous should return raw lines unchanged — no VTE strip
        let ansi_line = "\x1b[33mwarning: destructive\x1b[0m";
        let lines = vec![
            Line::Complete(ansi_line.into()),
            Line::Complete("removing files...".into()),
        ];

        let mut pipe = Pipeline::new(PipelineConfig::default());
        let result = pipe.process(lines, Category::Dangerous);

        assert_eq!(result.output.len(), 2);
        // ANSI codes preserved
        assert_eq!(result.output[0], ansi_line);
        assert_eq!(result.output[1], "removing files...");
        assert_eq!(result.metrics.vte_stripped, 0);
        assert_eq!(result.category, Category::Dangerous);
    }

    #[test]
    fn test_process_metrics_populated_for_all_categories() {
        let lines = vec![
            Line::Complete("line 1".into()),
            Line::Complete("line 2".into()),
            Line::Complete("line 3".into()),
        ];

        let categories = vec![
            Category::Condense,
            Category::Narrate,
            Category::Structured,
            Category::Passthrough,
            Category::Interactive,
            Category::Dangerous,
        ];

        for cat in categories {
            let mut pipe = Pipeline::new(PipelineConfig::default());
            let result = pipe.process(lines.clone(), cat);

            assert_eq!(
                result.metrics.lines_in, 3,
                "lines_in should be 3 for {:?}",
                cat
            );
            assert!(
                result.metrics.lines_out > 0,
                "lines_out should be > 0 for {:?}",
                cat
            );
            assert_eq!(
                result.category, cat,
                "category should match for {:?}",
                cat
            );
        }
    }

    #[test]
    fn test_process_empty_input() {
        let categories = vec![
            Category::Condense,
            Category::Narrate,
            Category::Passthrough,
            Category::Interactive,
        ];

        for cat in categories {
            let mut pipe = Pipeline::new(PipelineConfig::default());
            let result = pipe.process(vec![], cat);

            assert!(
                result.output.is_empty(),
                "empty input should produce empty output for {:?}",
                cat
            );
            assert_eq!(result.metrics.lines_in, 0);
            assert_eq!(result.metrics.lines_out, 0);
        }
    }

    // -----------------------------------------------------------------------
    // Binary detection tests
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Enriched truncation marker tests (d3)
    // -----------------------------------------------------------------------

    #[test]
    fn test_enriched_truncation_shows_error_count() {
        let config = PipelineConfig {
            truncate: TruncateConfig { head: 2, tail: 2 },
            dedup_all: false, // disable dedup to control exact lines
            ..Default::default()
        };
        let mut pipe = Pipeline::new(config);

        // Build 20 lines: head(2) + middle(16 with errors/warnings) + tail(2)
        pipe.feed(Line::Complete("line 1 — head".into()));
        pipe.feed(Line::Complete("line 2 — head".into()));
        // Middle section — these will be truncated
        pipe.feed(Line::Complete("error: undefined function 'foo'".into()));
        pipe.feed(Line::Complete("warning: unused variable".into()));
        pipe.feed(Line::Complete("normal output".into()));
        pipe.feed(Line::Complete("Error: cannot find module".into()));
        pipe.feed(Line::Complete("WARNING: deprecated API".into()));
        for i in 0..9 {
            pipe.feed(Line::Complete(format!("normal middle line {i}")));
        }
        // Tail
        pipe.feed(Line::Complete("line 19 — tail".into()));
        pipe.feed(Line::Complete("line 20 — tail".into()));

        let (output, _metrics) = pipe.finalize_with_metrics();

        // Should be truncated: head(2) + marker(1) + tail(2) = 5
        assert_eq!(output.len(), 5);
        let marker = &output[2];
        // Marker should mention errors and warnings in hidden region
        assert!(
            marker.contains("error"),
            "marker should mention errors, got: {marker}"
        );
        assert!(
            marker.contains("warning"),
            "marker should mention warnings, got: {marker}"
        );
    }

    #[test]
    fn test_enriched_truncation_no_hazards_simple_marker() {
        let config = PipelineConfig {
            truncate: TruncateConfig { head: 2, tail: 2 },
            dedup_all: false,
            ..Default::default()
        };
        let mut pipe = Pipeline::new(config);

        for i in 1..=20 {
            pipe.feed(Line::Complete(format!("clean line {i}")));
        }

        let (output, _) = pipe.finalize_with_metrics();
        assert_eq!(output.len(), 5);
        let marker = &output[2];
        // No errors or warnings → simple marker
        assert!(
            !marker.contains("error"),
            "marker should NOT mention errors, got: {marker}"
        );
        assert!(
            !marker.contains("warning"),
            "marker should NOT mention warnings, got: {marker}"
        );
        assert!(marker.contains("truncated"));
    }

    #[test]
    fn test_enriched_truncation_via_process() {
        // Verify enriched markers work through the process() path too
        let config = PipelineConfig {
            truncate: TruncateConfig { head: 2, tail: 2 },
            dedup_all: false,
            ..Default::default()
        };

        let mut lines = vec![
            Line::Complete("head 1".into()),
            Line::Complete("head 2".into()),
        ];
        // Middle with hazards
        lines.push(Line::Complete("error: something failed".into()));
        lines.push(Line::Complete("warning: something deprecated".into()));
        for i in 0..12 {
            lines.push(Line::Complete(format!("middle {i}")));
        }
        // Tail
        lines.push(Line::Complete("tail 1".into()));
        lines.push(Line::Complete("tail 2".into()));

        let mut pipe = Pipeline::new(config);
        let result = pipe.process(lines, Category::Condense);

        assert_eq!(result.output.len(), 5);
        let marker = &result.output[2];
        assert!(
            marker.contains("error"),
            "process() marker should mention errors, got: {marker}"
        );
        assert!(
            marker.contains("warning"),
            "process() marker should mention warnings, got: {marker}"
        );
    }

    // -----------------------------------------------------------------------
    // Binary detection tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_binary_detection_random_bytes() {
        // Simulate 1KB of random binary data that went through Utf8Decoder.
        // Each invalid byte becomes U+FFFD. Random bytes produce ~50% FFFD.
        let mut binary_content = String::new();
        for _ in 0..512 {
            binary_content.push('\u{FFFD}');
        }
        for _ in 0..512 {
            binary_content.push('a'); // some valid chars too
        }

        let lines = vec![Line::Complete(binary_content)];
        let mut pipe = Pipeline::new(PipelineConfig::default());
        let result = pipe.process(lines, Category::Condense);

        // Should detect binary and short-circuit
        assert!(result.metrics.binary_detected, "should detect binary output");
        assert_eq!(result.output.len(), 1);
        assert!(
            result.output[0].contains("binary output"),
            "should contain binary marker, got: {}",
            result.output[0]
        );
        assert!(
            result.output[0].contains("1024"),
            "should mention byte count, got: {}",
            result.output[0]
        );
    }

    #[test]
    fn test_binary_detection_normal_text_not_triggered() {
        let lines = vec![
            Line::Complete("Compiling mish v0.1.0".into()),
            Line::Complete("warning: unused variable".into()),
            Line::Complete("Finished dev profile".into()),
        ];

        let mut pipe = Pipeline::new(PipelineConfig::default());
        let result = pipe.process(lines, Category::Condense);

        assert!(!result.metrics.binary_detected, "normal text should not trigger binary detection");
        assert!(!result.output.is_empty(), "should produce output");
    }

    #[test]
    fn test_binary_detection_threshold_just_below() {
        // 9% replacement chars — just below the 10% threshold
        let mut content = String::new();
        for _ in 0..9 {
            content.push('\u{FFFD}');
        }
        for _ in 0..91 {
            content.push('x');
        }

        let lines = vec![Line::Complete(content)];
        let mut pipe = Pipeline::new(PipelineConfig::default());
        let result = pipe.process(lines, Category::Condense);

        assert!(!result.metrics.binary_detected, "9% FFFD should not trigger binary detection");
    }

    #[test]
    fn test_binary_detection_threshold_just_above() {
        // 11% replacement chars — above the 10% threshold
        let mut content = String::new();
        for _ in 0..11 {
            content.push('\u{FFFD}');
        }
        for _ in 0..89 {
            content.push('x');
        }

        let lines = vec![Line::Complete(content)];
        let mut pipe = Pipeline::new(PipelineConfig::default());
        let result = pipe.process(lines, Category::Condense);

        assert!(result.metrics.binary_detected, "11% FFFD should trigger binary detection");
    }

    #[test]
    fn test_binary_detection_works_for_all_categories() {
        // Binary detection should work across all category paths
        let mut binary_content = String::new();
        for _ in 0..200 {
            binary_content.push('\u{FFFD}');
        }

        let categories = vec![
            Category::Condense,
            Category::Narrate,
            Category::Structured,
            Category::Passthrough,
            Category::Interactive,
            Category::Dangerous,
        ];

        for cat in categories {
            let lines = vec![Line::Complete(binary_content.clone())];
            let mut pipe = Pipeline::new(PipelineConfig::default());
            let result = pipe.process(lines, cat);

            assert!(
                result.metrics.binary_detected,
                "binary detection should work for {:?}",
                cat
            );
            assert_eq!(
                result.output.len(),
                1,
                "binary output should be single marker for {:?}",
                cat
            );
            assert!(
                result.output[0].contains("binary output"),
                "should contain binary marker for {:?}, got: {}",
                cat,
                result.output[0]
            );
        }
    }
}
