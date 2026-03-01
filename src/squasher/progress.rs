/// Progress bar detection and removal.
///
/// Detects CR-based overwrite patterns and collapses spinner/progress frames.
/// Works on already-assembled Line types from the line buffer.

use crate::core::line_buffer::Line;

/// Result of filtering a line through the progress detector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressResult {
    /// Not a progress line — pass through
    Pass(Line),
    /// Detected as progress — strip it
    Stripped,
    /// Final state of a progress sequence (last overwrite before a Complete line)
    FinalState(String),
}

/// Detects and collapses progress bar / spinner sequences.
///
/// Overwrite lines (CR without LF) are accumulated. When a Complete line
/// arrives after a series of Overwrites, only the final state is emitted.
pub struct ProgressFilter {
    pending_overwrites: Vec<String>,
}

impl ProgressFilter {
    pub fn new() -> Self {
        Self {
            pending_overwrites: Vec::new(),
        }
    }

    /// Feed a line into the progress filter.
    ///
    /// Overwrite lines are accumulated and stripped. When a Complete line
    /// arrives after overwrites, the overwrites are discarded (collapsed)
    /// and only the Complete line passes through.
    pub fn feed(&mut self, line: Line) -> Vec<ProgressResult> {
        match line {
            Line::Overwrite(text) => {
                self.pending_overwrites.push(text);
                vec![ProgressResult::Stripped]
            }
            Line::Complete(text) => {
                // If we had pending overwrites, discard them
                self.pending_overwrites.clear();
                vec![ProgressResult::Pass(Line::Complete(text))]
            }
            Line::Partial(text) => {
                // Partials pass through; if we had pending overwrites, flush them
                self.pending_overwrites.clear();
                vec![ProgressResult::Pass(Line::Partial(text))]
            }
        }
    }

    /// Flush any pending overwrites at end of stream.
    /// Returns the final overwrite state if any exist.
    pub fn flush(&mut self) -> Vec<ProgressResult> {
        if self.pending_overwrites.is_empty() {
            return Vec::new();
        }
        let last = self.pending_overwrites.last().cloned().unwrap();
        self.pending_overwrites.clear();
        vec![ProgressResult::FinalState(last)]
    }
}

impl Default for ProgressFilter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_complete_line_passthrough() {
        let mut pf = ProgressFilter::new();
        let results = pf.feed(Line::Complete("hello world".into()));
        assert_eq!(results, vec![ProgressResult::Pass(Line::Complete("hello world".into()))]);
    }

    #[test]
    fn test_single_overwrite_stripped() {
        let mut pf = ProgressFilter::new();
        let results = pf.feed(Line::Overwrite("50%".into()));
        assert_eq!(results, vec![ProgressResult::Stripped]);
    }

    #[test]
    fn test_overwrite_sequence_then_complete() {
        let mut pf = ProgressFilter::new();
        // Progress bar: 10%, 20%, 30%, done
        assert_eq!(pf.feed(Line::Overwrite("10%".into())), vec![ProgressResult::Stripped]);
        assert_eq!(pf.feed(Line::Overwrite("20%".into())), vec![ProgressResult::Stripped]);
        assert_eq!(pf.feed(Line::Overwrite("30%".into())), vec![ProgressResult::Stripped]);
        let results = pf.feed(Line::Complete("done".into()));
        // Should get the complete line (overwrites were collapsed)
        assert_eq!(results, vec![ProgressResult::Pass(Line::Complete("done".into()))]);
    }

    #[test]
    fn test_overwrite_then_complete_emits_nothing_extra() {
        let mut pf = ProgressFilter::new();
        pf.feed(Line::Overwrite("downloading...".into()));
        pf.feed(Line::Overwrite("downloading... 50%".into()));
        let results = pf.feed(Line::Complete("downloaded".into()));
        // Just the final complete line
        assert_eq!(results, vec![ProgressResult::Pass(Line::Complete("downloaded".into()))]);
    }

    #[test]
    fn test_partial_line_passthrough() {
        let mut pf = ProgressFilter::new();
        let results = pf.feed(Line::Partial("prompt> ".into()));
        assert_eq!(results, vec![ProgressResult::Pass(Line::Partial("prompt> ".into()))]);
    }

    #[test]
    fn test_flush_pending_overwrites() {
        let mut pf = ProgressFilter::new();
        pf.feed(Line::Overwrite("spinner |".into()));
        pf.feed(Line::Overwrite("spinner /".into()));
        // Flush at end — emit final state
        let results = pf.flush();
        assert_eq!(results, vec![ProgressResult::FinalState("spinner /".into())]);
    }

    #[test]
    fn test_flush_empty() {
        let mut pf = ProgressFilter::new();
        let results = pf.flush();
        assert!(results.is_empty());
    }

    #[test]
    fn test_interleaved_overwrites_and_completes() {
        let mut pf = ProgressFilter::new();
        // First progress sequence
        pf.feed(Line::Overwrite("10%".into()));
        let r1 = pf.feed(Line::Complete("step 1 done".into()));
        assert_eq!(r1, vec![ProgressResult::Pass(Line::Complete("step 1 done".into()))]);

        // Second progress sequence
        pf.feed(Line::Overwrite("20%".into()));
        let r2 = pf.feed(Line::Complete("step 2 done".into()));
        assert_eq!(r2, vec![ProgressResult::Pass(Line::Complete("step 2 done".into()))]);
    }
}
