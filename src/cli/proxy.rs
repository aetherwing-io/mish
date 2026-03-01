/// CLI proxy entry point.
///
/// Parses the command, invokes the category router, and formats terminal output.
/// Handles compound commands (split on &&, ||, ;) and output mode flags.

use std::collections::HashMap;

use crate::core::format::{
    self, EnrichmentLine, FormatInput, HazardEntry, OutputMode,
};
use crate::handlers::structured::StructuredData;
use crate::router::categories::{CategoriesConfig, Category, DangerousPattern};
use crate::router::{self, HandlerOutput, RouterResult};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Compound command operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompoundOp {
    /// `&&` — run next only if previous succeeded.
    And,
    /// `||` — run next only if previous failed.
    Or,
    /// `;` — always run next.
    Seq,
}

/// A segment of a compound command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompoundSegment {
    pub command: Vec<String>,
    pub operator: Option<CompoundOp>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Split args on compound operators (&&, ||, ;).
///
/// Each segment's `operator` indicates the operator that *follows* it.
/// The last segment always has `operator: None`.
pub fn split_compound(args: &[String]) -> Vec<CompoundSegment> {
    let mut segments = Vec::new();
    let mut current = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        if arg == "&&" {
            if !current.is_empty() {
                segments.push(CompoundSegment {
                    command: std::mem::take(&mut current),
                    operator: Some(CompoundOp::And),
                });
            }
        } else if arg == "||" {
            if !current.is_empty() {
                segments.push(CompoundSegment {
                    command: std::mem::take(&mut current),
                    operator: Some(CompoundOp::Or),
                });
            }
        } else if arg == ";" {
            if !current.is_empty() {
                segments.push(CompoundSegment {
                    command: std::mem::take(&mut current),
                    operator: Some(CompoundOp::Seq),
                });
            }
        } else {
            current.push(arg.clone());
        }

        i += 1;
    }

    // Push the last segment (no trailing operator)
    if !current.is_empty() {
        segments.push(CompoundSegment {
            command: current,
            operator: None,
        });
    }

    segments
}

/// Extract output mode flag from args, returning (mode, remaining_args).
///
/// Recognises `--json`, `--passthrough`, `--context` as the first argument.
pub fn parse_mode(args: &[String]) -> (OutputMode, Vec<String>) {
    if args.is_empty() {
        return (OutputMode::Human, vec![]);
    }

    match args[0].as_str() {
        "--json" => (OutputMode::Json, args[1..].to_vec()),
        "--passthrough" => (OutputMode::Passthrough, args[1..].to_vec()),
        "--context" => (OutputMode::Context, args[1..].to_vec()),
        _ => (OutputMode::Human, args.to_vec()),
    }
}

/// Run the CLI proxy pipeline: parse mode from args, split compounds, route, format, print.
/// Returns the exit code of the last executed command.
pub fn run(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let (mode, command_args) = parse_mode(args);
    run_with_mode(&command_args, mode)
}

/// Run the CLI proxy pipeline with an explicit output mode.
/// Returns the exit code of the last executed command.
pub fn run_with_mode(args: &[String], mode: OutputMode) -> Result<i32, Box<dyn std::error::Error>> {
    if args.is_empty() {
        return Err("usage: mish <command> [args...]".into());
    }

    let segments = split_compound(args);

    // Use empty defaults — real config loading is a separate bead
    let grammars: HashMap<String, crate::core::grammar::Grammar> = HashMap::new();
    let categories_config = CategoriesConfig {
        categories: HashMap::new(),
    };
    let dangerous_patterns: Vec<DangerousPattern> = Vec::new();

    let mut results: Vec<FormatInput> = Vec::new();
    let mut last_exit_code = 0i32;

    for (i, segment) in segments.iter().enumerate() {
        // Compound operator logic: check previous segment's operator
        if i > 0 {
            if let Some(prev_op) = segments[i - 1].operator {
                match prev_op {
                    CompoundOp::And => {
                        if last_exit_code != 0 {
                            continue; // skip — previous failed
                        }
                    }
                    CompoundOp::Or => {
                        if last_exit_code == 0 {
                            continue; // skip — previous succeeded
                        }
                    }
                    CompoundOp::Seq => {} // always run
                }
            }
        }

        let router_result = router::route(
            &segment.command,
            &grammars,
            &categories_config,
            &dangerous_patterns,
            to_preflight_mode(mode),
        )?;

        last_exit_code = router_result.exit_code;
        results.push(router_result_to_format_input(&router_result, &segment.command));
    }

    // Format and print
    let formatted = if results.len() == 1 {
        format::format_result(&results[0], mode)
    } else {
        format::format_results(&results, mode)
    };

    println!("{}", formatted);

    Ok(last_exit_code)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Convert format::OutputMode to preflight::OutputMode for the router.
fn to_preflight_mode(mode: OutputMode) -> crate::core::preflight::OutputMode {
    match mode {
        OutputMode::Human => crate::core::preflight::OutputMode::Human,
        OutputMode::Json => crate::core::preflight::OutputMode::Json,
        OutputMode::Passthrough => crate::core::preflight::OutputMode::Passthrough,
        OutputMode::Context => crate::core::preflight::OutputMode::Context,
    }
}

/// Convert a RouterResult into a FormatInput for the formatter.
fn router_result_to_format_input(result: &RouterResult, command: &[String]) -> FormatInput {
    let cmd_str = command.join(" ");
    let category = category_to_str(result.category);

    let (body, raw_output, outcomes, hazards) = match &result.output {
        HandlerOutput::Condensed(summary) => {
            // Assemble body from summary parts
            let mut body_parts = vec![summary.header.clone()];
            body_parts.extend(summary.summary_lines.iter().cloned());
            body_parts.extend(summary.hazard_lines.iter().cloned());
            let body = body_parts.join("\n");

            let outcomes: Vec<String> = summary
                .summary_lines
                .iter()
                .filter_map(|l| l.strip_prefix(" + ").map(|s| s.to_string()))
                .collect();

            let hazards: Vec<HazardEntry> = summary
                .hazard_lines
                .iter()
                .map(|l| {
                    if let Some(text) = l.strip_prefix(" ! ") {
                        HazardEntry {
                            severity: "error".to_string(),
                            text: text.to_string(),
                        }
                    } else if let Some(text) = l.strip_prefix(" ~ ") {
                        HazardEntry {
                            severity: "warning".to_string(),
                            text: text.to_string(),
                        }
                    } else {
                        HazardEntry {
                            severity: "info".to_string(),
                            text: l.to_string(),
                        }
                    }
                })
                .collect();

            (body, None, outcomes, hazards)
        }
        HandlerOutput::Narrated(nr) => (nr.message.clone(), None, vec![], vec![]),
        HandlerOutput::Passthrough(pr) => {
            let body = format!("{}\n\u{2500}\u{2500} {} \u{2500}\u{2500}", pr.output, pr.footer);
            (body, Some(pr.output.clone()), vec![], vec![])
        }
        HandlerOutput::Structured(sr) => {
            let body = format_structured_data(&sr.parsed);
            (body, None, vec![], vec![])
        }
        HandlerOutput::Interactive(ir) => {
            let secs = ir.duration.as_secs();
            let body = format!("{}: session ended ({}s)", cmd_str, secs);
            (body, None, vec![], vec![])
        }
        HandlerOutput::Dangerous(dr) => (dr.warning.clone(), None, vec![], vec![]),
    };

    let enrichment = result
        .enrichment
        .as_ref()
        .map(|lines| {
            lines
                .iter()
                .map(|l| EnrichmentLine {
                    kind: l.kind.clone(),
                    message: l.message.clone(),
                })
                .collect()
        })
        .unwrap_or_default();

    FormatInput {
        command: cmd_str,
        exit_code: result.exit_code,
        category: category.to_string(),
        body,
        raw_output,
        total_lines: None,
        elapsed_secs: None,
        outcomes,
        hazards,
        enrichment,
    }
}

/// Format structured data for human display.
fn format_structured_data(data: &StructuredData) -> String {
    match data {
        StructuredData::GitStatus(info) => {
            let total = info.modified + info.added + info.deleted + info.untracked;
            let mut parts = Vec::new();
            if info.modified > 0 {
                parts.push(format!("{} modified", info.modified));
            }
            if info.added > 0 {
                parts.push(format!("{} added", info.added));
            }
            if info.deleted > 0 {
                parts.push(format!("{} deleted", info.deleted));
            }
            if info.untracked > 0 {
                parts.push(format!("{} untracked", info.untracked));
            }
            format!("git status: {} files ({})", total, parts.join(", "))
        }
        StructuredData::DockerPs(containers) => {
            format!("docker ps: {} containers running", containers.len())
        }
        StructuredData::Generic(raw) => raw.clone(),
    }
}

fn category_to_str(cat: Category) -> &'static str {
    match cat {
        Category::Condense => "condense",
        Category::Narrate => "narrate",
        Category::Passthrough => "passthrough",
        Category::Structured => "structured",
        Category::Interactive => "interactive",
        Category::Dangerous => "dangerous",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    // -----------------------------------------------------------------------
    // Test 12: split_compound with &&
    // -----------------------------------------------------------------------
    #[test]
    fn test_split_compound_and() {
        let input = args(&["echo", "hello", "&&", "echo", "world"]);
        let segments = split_compound(&input);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].command, args(&["echo", "hello"]));
        assert_eq!(segments[0].operator, Some(CompoundOp::And));
        assert_eq!(segments[1].command, args(&["echo", "world"]));
        assert_eq!(segments[1].operator, None);
    }

    // -----------------------------------------------------------------------
    // Test 13: split_compound with ||
    // -----------------------------------------------------------------------
    #[test]
    fn test_split_compound_or() {
        let input = args(&["false", "||", "echo", "fallback"]);
        let segments = split_compound(&input);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].command, args(&["false"]));
        assert_eq!(segments[0].operator, Some(CompoundOp::Or));
        assert_eq!(segments[1].command, args(&["echo", "fallback"]));
        assert_eq!(segments[1].operator, None);
    }

    // -----------------------------------------------------------------------
    // Test 14: split_compound with ;
    // -----------------------------------------------------------------------
    #[test]
    fn test_split_compound_seq() {
        let input = args(&["echo", "a", ";", "echo", "b"]);
        let segments = split_compound(&input);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].command, args(&["echo", "a"]));
        assert_eq!(segments[0].operator, Some(CompoundOp::Seq));
        assert_eq!(segments[1].command, args(&["echo", "b"]));
        assert_eq!(segments[1].operator, None);
    }

    // -----------------------------------------------------------------------
    // Test 15: parse_mode extracts flags correctly
    // -----------------------------------------------------------------------
    #[test]
    fn test_parse_mode_flags() {
        let (mode, cmd) = parse_mode(&args(&["--json", "echo", "hello"]));
        assert_eq!(mode, OutputMode::Json);
        assert_eq!(cmd, args(&["echo", "hello"]));

        let (mode, cmd) = parse_mode(&args(&["--passthrough", "ls"]));
        assert_eq!(mode, OutputMode::Passthrough);
        assert_eq!(cmd, args(&["ls"]));

        let (mode, cmd) = parse_mode(&args(&["--context", "npm", "install"]));
        assert_eq!(mode, OutputMode::Context);
        assert_eq!(cmd, args(&["npm", "install"]));

        let (mode, cmd) = parse_mode(&args(&["echo", "hello"]));
        assert_eq!(mode, OutputMode::Human);
        assert_eq!(cmd, args(&["echo", "hello"]));
    }

    // -----------------------------------------------------------------------
    // Test 16: exit code propagation through run()
    // -----------------------------------------------------------------------
    #[test]
    fn test_exit_code_propagation() {
        let exit_code = run(&args(&["/bin/sh", "-c", "exit 1"])).unwrap();
        assert_ne!(exit_code, 0, "/bin/sh -c 'exit 1' should return non-zero");

        let exit_code = run(&args(&["echo", "hello"])).unwrap();
        assert_eq!(exit_code, 0, "echo should return zero");
    }

    // -----------------------------------------------------------------------
    // Test 17: split_compound with mixed operators
    // -----------------------------------------------------------------------
    #[test]
    fn test_split_compound_mixed() {
        let input = args(&["echo", "a", "&&", "echo", "b", ";", "echo", "c"]);
        let segments = split_compound(&input);
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].command, args(&["echo", "a"]));
        assert_eq!(segments[0].operator, Some(CompoundOp::And));
        assert_eq!(segments[1].command, args(&["echo", "b"]));
        assert_eq!(segments[1].operator, Some(CompoundOp::Seq));
        assert_eq!(segments[2].command, args(&["echo", "c"]));
        assert_eq!(segments[2].operator, None);
    }
}
