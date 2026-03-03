/// Squasher pipeline orchestration.
///
/// VTE strip -> progress removal -> dedup -> truncation -> output

use crate::core::line_buffer::Line;
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
}
