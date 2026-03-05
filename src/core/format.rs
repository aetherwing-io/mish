//! Output formatting.
//!
//! Modes: human (default), json, passthrough, context.
//! Provides status symbols and mode-specific renderers for command results.

use serde::Serialize;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Output mode controlling how results are displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputMode {
    /// Status symbols + indented body (default).
    #[default]
    Human,
    /// Structured JSON envelope.
    Json,
    /// Raw output + mish summary footer.
    Passthrough,
    /// Ultra-compressed single line for LLM context.
    Context,
}

/// A hazard/warning from command output.
#[derive(Debug, Clone, Serialize)]
pub struct HazardEntry {
    pub severity: String,
    pub text: String,
}

/// An enrichment diagnostic line from error analysis.
#[derive(Debug, Clone)]
pub struct EnrichmentLine {
    pub kind: String,
    pub message: String,
}

/// A recommendation from preflight analysis (flag the user could add next time).
#[derive(Debug, Clone, Serialize)]
pub struct RecommendationEntry {
    pub flag: String,
    pub reason: String,
}

/// A command result ready to be formatted.
///
/// Constructed by the proxy from a `RouterResult` — decoupled from router types
/// to avoid circular dependencies between `core` and `router`.
#[derive(Debug, Clone)]
pub struct FormatInput {
    pub command: String,
    pub exit_code: i32,
    pub category: String,
    pub body: String,
    pub raw_output: Option<String>,
    pub total_lines: Option<u64>,
    pub elapsed_secs: Option<f64>,
    pub outcomes: Vec<String>,
    pub hazards: Vec<HazardEntry>,
    pub enrichment: Vec<EnrichmentLine>,
    pub recommendations: Vec<RecommendationEntry>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Get the status symbol for a category and exit code.
///
/// Symbols: `+` success, `!` error, `→` narration, `⚠` dangerous.
pub fn status_symbol(category: &str, exit_code: i32) -> &'static str {
    match category {
        "dangerous" => "\u{26a0}",            // ⚠ always
        "interactive" => "\u{2192}",          // → always
        "narrate" | "structured" => {
            if exit_code == 0 {
                "\u{2192}" // →
            } else {
                "!"
            }
        }
        _ => {
            // condense, passthrough, unknown
            if exit_code == 0 {
                "+"
            } else {
                "!"
            }
        }
    }
}

/// Format a single command result in the given output mode.
pub fn format_result(input: &FormatInput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Human => format_human(input),
        OutputMode::Json => format_json(input),
        OutputMode::Passthrough => format_passthrough(input),
        OutputMode::Context => format_context(input),
    }
}

/// Format multiple command results (for compound commands).
pub fn format_results(inputs: &[FormatInput], mode: OutputMode) -> String {
    inputs
        .iter()
        .map(|input| format_result(input, mode))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Format an elapsed time for compact output.
///
/// - `<1s` → `"150ms"`
/// - `<60s` → `"12.3s"` (strips `.0`)
/// - `≥60s` → `"2m"`
pub fn format_elapsed(elapsed_secs: Option<f64>) -> String {
    match elapsed_secs {
        None => String::new(),
        Some(secs) => {
            if secs < 1.0 {
                format!("{}ms", (secs * 1000.0).round() as u64)
            } else if secs < 60.0 {
                let s = format!("{:.1}s", secs);
                s.replace(".0s", "s")
            } else {
                format!("{}m", (secs / 60.0).ceil() as u64)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Private formatters
// ---------------------------------------------------------------------------

fn format_human(input: &FormatInput) -> String {
    let symbol = status_symbol(&input.category, input.exit_code);

    let mut lines = Vec::new();

    let has_metadata = input.total_lines.is_some() || input.elapsed_secs.is_some();

    if has_metadata {
        // Compact header: {symbol} exit:{code} {elapsed} {category} ({total}→{shown})
        let mut parts = vec![format!("{} exit:{}", symbol, input.exit_code)];

        let elapsed = format_elapsed(input.elapsed_secs);
        if !elapsed.is_empty() {
            parts.push(elapsed);
        }

        if let Some(total) = input.total_lines {
            let shown = if input.body.is_empty() {
                0
            } else {
                input.body.lines().count()
            };
            // Only show ratio when condensing actually reduced lines
            if (total as usize) != shown {
                parts.push(format!("({}\u{2192}{})", total, shown));
            }
        }

        lines.push(parts.join(" "));

        // Body lines — no indentation
        if !input.body.is_empty() {
            for body_line in input.body.lines() {
                lines.push(body_line.to_string());
            }
        }
    } else {
        match input.category.as_str() {
            "passthrough" => {
                lines.push(input.body.clone());
            }
            _ => {
                lines.push(format!("{} {}", symbol, input.body));
            }
        }
    }

    // Outcome lines (no indent)
    for outcome in &input.outcomes {
        lines.push(format!("+ {}", outcome));
    }

    // Hazard lines (no indent)
    for hazard in &input.hazards {
        let prefix = if hazard.severity == "error" { "!" } else { "~" };
        lines.push(format!("{} {}", prefix, hazard.text));
    }

    // Enrichment lines: ~ kind message
    for e in &input.enrichment {
        lines.push(format!("~ {} {}", e.kind, e.message));
    }

    // Recommendation lines (only on success)
    if input.exit_code == 0 && !input.recommendations.is_empty() {
        for r in &input.recommendations {
            lines.push(format!("\u{2192} prefer: {} ({})", r.flag, r.reason));
        }
    }

    lines.join("\n")
}

fn format_json(input: &FormatInput) -> String {
    #[derive(Serialize)]
    struct JsonEnrichment<'a> {
        kind: &'a str,
        message: &'a str,
    }

    #[derive(Serialize)]
    struct JsonOutput<'a> {
        command: &'a str,
        exit_code: i32,
        category: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        elapsed_seconds: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        total_lines: Option<u64>,
        #[serde(skip_serializing_if = "str::is_empty")]
        body: &'a str,
        outcomes: &'a [String],
        hazards: &'a [HazardEntry],
        #[serde(skip_serializing_if = "Vec::is_empty")]
        enrichment: Vec<JsonEnrichment<'a>>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        recommendations: Vec<&'a RecommendationEntry>,
    }

    let enrichment: Vec<JsonEnrichment> = input
        .enrichment
        .iter()
        .map(|e| JsonEnrichment {
            kind: &e.kind,
            message: &e.message,
        })
        .collect();

    // Only include recommendations on success
    let recommendations: Vec<&RecommendationEntry> = if input.exit_code == 0 {
        input.recommendations.iter().collect()
    } else {
        vec![]
    };

    let out = JsonOutput {
        command: &input.command,
        exit_code: input.exit_code,
        category: &input.category,
        elapsed_seconds: input.elapsed_secs,
        total_lines: input.total_lines,
        body: &input.body,
        outcomes: &input.outcomes,
        hazards: &input.hazards,
        enrichment,
        recommendations,
    };

    serde_json::to_string_pretty(&out).unwrap_or_else(|_| "{}".to_string())
}

fn format_passthrough(input: &FormatInput) -> String {
    let raw = input.raw_output.as_deref().unwrap_or(&input.body);
    let symbol = status_symbol(&input.category, input.exit_code);

    let summary_line = if let Some(first) = input.outcomes.first() {
        format!("{} {}: {}", symbol, input.command, first)
    } else {
        format!("{} {}: exit {}", symbol, input.command, input.exit_code)
    };

    let mut summary_lines = vec![summary_line];

    // Hazard lines in summary section
    for hazard in &input.hazards {
        let prefix = if hazard.severity == "error" { "!" } else { "~" };
        summary_lines.push(format!("  {} {}", prefix, hazard.text));
    }

    // Recommendation lines in summary section (only on success)
    if input.exit_code == 0 {
        for r in &input.recommendations {
            summary_lines.push(format!("  ~ next time: consider {} ({})", r.flag, r.reason));
        }
    }

    format!(
        "{}\n\u{2500}\u{2500} mish summary \u{2500}\u{2500}\n{}",
        raw,
        summary_lines.join("\n")
    )
}

fn format_context(input: &FormatInput) -> String {
    let status = if input.exit_code == 0 { "ok" } else { "err" };

    let outcome_str = if !input.outcomes.is_empty() {
        format!(" {}", input.outcomes.join(" "))
    } else {
        String::new()
    };

    let elapsed_str = if let Some(elapsed) = input.elapsed_secs {
        format!(" {}s", elapsed)
    } else {
        String::new()
    };

    let hazard_str = if !input.hazards.is_empty() {
        // Compress identical hazards: count occurrences, show (xN) for N > 1
        let mut counts: Vec<(String, String, usize)> = Vec::new(); // (prefix, text, count)
        for h in &input.hazards {
            let prefix = if h.severity == "error" { "!" } else { "~" };
            if let Some(entry) = counts
                .iter_mut()
                .find(|(p, t, _)| p == prefix && t == &h.text)
            {
                entry.2 += 1;
            } else {
                counts.push((prefix.to_string(), h.text.clone(), 1));
            }
        }
        let h: Vec<String> = counts
            .iter()
            .map(|(prefix, text, count)| {
                if *count > 1 {
                    format!("{}{}(x{})", prefix, text, count)
                } else {
                    format!("{}{}", prefix, text)
                }
            })
            .collect();
        format!(" {}", h.join(" "))
    } else {
        String::new()
    };

    format!(
        "{}: {}{}{}{}",
        input.command, status, outcome_str, elapsed_str, hazard_str
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_input(command: &str, exit_code: i32, category: &str, body: &str) -> FormatInput {
        FormatInput {
            command: command.to_string(),
            exit_code,
            category: category.to_string(),
            body: body.to_string(),
            raw_output: None,
            total_lines: None,
            elapsed_secs: None,
            outcomes: vec![],
            hazards: vec![],
            enrichment: vec![],
            recommendations: vec![],
        }
    }

    // -----------------------------------------------------------------------
    // Test 1: status_symbol correctness for all categories + exit codes
    // -----------------------------------------------------------------------
    #[test]
    fn test_status_symbol_correctness() {
        // Success cases
        assert_eq!(status_symbol("condense", 0), "+");
        assert_eq!(status_symbol("passthrough", 0), "+");
        assert_eq!(status_symbol("narrate", 0), "\u{2192}");     // →
        assert_eq!(status_symbol("structured", 0), "\u{2192}");  // →
        assert_eq!(status_symbol("interactive", 0), "\u{2192}"); // →
        assert_eq!(status_symbol("dangerous", 0), "\u{26a0}");   // ⚠

        // Error cases
        assert_eq!(status_symbol("condense", 1), "!");
        assert_eq!(status_symbol("narrate", 1), "!");
        assert_eq!(status_symbol("passthrough", 1), "!");
        assert_eq!(status_symbol("structured", 1), "!");

        // Dangerous always ⚠ regardless of exit code
        assert_eq!(status_symbol("dangerous", 1), "\u{26a0}");

        // Interactive always → regardless of exit code
        assert_eq!(status_symbol("interactive", 1), "\u{2192}");

        // Unknown category defaults to +/!
        assert_eq!(status_symbol("unknown", 0), "+");
        assert_eq!(status_symbol("unknown", 1), "!");
    }

    // -----------------------------------------------------------------------
    // Test 2: human format — condense success
    // -----------------------------------------------------------------------
    #[test]
    fn test_human_format_condensed_success() {
        let mut input = make_input(
            "npm install",
            0,
            "condense",
            "147 packages installed (12.3s)",
        );
        input.outcomes = vec!["147 packages installed".to_string()];

        let output = format_result(&input, OutputMode::Human);
        assert!(
            output.starts_with("+ "),
            "condense success should start with + symbol: {}",
            output
        );
        assert!(
            output.contains("147 packages"),
            "should contain outcome text: {}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 3: human format — narrate success
    // -----------------------------------------------------------------------
    #[test]
    fn test_human_format_narrated() {
        let input = make_input(
            "cp",
            0,
            "narrate",
            "cp: src/main.rs \u{2192} backup/ (4.2KB)",
        );

        let output = format_result(&input, OutputMode::Human);
        assert!(
            output.starts_with("\u{2192} "),
            "narrate success should use \u{2192} symbol: {}",
            output
        );
        assert!(
            output.contains("src/main.rs"),
            "should contain file path: {}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 4: human format — error (non-zero exit)
    // -----------------------------------------------------------------------
    #[test]
    fn test_human_format_error() {
        let input = make_input(
            "cargo build",
            1,
            "condense",
            "error[E0308] mismatched types",
        );

        let output = format_result(&input, OutputMode::Human);
        assert!(
            output.starts_with("! "),
            "error should use ! symbol: {}",
            output
        );
        assert!(
            output.contains("E0308"),
            "should contain error code: {}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 5: human format — dangerous
    // -----------------------------------------------------------------------
    #[test]
    fn test_human_format_dangerous() {
        let input = make_input(
            "rm -rf",
            0,
            "dangerous",
            "node_modules/ (47,231 files, 312MB) \u{2014} destructive",
        );

        let output = format_result(&input, OutputMode::Human);
        assert!(
            output.starts_with("\u{26a0} "),
            "dangerous should use \u{26a0} symbol: {}",
            output
        );
        assert!(
            output.contains("destructive"),
            "should contain warning: {}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 6: JSON format — valid structure with correct fields
    // -----------------------------------------------------------------------
    #[test]
    fn test_json_format_structure() {
        let mut input = make_input("echo hello", 0, "passthrough", "hello");
        input.outcomes = vec!["hello".to_string()];

        let output = format_result(&input, OutputMode::Json);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).expect("should be valid JSON");

        assert_eq!(parsed["command"], "echo hello");
        assert_eq!(parsed["exit_code"], 0);
        assert_eq!(parsed["category"], "passthrough");
        assert!(parsed["outcomes"].is_array());
        assert_eq!(parsed["outcomes"][0], "hello");
    }

    // -----------------------------------------------------------------------
    // Test 7: passthrough mode — raw output + mish summary
    // -----------------------------------------------------------------------
    #[test]
    fn test_passthrough_format() {
        let mut input = make_input("echo hello", 0, "passthrough", "hello");
        input.raw_output = Some("hello\n".to_string());
        input.outcomes = vec!["output captured".to_string()];

        let output = format_result(&input, OutputMode::Passthrough);
        assert!(
            output.starts_with("hello\n"),
            "should start with raw output: {}",
            output
        );
        assert!(
            output.contains("\u{2500}\u{2500} mish summary \u{2500}\u{2500}"),
            "should contain summary separator: {}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 8: context mode — ultra-compressed single line
    // -----------------------------------------------------------------------
    #[test]
    fn test_context_format() {
        let mut input = make_input("npm install", 0, "condense", "147 packages");
        input.outcomes = vec!["147pkg".to_string()];

        let output = format_result(&input, OutputMode::Context);
        assert!(
            output.starts_with("npm install:"),
            "should start with command: {}",
            output
        );
        assert!(
            output.contains("ok"),
            "should contain ok for exit 0: {}",
            output
        );
        assert!(
            !output.contains('\n'),
            "context should be single line: {}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 9: human format — enrichment lines included
    // -----------------------------------------------------------------------
    #[test]
    fn test_format_with_enrichment() {
        let mut input = make_input(
            "cp",
            1,
            "narrate",
            "cp: error \u{2014} nonexistent.txt: no such file",
        );
        input.enrichment = vec![EnrichmentLine {
            kind: "source".to_string(),
            message: "nonexistent.txt \u{2717}".to_string(),
        }];

        let output = format_result(&input, OutputMode::Human);
        assert!(
            output.contains("~ source"),
            "should include enrichment with ~ prefix: {}",
            output
        );
        assert!(
            output.contains("nonexistent.txt"),
            "should include enrichment message: {}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 10: format_results — multiple results joined
    // -----------------------------------------------------------------------
    #[test]
    fn test_format_multiple_results() {
        let inputs = vec![
            make_input("echo a", 0, "condense", "a"),
            make_input("echo b", 0, "condense", "b"),
        ];

        let output = format_results(&inputs, OutputMode::Human);
        assert!(output.contains("+ a"), "should contain first result: {}", output);
        assert!(output.contains("+ b"), "should contain second result: {}", output);
    }

    // -----------------------------------------------------------------------
    // Test 11: JSON format — hazards included correctly
    // -----------------------------------------------------------------------
    #[test]
    fn test_json_format_with_hazards() {
        let mut input = make_input("npm install", 0, "condense", "147 packages");
        input.hazards = vec![HazardEntry {
            severity: "warning".to_string(),
            text: "deprecated package".to_string(),
        }];

        let output = format_result(&input, OutputMode::Json);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).expect("should be valid JSON");

        assert!(parsed["hazards"].is_array());
        assert_eq!(parsed["hazards"][0]["severity"], "warning");
        assert_eq!(parsed["hazards"][0]["text"], "deprecated package");
    }

    // -----------------------------------------------------------------------
    // Test 12: Human with metadata header (compact format)
    // -----------------------------------------------------------------------
    #[test]
    fn test_human_with_metadata_header() {
        let mut input = make_input(
            "npm install",
            0,
            "condense",
            "last: npm warn deprecated inflight@1.0.6",
        );
        input.total_lines = Some(1400);
        input.elapsed_secs = Some(12.3);

        let output = format_result(&input, OutputMode::Human);
        let first_line = output.lines().next().unwrap();

        // Compact header: + exit:0 12.3s (1400→1)
        assert!(
            first_line.starts_with("+ exit:0"),
            "header should start with symbol and exit code: {}",
            first_line
        );
        assert!(
            first_line.contains("12.3s"),
            "header should contain elapsed time: {}",
            first_line
        );
        assert!(
            !first_line.contains("condense"),
            "category should NOT appear in header: {}",
            first_line
        );
        assert!(
            first_line.contains("(1400\u{2192}1)"),
            "header should contain line ratio: {}",
            first_line
        );

        // Body should NOT be indented
        let second_line = output.lines().nth(1).unwrap();
        assert!(
            !second_line.starts_with("  "),
            "body should not be indented: {}",
            second_line
        );
        assert!(
            second_line.contains("npm warn deprecated"),
            "body should contain original text: {}",
            second_line
        );
    }

    // -----------------------------------------------------------------------
    // Test 13: Human with outcomes (no indent)
    // -----------------------------------------------------------------------
    #[test]
    fn test_human_with_outcomes() {
        let mut input = make_input("npm install", 0, "condense", "install complete");
        input.outcomes = vec![
            "147 packages installed".to_string(),
            "0 vulnerabilities".to_string(),
        ];

        let output = format_result(&input, OutputMode::Human);
        assert!(
            output.contains("\n+ 147 packages installed"),
            "should show outcome with + prefix (no indent): {}",
            output
        );
        assert!(
            output.contains("\n+ 0 vulnerabilities"),
            "should show second outcome (no indent): {}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 14: Human with hazards (no indent)
    // -----------------------------------------------------------------------
    #[test]
    fn test_human_with_hazards() {
        let mut input = make_input("npm install", 1, "condense", "install failed");
        input.hazards = vec![
            HazardEntry {
                severity: "error".to_string(),
                text: "2 vulnerabilities found".to_string(),
            },
            HazardEntry {
                severity: "warning".to_string(),
                text: "npm warn deprecated".to_string(),
            },
        ];

        let output = format_result(&input, OutputMode::Human);
        assert!(
            output.contains("\n! 2 vulnerabilities found"),
            "should show error hazard with ! prefix (no indent): {}",
            output
        );
        assert!(
            output.contains("\n~ npm warn deprecated"),
            "should show warning hazard with ~ prefix (no indent): {}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 15: Human with no metadata — simple format preserved
    // -----------------------------------------------------------------------
    #[test]
    fn test_human_no_metadata_simple_format() {
        let input = make_input("echo hello", 0, "condense", "hello");

        let output = format_result(&input, OutputMode::Human);
        assert_eq!(
            output, "+ hello",
            "no metadata should use simple format: {}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 16: JSON includes body field
    // -----------------------------------------------------------------------
    #[test]
    fn test_json_includes_body() {
        let input = make_input("npm install", 0, "condense", "147 packages installed");

        let output = format_result(&input, OutputMode::Json);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).expect("should be valid JSON");

        assert_eq!(
            parsed["body"], "147 packages installed",
            "JSON should include body field: {}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 17: JSON includes enrichment
    // -----------------------------------------------------------------------
    #[test]
    fn test_json_includes_enrichment() {
        let mut input = make_input("cp foo bar", 1, "narrate", "cp: no such file");
        input.enrichment = vec![
            EnrichmentLine {
                kind: "source".to_string(),
                message: "foo \u{2717}".to_string(),
            },
            EnrichmentLine {
                kind: "dest".to_string(),
                message: "bar/ \u{2713}".to_string(),
            },
        ];

        let output = format_result(&input, OutputMode::Json);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).expect("should be valid JSON");

        assert!(
            parsed["enrichment"].is_array(),
            "should have enrichment array: {}",
            output
        );
        let enrichment = &parsed["enrichment"];
        assert_eq!(enrichment.as_array().unwrap().len(), 2);
        assert_eq!(enrichment[0]["kind"], "source");
        assert_eq!(enrichment[0]["message"], "foo \u{2717}");
        assert_eq!(enrichment[1]["kind"], "dest");
        assert_eq!(enrichment[1]["message"], "bar/ \u{2713}");
    }

    // -----------------------------------------------------------------------
    // Test 18: Context with elapsed time
    // -----------------------------------------------------------------------
    #[test]
    fn test_context_with_elapsed() {
        let mut input = make_input("npm install", 0, "condense", "147 packages");
        input.outcomes = vec!["147pkg".to_string()];
        input.elapsed_secs = Some(12.3);

        let output = format_result(&input, OutputMode::Context);
        assert!(
            output.contains("12.3s"),
            "context should include elapsed time: {}",
            output
        );
        assert!(
            !output.contains('\n'),
            "context should be single line: {}",
            output
        );
        // Verify ordering: ok, outcomes, elapsed, hazards
        let ok_pos = output.find("ok").unwrap();
        let elapsed_pos = output.find("12.3s").unwrap();
        assert!(
            elapsed_pos > ok_pos,
            "elapsed should come after ok: {}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 19: Context hazard compression
    // -----------------------------------------------------------------------
    #[test]
    fn test_context_hazard_compression() {
        let mut input = make_input("npm install", 0, "condense", "147 packages");
        input.outcomes = vec!["147pkg".to_string()];
        input.hazards = vec![
            HazardEntry {
                severity: "warning".to_string(),
                text: "deprecated".to_string(),
            },
            HazardEntry {
                severity: "warning".to_string(),
                text: "deprecated".to_string(),
            },
            HazardEntry {
                severity: "warning".to_string(),
                text: "deprecated".to_string(),
            },
            HazardEntry {
                severity: "error".to_string(),
                text: "2vuln".to_string(),
            },
        ];

        let output = format_result(&input, OutputMode::Context);
        assert!(
            output.contains("~deprecated(x3)"),
            "should compress 3 identical hazards: {}",
            output
        );
        assert!(
            output.contains("!2vuln"),
            "should show single error hazard without count: {}",
            output
        );
        assert!(
            !output.contains('\n'),
            "context should be single line: {}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 20: Passthrough with hazards in summary
    // -----------------------------------------------------------------------
    #[test]
    fn test_passthrough_with_hazards() {
        let mut input = make_input("npm install", 0, "condense", "raw output here");
        input.raw_output = Some("raw output here\n".to_string());
        input.hazards = vec![
            HazardEntry {
                severity: "warning".to_string(),
                text: "deprecated package".to_string(),
            },
            HazardEntry {
                severity: "error".to_string(),
                text: "critical vulnerability".to_string(),
            },
        ];

        let output = format_result(&input, OutputMode::Passthrough);
        assert!(
            output.contains("\u{2500}\u{2500} mish summary \u{2500}\u{2500}"),
            "should contain summary separator: {}",
            output
        );
        assert!(
            output.contains("  ~ deprecated package"),
            "should show warning hazard in summary: {}",
            output
        );
        assert!(
            output.contains("  ! critical vulnerability"),
            "should show error hazard in summary: {}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 21: Human compound results with metadata (compact format)
    // -----------------------------------------------------------------------
    #[test]
    fn test_human_compound_results() {
        let mut input1 = make_input("npm install", 0, "condense", "147 packages installed");
        input1.total_lines = Some(1400);
        input1.elapsed_secs = Some(12.3);
        input1.outcomes = vec!["147 packages installed".to_string()];

        let mut input2 = make_input("npm test", 1, "condense", "3 tests failed");
        input2.hazards = vec![HazardEntry {
            severity: "error".to_string(),
            text: "test suite failure".to_string(),
        }];

        let output = format_results(&[input1, input2], OutputMode::Human);

        // First result: compact header
        assert!(
            output.contains("+ exit:0 12.3s (1400"),
            "first result should have compact header: {}",
            output
        );
        // Second result: error symbol
        assert!(
            output.contains("! 3 tests failed"),
            "second result should show error: {}",
            output
        );
        // Second result: hazard (no indent)
        assert!(
            output.contains("\n! test suite failure"),
            "second result should show hazard (no indent): {}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 22: Human format — recommendations on success (→ consider)
    // -----------------------------------------------------------------------
    #[test]
    fn test_human_format_recommendations_on_success() {
        let mut input = make_input("npm install", 0, "condense", "147 packages installed");
        input.recommendations = vec![
            RecommendationEntry {
                flag: "--prefer-offline".to_string(),
                reason: "quieter output".to_string(),
            },
        ];

        let output = format_result(&input, OutputMode::Human);
        assert!(
            output.contains("\u{2192} prefer: --prefer-offline"),
            "should show recommendation with \u{2192} prefix: {}",
            output
        );
        assert!(
            output.contains("(quieter output)"),
            "should include reason in parens: {}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 23: Human format — recommendations suppressed on failure
    // -----------------------------------------------------------------------
    #[test]
    fn test_human_format_recommendations_suppressed_on_failure() {
        let mut input = make_input("npm install", 1, "condense", "install failed");
        input.recommendations = vec![
            RecommendationEntry {
                flag: "--prefer-offline".to_string(),
                reason: "quieter output".to_string(),
            },
        ];

        let output = format_result(&input, OutputMode::Human);
        assert!(
            !output.contains("consider"),
            "recommendations should NOT appear on failure: {}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 24: JSON format — recommendations included on success
    // -----------------------------------------------------------------------
    #[test]
    fn test_json_format_recommendations_on_success() {
        let mut input = make_input("npm install", 0, "condense", "147 packages installed");
        input.recommendations = vec![
            RecommendationEntry {
                flag: "--no-progress".to_string(),
                reason: "reduces noise for LLM consumption".to_string(),
            },
        ];

        let output = format_result(&input, OutputMode::Json);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).expect("should be valid JSON");

        assert!(parsed["recommendations"].is_array());
        let recs = parsed["recommendations"].as_array().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0]["flag"], "--no-progress");
        assert_eq!(recs[0]["reason"], "reduces noise for LLM consumption");
    }

    // -----------------------------------------------------------------------
    // Test 25: JSON format — recommendations omitted on failure
    // -----------------------------------------------------------------------
    #[test]
    fn test_json_format_recommendations_omitted_on_failure() {
        let mut input = make_input("npm install", 1, "condense", "install failed");
        input.recommendations = vec![
            RecommendationEntry {
                flag: "--no-progress".to_string(),
                reason: "reduces noise".to_string(),
            },
        ];

        let output = format_result(&input, OutputMode::Json);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).expect("should be valid JSON");

        // recommendations should be absent (empty vec is skipped by skip_serializing_if)
        assert!(
            parsed.get("recommendations").is_none(),
            "recommendations should be omitted on failure: {}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 26: Passthrough format — recommendations in summary on success
    // -----------------------------------------------------------------------
    #[test]
    fn test_passthrough_format_recommendations_on_success() {
        let mut input = make_input("npm install", 0, "condense", "raw output here");
        input.raw_output = Some("raw output here\n".to_string());
        input.recommendations = vec![
            RecommendationEntry {
                flag: "--silent".to_string(),
                reason: "Consider adding --silent for quieter output".to_string(),
            },
        ];

        let output = format_result(&input, OutputMode::Passthrough);
        assert!(
            output.contains("~ next time: consider --silent"),
            "passthrough summary should include recommendation: {}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 27: Multiple recommendations rendered
    // -----------------------------------------------------------------------
    #[test]
    fn test_multiple_recommendations() {
        let mut input = make_input("npm install", 0, "condense", "installed");
        input.recommendations = vec![
            RecommendationEntry {
                flag: "--no-progress".to_string(),
                reason: "reduces noise".to_string(),
            },
            RecommendationEntry {
                flag: "--prefer-offline".to_string(),
                reason: "faster when packages cached".to_string(),
            },
        ];

        let output = format_result(&input, OutputMode::Human);
        assert!(
            output.contains("\u{2192} prefer: --no-progress"),
            "should show first recommendation: {}",
            output
        );
        assert!(
            output.contains("\u{2192} prefer: --prefer-offline"),
            "should show second recommendation: {}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 28: Empty recommendations — no extra output
    // -----------------------------------------------------------------------
    #[test]
    fn test_empty_recommendations_no_output() {
        let input = make_input("echo hello", 0, "condense", "hello");
        let output = format_result(&input, OutputMode::Human);
        assert!(
            !output.contains("consider"),
            "no recommendations means no recommendation lines: {}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 29: format_elapsed helper
    // -----------------------------------------------------------------------
    #[test]
    fn test_format_elapsed() {
        assert_eq!(format_elapsed(None), "");
        assert_eq!(format_elapsed(Some(0.023)), "23ms");
        assert_eq!(format_elapsed(Some(0.150)), "150ms");
        assert_eq!(format_elapsed(Some(0.999)), "999ms");
        assert_eq!(format_elapsed(Some(1.0)), "1s");
        assert_eq!(format_elapsed(Some(5.2)), "5.2s");
        assert_eq!(format_elapsed(Some(12.3)), "12.3s");
        assert_eq!(format_elapsed(Some(59.9)), "59.9s");
        assert_eq!(format_elapsed(Some(60.0)), "1m");
        assert_eq!(format_elapsed(Some(90.0)), "2m");
        assert_eq!(format_elapsed(Some(120.0)), "2m");
    }
}
