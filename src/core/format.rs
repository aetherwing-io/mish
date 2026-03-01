/// Output formatting.
///
/// Modes: human (default), json, passthrough, context.
/// Provides status symbols and mode-specific renderers for command results.

use serde::Serialize;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Output mode controlling how results are displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Status symbols + indented body (default).
    Human,
    /// Structured JSON envelope.
    Json,
    /// Raw output + mish summary footer.
    Passthrough,
    /// Ultra-compressed single line for LLM context.
    Context,
}

impl Default for OutputMode {
    fn default() -> Self {
        OutputMode::Human
    }
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
// Private formatters
// ---------------------------------------------------------------------------

fn format_human(input: &FormatInput) -> String {
    let symbol = status_symbol(&input.category, input.exit_code);

    let mut lines = Vec::new();

    match input.category.as_str() {
        "passthrough" => {
            // Passthrough: raw output + footer (no symbol prefix)
            lines.push(input.body.clone());
        }
        _ => {
            lines.push(format!("{} {}", symbol, input.body));
        }
    }

    // Enrichment lines (indented under the main output)
    for e in &input.enrichment {
        lines.push(format!("  {}: {}", e.kind, e.message));
    }

    lines.join("\n")
}

fn format_json(input: &FormatInput) -> String {
    #[derive(Serialize)]
    struct JsonOutput<'a> {
        command: &'a str,
        exit_code: i32,
        category: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        elapsed_seconds: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        total_lines: Option<u64>,
        outcomes: &'a [String],
        hazards: &'a [HazardEntry],
    }

    let out = JsonOutput {
        command: &input.command,
        exit_code: input.exit_code,
        category: &input.category,
        elapsed_seconds: input.elapsed_secs,
        total_lines: input.total_lines,
        outcomes: &input.outcomes,
        hazards: &input.hazards,
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

    format!(
        "{}\n\u{2500}\u{2500} mish summary \u{2500}\u{2500}\n{}",
        raw, summary_line
    )
}

fn format_context(input: &FormatInput) -> String {
    let status = if input.exit_code == 0 { "ok" } else { "err" };

    let outcome_str = if !input.outcomes.is_empty() {
        format!(" {}", input.outcomes.join(" "))
    } else {
        String::new()
    };

    let hazard_str = if !input.hazards.is_empty() {
        let h: Vec<String> = input
            .hazards
            .iter()
            .map(|h| {
                let prefix = if h.severity == "error" { "!" } else { "~" };
                format!("{}{}", prefix, h.text)
            })
            .collect();
        format!(" {}", h.join(" "))
    } else {
        String::new()
    };

    format!("{}: {}{}{}", input.command, status, outcome_str, hazard_str)
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
            output.contains("source:"),
            "should include enrichment kind: {}",
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
}
