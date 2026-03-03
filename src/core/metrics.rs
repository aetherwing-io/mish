// SquasherReport — aggregate struct for squasher metrics + timing + sizes.
// Produced by both the condense handler (Pipeline path) and sh_run (either path).

use crate::squasher::pipeline::PipelineMetrics;
use crate::core::emit::EmitMetrics;

#[derive(Debug, Clone)]
pub enum SquasherPath {
    Pipeline(PipelineMetrics),
    Emit(EmitMetrics),
}

#[derive(Debug, Clone)]
pub struct SquasherReport {
    pub path_metrics: SquasherPath,
    pub wall_ms: u64,
    pub squash_ms: u64,
    pub grammar_load_ms: u64,
    pub raw_bytes: u64,
    pub squashed_bytes: u64,
    pub compression_ratio: f64,
}

fn compute_ratio(raw: u64, squashed: u64) -> f64 {
    if raw == 0 {
        1.0
    } else {
        squashed as f64 / raw as f64
    }
}

impl SquasherReport {
    pub fn from_pipeline(
        metrics: PipelineMetrics,
        wall_ms: u64,
        squash_ms: u64,
        grammar_load_ms: u64,
        raw_bytes: u64,
        squashed_bytes: u64,
    ) -> Self {
        Self {
            path_metrics: SquasherPath::Pipeline(metrics),
            wall_ms,
            squash_ms,
            grammar_load_ms,
            raw_bytes,
            squashed_bytes,
            compression_ratio: compute_ratio(raw_bytes, squashed_bytes),
        }
    }

    pub fn from_emit(
        metrics: EmitMetrics,
        wall_ms: u64,
        squash_ms: u64,
        grammar_load_ms: u64,
        raw_bytes: u64,
        squashed_bytes: u64,
    ) -> Self {
        Self {
            path_metrics: SquasherPath::Emit(metrics),
            wall_ms,
            squash_ms,
            grammar_load_ms,
            raw_bytes,
            squashed_bytes,
            compression_ratio: compute_ratio(raw_bytes, squashed_bytes),
        }
    }
}

impl Default for SquasherReport {
    fn default() -> Self {
        Self {
            path_metrics: SquasherPath::Pipeline(PipelineMetrics::default()),
            wall_ms: 0,
            squash_ms: 0,
            grammar_load_ms: 0,
            raw_bytes: 0,
            squashed_bytes: 0,
            compression_ratio: 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::squasher::pipeline::PipelineMetrics;
    use crate::core::emit::EmitMetrics;

    #[test]
    fn report_from_pipeline_metrics() {
        let pm = PipelineMetrics {
            lines_in: 100,
            lines_out: 20,
            vte_stripped: 5,
            progress_stripped: 3,
            dedup_groups: 2,
            dedup_absorbed: 10,
            oreo_suppressed: 60,
            binary_detected: false,
        };
        let report = SquasherReport::from_pipeline(pm, 500, 120, 15, 8000, 1600);

        assert!(matches!(report.path_metrics, SquasherPath::Pipeline(_)));
        assert_eq!(report.wall_ms, 500);
        assert_eq!(report.squash_ms, 120);
        assert_eq!(report.grammar_load_ms, 15);
        assert_eq!(report.raw_bytes, 8000);
        assert_eq!(report.squashed_bytes, 1600);
        assert!((report.compression_ratio - 0.2).abs() < f64::EPSILON);
    }

    #[test]
    fn report_from_emit_metrics() {
        let em = EmitMetrics {
            noise_lines: 50,
            signal_lines: 10,
            outcome_lines: 5,
            unclassified_lines: 35,
        };
        let report = SquasherReport::from_emit(em, 300, 80, 10, 4000, 1000);

        assert!(matches!(report.path_metrics, SquasherPath::Emit(_)));
        assert_eq!(report.wall_ms, 300);
        assert_eq!(report.squash_ms, 80);
        assert_eq!(report.grammar_load_ms, 10);
        assert_eq!(report.raw_bytes, 4000);
        assert_eq!(report.squashed_bytes, 1000);
        assert!((report.compression_ratio - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn compression_ratio_zero_raw_bytes() {
        let pm = PipelineMetrics::default();
        let report = SquasherReport::from_pipeline(pm, 0, 0, 0, 0, 0);

        // When raw_bytes is 0, ratio should be 1.0 (no compression)
        assert!((report.compression_ratio - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn compression_ratio_no_reduction() {
        let pm = PipelineMetrics::default();
        let report = SquasherReport::from_pipeline(pm, 100, 50, 5, 500, 500);

        // Same size in and out = ratio 1.0
        assert!((report.compression_ratio - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn pipeline_metrics_accessible() {
        let pm = PipelineMetrics {
            lines_in: 42,
            lines_out: 10,
            vte_stripped: 0,
            progress_stripped: 0,
            dedup_groups: 0,
            dedup_absorbed: 0,
            oreo_suppressed: 0,
            binary_detected: false,
        };
        let report = SquasherReport::from_pipeline(pm, 0, 0, 0, 0, 0);

        if let SquasherPath::Pipeline(ref m) = report.path_metrics {
            assert_eq!(m.lines_in, 42);
            assert_eq!(m.lines_out, 10);
        } else {
            panic!("expected Pipeline path");
        }
    }

    #[test]
    fn emit_metrics_accessible() {
        let em = EmitMetrics {
            noise_lines: 7,
            signal_lines: 3,
            outcome_lines: 1,
            unclassified_lines: 0,
        };
        let report = SquasherReport::from_emit(em, 0, 0, 0, 0, 0);

        if let SquasherPath::Emit(ref m) = report.path_metrics {
            assert_eq!(m.noise_lines, 7);
            assert_eq!(m.signal_lines, 3);
        } else {
            panic!("expected Emit path");
        }
    }

    #[test]
    fn default_report_has_sane_values() {
        let report = SquasherReport::default();

        assert!(matches!(report.path_metrics, SquasherPath::Pipeline(_)));
        assert_eq!(report.wall_ms, 0);
        assert_eq!(report.squash_ms, 0);
        assert_eq!(report.grammar_load_ms, 0);
        assert_eq!(report.raw_bytes, 0);
        assert_eq!(report.squashed_bytes, 0);
        assert!((report.compression_ratio - 1.0).abs() < f64::EPSILON);
    }
}
