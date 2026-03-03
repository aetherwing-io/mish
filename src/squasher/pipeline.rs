/// Squasher pipeline orchestration.
///
/// VTE strip -> progress removal -> dedup -> truncation -> output

use crate::core::line_buffer::Line;
use crate::router::categories::Category;
use crate::squasher::dedup::DedupEngine;
use crate::squasher::progress::{ProgressFilter, ProgressResult};
use crate::squasher::truncate::{TruncateConfig, Truncator};
use crate::squasher::vte_strip::VteStripper;

/// Metrics collected during pipeline processing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PipelineMetrics {
    pub lines_in: u64,
    pub lines_out: u64,
    pub vte_stripped: u64,
    pub progress_stripped: u64,
    pub dedup_groups: u64,
    pub dedup_absorbed: u64,
    pub oreo_suppressed: u64,
}

/// Configuration for the squasher pipeline.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    pub truncate: TruncateConfig,
    /// If true, run dedup on all lines (not just noise-classified ones)
    pub dedup_all: bool,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            truncate: TruncateConfig::default(),
            dedup_all: true,
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
    dedup: DedupEngine,
    truncator: Truncator,
    config: PipelineConfig,
    /// Accumulated clean output lines
    output: Vec<String>,
    metrics: PipelineMetrics,
}

impl Pipeline {
    pub fn new(config: PipelineConfig) -> Self {
        Self {
            progress: ProgressFilter::new(),
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

                    // Stage 3: Dedup (if enabled)
                    if self.config.dedup_all {
                        self.dedup.ingest(&clean);
                    } else {
                        self.output.push(clean);
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

        // Stage 4: Truncation
        let output = std::mem::take(&mut self.output);
        let truncated = self.truncator.truncate(&output);

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

    /// Flush remaining progress and dedup into output (shared by finalize paths).
    fn flush_remaining(&mut self) {
        let remaining = self.progress.flush();
        for pr in remaining {
            if let ProgressResult::FinalState(text) = pr {
                let meta = VteStripper::strip(text.as_bytes());
                if self.config.dedup_all {
                    self.dedup.ingest(&meta.clean_text);
                } else {
                    self.output.push(meta.clean_text);
                }
            }
        }
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
    pub fn process(&mut self, raw_lines: Vec<Line>, category: Category) -> PipelineResult {
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

    /// Full condense pipeline: VTE strip -> progress -> dedup -> truncation.
    fn process_condense(&mut self, raw_lines: Vec<Line>, category: Category) -> PipelineResult {
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
}
