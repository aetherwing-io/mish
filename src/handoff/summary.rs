//! Credential-blind summary generation for operator handoffs.
//!
//! When an operator detaches from a handoff, the server returns a summary
//! to the LLM. By default, this summary is **credential-blind**: it contains
//! only duration, line count, and outcome — no process output. This prevents
//! secrets (passwords, MFA codes, tokens) from leaking into LLM context.
//!
//! Opt-in: `mish attach --share-output` includes the process output captured
//! during the handoff period.

use serde::Serialize;

use super::state::HandoffSummary;

/// Extended summary with optional output content.
///
/// This is what gets serialized and returned to the MCP response.
/// The `output` field is `None` by default (credential-blind mode)
/// and only populated when `--share-output` is used.
#[derive(Debug, Clone, Serialize)]
pub struct HandoffReport {
    /// Summary of the handoff (always present).
    pub handoff_resolved: HandoffReportInner,
}

/// Inner structure of the handoff report.
#[derive(Debug, Clone, Serialize)]
pub struct HandoffReportInner {
    /// Wall-clock duration of the handoff in milliseconds.
    pub duration_ms: u64,
    /// Number of output lines produced during the handoff.
    pub lines_during_handoff: usize,
    /// Outcome: "resolved", "process_exited", or "timed_out".
    pub outcome: String,
    /// Process output during handoff (only present with --share-output).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// Generate a credential-blind handoff report from a HandoffSummary.
///
/// No output content is included. Only metadata: duration, line count, outcome.
pub fn credential_blind_report(summary: &HandoffSummary) -> HandoffReport {
    HandoffReport {
        handoff_resolved: HandoffReportInner {
            duration_ms: summary.duration_ms,
            lines_during_handoff: summary.lines_during_handoff,
            outcome: summary.outcome.clone(),
            output: None,
        },
    }
}

/// Generate a handoff report with process output included (opt-in mode).
///
/// Used when the operator runs `mish attach --share-output`.
pub fn shared_output_report(summary: &HandoffSummary, output: String) -> HandoffReport {
    HandoffReport {
        handoff_resolved: HandoffReportInner {
            duration_ms: summary.duration_ms,
            lines_during_handoff: summary.lines_during_handoff,
            outcome: summary.outcome.clone(),
            output: Some(output),
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_summary(outcome: &str) -> HandoffSummary {
        HandoffSummary {
            duration_ms: 1234,
            lines_during_handoff: 42,
            outcome: outcome.to_string(),
        }
    }

    // ── Test 1: Credential-blind report has no output content ──

    #[test]
    fn credential_blind_has_no_output() {
        let summary = make_summary("resolved");
        let report = credential_blind_report(&summary);

        assert_eq!(report.handoff_resolved.duration_ms, 1234);
        assert_eq!(report.handoff_resolved.lines_during_handoff, 42);
        assert_eq!(report.handoff_resolved.outcome, "resolved");
        assert!(report.handoff_resolved.output.is_none());
    }

    // ── Test 2: Credential-blind JSON omits output field ──

    #[test]
    fn credential_blind_json_omits_output() {
        let summary = make_summary("resolved");
        let report = credential_blind_report(&summary);
        let json = serde_json::to_string(&report).unwrap();

        assert!(!json.contains("output"), "JSON should not contain 'output' field, got: {json}");
        assert!(json.contains("duration_ms"));
        assert!(json.contains("lines_during_handoff"));
        assert!(json.contains("resolved"));
    }

    // ── Test 3: Credential-blind with process_exited outcome ──

    #[test]
    fn credential_blind_process_exited() {
        let summary = make_summary("process_exited");
        let report = credential_blind_report(&summary);

        assert_eq!(report.handoff_resolved.outcome, "process_exited");
        assert!(report.handoff_resolved.output.is_none());
    }

    // ── Test 4: Shared output report includes output content ──

    #[test]
    fn shared_output_includes_content() {
        let summary = make_summary("resolved");
        let output = "Login successful\nWelcome user!".to_string();
        let report = shared_output_report(&summary, output);

        assert_eq!(report.handoff_resolved.duration_ms, 1234);
        assert_eq!(report.handoff_resolved.lines_during_handoff, 42);
        assert_eq!(report.handoff_resolved.outcome, "resolved");
        assert_eq!(
            report.handoff_resolved.output.as_deref(),
            Some("Login successful\nWelcome user!")
        );
    }

    // ── Test 5: Shared output JSON contains output field ──

    #[test]
    fn shared_output_json_includes_output() {
        let summary = make_summary("resolved");
        let report = shared_output_report(&summary, "some output".to_string());
        let json = serde_json::to_string(&report).unwrap();

        assert!(json.contains("\"output\""), "JSON should contain 'output' field, got: {json}");
        assert!(json.contains("some output"));
    }

    // ── Test 6: Shared output with process_exited ──

    #[test]
    fn shared_output_process_exited() {
        let summary = make_summary("process_exited");
        let report = shared_output_report(&summary, "exit code: 1".to_string());

        assert_eq!(report.handoff_resolved.outcome, "process_exited");
        assert_eq!(
            report.handoff_resolved.output.as_deref(),
            Some("exit code: 1")
        );
    }

    // ── Test 7: Empty output string is still included with --share-output ──

    #[test]
    fn shared_output_empty_string() {
        let summary = make_summary("resolved");
        let report = shared_output_report(&summary, String::new());

        // Empty string is still Some("") — it's explicitly shared
        assert_eq!(report.handoff_resolved.output.as_deref(), Some(""));
    }

    // ── Test 8: JSON round-trip preserves all fields ──

    #[test]
    fn json_roundtrip() {
        let summary = make_summary("resolved");
        let report = credential_blind_report(&summary);
        let json = serde_json::to_string(&report).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["handoff_resolved"]["duration_ms"], 1234);
        assert_eq!(parsed["handoff_resolved"]["lines_during_handoff"], 42);
        assert_eq!(parsed["handoff_resolved"]["outcome"], "resolved");
        assert!(parsed["handoff_resolved"]["output"].is_null() || !parsed["handoff_resolved"].as_object().unwrap().contains_key("output"));
    }

    // ── Test 9: Duration and line count values preserved exactly ──

    #[test]
    fn values_preserved() {
        let summary = HandoffSummary {
            duration_ms: 99999,
            lines_during_handoff: 0,
            outcome: "timed_out".to_string(),
        };
        let report = credential_blind_report(&summary);

        assert_eq!(report.handoff_resolved.duration_ms, 99999);
        assert_eq!(report.handoff_resolved.lines_during_handoff, 0);
        assert_eq!(report.handoff_resolved.outcome, "timed_out");
    }
}
