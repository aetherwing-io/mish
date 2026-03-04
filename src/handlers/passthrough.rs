//! Passthrough handler — execute and pass output verbatim with metadata footer.
//!
//! Commands like cat, head, tail, grep, ls, echo -- the output is the point.
//! We pass it through unchanged and append a small metadata footer.

use std::process::Command;

/// Result of a passthrough command execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassthroughResult {
    /// The raw command output, passed through verbatim.
    pub output: String,
    /// Metadata footer: "-- {lines} lines, {size} --"
    pub footer: String,
    /// Process exit code.
    pub exit_code: i32,
}

/// Format a byte count into a human-readable size string.
///
/// - < 1024: "{n} B"
/// - < 1_048_576: "{n:.1} KB"
/// - < 1_073_741_824: "{n:.1} MB"
/// - otherwise: "{n:.1} GB"
pub fn human_size(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;

    let b = bytes as f64;
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if b < MB {
        format!("{:.1} KB", b / KB)
    } else if b < GB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.1} GB", b / GB)
    }
}

/// Format raw output into a PassthroughResult with metadata footer.
///
/// This is the pure/testable core — no command execution.
pub fn format_passthrough(output: &str, exit_code: i32) -> PassthroughResult {
    let line_count = if output.is_empty() {
        0
    } else {
        output.lines().count()
    };
    let size = human_size(output.len());
    let footer = format!("-- {} lines, {} --", line_count, size);

    PassthroughResult {
        output: output.to_string(),
        footer,
        exit_code,
    }
}

/// Execute a command and return its output verbatim with a metadata footer.
pub fn handle(args: &[String]) -> Result<PassthroughResult, Box<dyn std::error::Error>> {
    if args.is_empty() {
        return Err("passthrough: empty command".into());
    }

    let output = Command::new(&args[0])
        .args(&args[1..])
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Combine stdout and stderr (stderr appended if non-empty)
    let combined = if stderr.is_empty() {
        stdout.into_owned()
    } else {
        format!("{}{}", stdout, stderr)
    };

    let exit_code = output.status.code().unwrap_or(-1);

    Ok(format_passthrough(&combined, exit_code))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Test 1: passthrough output fidelity — output matches input exactly
    #[test]
    fn test_passthrough_output_fidelity() {
        let raw = "line one\nline two\nline three\n";
        let result = format_passthrough(raw, 0);
        assert_eq!(result.output, raw, "output must be passed through verbatim");
        assert_eq!(result.exit_code, 0);
    }

    // Test 2: metadata footer accuracy — correct line count and size
    #[test]
    fn test_metadata_footer_accuracy() {
        // "hello\nworld\n" = 12 bytes, 2 lines
        let raw = "hello\nworld\n";
        let result = format_passthrough(raw, 0);
        assert_eq!(result.footer, "-- 2 lines, 12 B --");
    }

    // Test 6: empty output handling
    #[test]
    fn test_empty_output_handling() {
        let result = format_passthrough("", 0);
        assert_eq!(result.output, "");
        assert_eq!(result.footer, "-- 0 lines, 0 B --");
        assert_eq!(result.exit_code, 0);
    }

    // Test 7: passthrough with multi-line output
    #[test]
    fn test_passthrough_multiline_output() {
        let raw = "alpha\nbeta\ngamma\ndelta\nepsilon\n";
        let result = format_passthrough(raw, 0);
        assert_eq!(result.output, raw);
        // 5 lines (lines() skips trailing empty)
        assert!(
            result.footer.starts_with("-- 5 lines,"),
            "expected 5 lines in footer, got: {}",
            result.footer
        );
    }

    // Test 9: passthrough footer format correctness
    #[test]
    fn test_passthrough_footer_format() {
        // Verify the footer pattern: "-- {N} lines, {size} --"
        let raw = "a\n";
        let result = format_passthrough(raw, 0);
        assert_eq!(result.footer, "-- 1 lines, 2 B --");

        // Larger output to test KB formatting
        let big = "x".repeat(2048);
        let result_big = format_passthrough(&big, 0);
        assert_eq!(result_big.footer, "-- 1 lines, 2.0 KB --");
    }

    // human_size edge cases
    #[test]
    fn test_human_size_boundaries() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1), "1 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(1048576), "1.0 MB");
        assert_eq!(human_size(1073741824), "1.0 GB");
    }

    // Test with non-zero exit code
    #[test]
    fn test_passthrough_nonzero_exit() {
        let result = format_passthrough("error output\n", 1);
        assert_eq!(result.exit_code, 1);
        assert_eq!(result.output, "error output\n");
    }
}
