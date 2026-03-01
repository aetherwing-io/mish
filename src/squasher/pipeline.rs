/// Squasher pipeline orchestration.
///
/// VTE strip -> progress removal -> dedup -> truncation -> output

use crate::core::line_buffer::Line;
use crate::squasher::dedup::DedupEngine;
use crate::squasher::progress::{ProgressFilter, ProgressResult};
use crate::squasher::truncate::{TruncateConfig, Truncator};
use crate::squasher::vte_strip::VteStripper;

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
}

impl Pipeline {
    pub fn new(config: PipelineConfig) -> Self {
        Self {
            progress: ProgressFilter::new(),
            dedup: DedupEngine::new(),
            truncator: Truncator::new(config.truncate.clone()),
            config,
            output: Vec::new(),
        }
    }

    /// Feed a line through the pipeline stages.
    pub fn feed(&mut self, line: Line) {
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

                    // Stage 3: Dedup (if enabled)
                    if self.config.dedup_all {
                        self.dedup.ingest(&clean);
                    } else {
                        self.output.push(clean);
                    }
                }
                ProgressResult::Stripped => {
                    // Progress line — already collapsed, nothing to emit
                }
                ProgressResult::FinalState(text) => {
                    let meta = VteStripper::strip(text.as_bytes());
                    self.output.push(meta.clean_text);
                }
            }
        }
    }

    /// Finalize the pipeline: flush progress, flush dedup, apply truncation.
    pub fn finalize(&mut self) -> Vec<String> {
        // Flush remaining progress
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

        // Flush dedup groups
        if self.config.dedup_all {
            self.dedup.flush_into(&mut self.output);
        }

        // Stage 4: Truncation
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
        assert!(result[2].contains("suppressed"));
        assert_eq!(result[3], "unique line 19");
        assert_eq!(result[4], "unique line 20");
    }

    #[test]
    fn test_pipeline_empty_input() {
        let mut pipe = Pipeline::new(PipelineConfig::default());
        let result = pipe.finalize();
        assert!(result.is_empty());
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
