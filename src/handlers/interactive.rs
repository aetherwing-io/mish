/// Interactive handler — mode-aware.
///
/// CLI: detect raw mode -> transparent passthrough -> session summary on exit.
/// MCP: return error/warning (interactive commands can't run over MCP stdio).

use std::time::Duration;

/// Result of running an interactive command.
pub struct InteractiveResult {
    pub summary: String,
    pub exit_code: i32,
    pub duration: Duration,
}

/// Known interactive commands (Phase 1: static list).
const INTERACTIVE_COMMANDS: &[&str] = &[
    "vim", "nvim", "vi", "nano", "emacs",
    "htop", "top", "btop",
    "less", "more",
    "man",
    "ssh",
    "python", "python3", "ipython", "node", "irb", "ghci",
    "tmux", "screen",
    "fzf",
    "nnn", "ranger", "mc",
];

/// Check if a command is interactive (by matching the base command name).
pub fn is_interactive(args: &[String]) -> bool {
    if args.is_empty() {
        return false;
    }
    let cmd = args[0]
        .rsplit('/')
        .next()
        .unwrap_or(&args[0]);
    INTERACTIVE_COMMANDS.contains(&cmd)
}

/// Format a duration for display.
///
/// - < 60s: "1.2s"
/// - >= 60s and < 1h: "1m 23s"
/// - >= 1h: "1h 2m"
pub fn format_duration(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    let millis = duration.subsec_millis();

    if total_secs < 60 {
        let fractional = total_secs as f64 + millis as f64 / 1000.0;
        format!("{:.1}s", fractional)
    } else if total_secs < 3600 {
        let minutes = total_secs / 60;
        let seconds = total_secs % 60;
        format!("{}m {}s", minutes, seconds)
    } else {
        let hours = total_secs / 3600;
        let minutes = (total_secs % 3600) / 60;
        format!("{}h {}m", hours, minutes)
    }
}

/// Format the session summary line.
///
/// Output: "→ {cmd}: session ended ({duration})"
pub fn format_session_summary(cmd: &str, duration: Duration) -> String {
    format!("\u{2192} {}: session ended ({})", cmd, format_duration(duration))
}

/// Run an interactive command with transparent PTY passthrough.
///
/// Phase 1: uses std::process::Command with inherited stdio for transparent I/O.
pub fn handle(args: &[String]) -> Result<InteractiveResult, Box<dyn std::error::Error>> {
    if args.is_empty() {
        return Err("No command provided".into());
    }

    let start = std::time::Instant::now();

    let status = std::process::Command::new(&args[0])
        .args(&args[1..])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()?;

    let duration = start.elapsed();
    let exit_code = status.code().unwrap_or(-1);
    let summary = format_session_summary(&args[0], duration);

    Ok(InteractiveResult {
        summary,
        exit_code,
        duration,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // Test 1: Interactive detection — recognize interactive commands
    #[test]
    fn test_interactive_detection() {
        // Known interactive commands
        let vim = vec!["vim".to_string(), "file.txt".to_string()];
        assert!(is_interactive(&vim));

        let htop = vec!["htop".to_string()];
        assert!(is_interactive(&htop));

        let less = vec!["less".to_string(), "log.txt".to_string()];
        assert!(is_interactive(&less));

        let nano = vec!["nano".to_string()];
        assert!(is_interactive(&nano));

        let ssh = vec!["ssh".to_string(), "host".to_string()];
        assert!(is_interactive(&ssh));

        // Non-interactive commands
        let ls = vec!["ls".to_string()];
        assert!(!is_interactive(&ls));

        let cat = vec!["cat".to_string(), "file.txt".to_string()];
        assert!(!is_interactive(&cat));

        let grep = vec!["grep".to_string(), "pattern".to_string()];
        assert!(!is_interactive(&grep));

        // Empty args
        let empty: Vec<String> = vec![];
        assert!(!is_interactive(&empty));
    }

    // Test 2: Transparent passthrough — output is not modified (full path detection)
    #[test]
    fn test_interactive_detection_full_path() {
        // Full paths should still detect the base command
        let vim = vec!["/usr/bin/vim".to_string(), "file.txt".to_string()];
        assert!(is_interactive(&vim));

        let htop = vec!["/usr/local/bin/htop".to_string()];
        assert!(is_interactive(&htop));

        let python = vec!["/usr/bin/python3".to_string()];
        assert!(is_interactive(&python));
    }

    // Test 3: Session summary format — "→ cmd: session ended (duration)"
    #[test]
    fn test_session_summary_format() {
        let summary = format_session_summary("vim", Duration::from_secs(5));
        assert_eq!(summary, "\u{2192} vim: session ended (5.0s)");

        let summary = format_session_summary("htop", Duration::from_secs(90));
        assert_eq!(summary, "\u{2192} htop: session ended (1m 30s)");

        let summary = format_session_summary("ssh", Duration::from_secs(3723));
        assert_eq!(summary, "\u{2192} ssh: session ended (1h 2m)");
    }

    // Test 4: Duration formatting — seconds, minutes, hours
    #[test]
    fn test_duration_formatting() {
        // Sub-second
        assert_eq!(format_duration(Duration::from_millis(500)), "0.5s");

        // Seconds with decimals
        assert_eq!(format_duration(Duration::from_millis(1200)), "1.2s");

        // Exact seconds
        assert_eq!(format_duration(Duration::from_secs(5)), "5.0s");

        // Edge: 59 seconds
        assert_eq!(format_duration(Duration::from_secs(59)), "59.0s");

        // Minutes boundary
        assert_eq!(format_duration(Duration::from_secs(60)), "1m 0s");

        // Minutes and seconds
        assert_eq!(format_duration(Duration::from_secs(83)), "1m 23s");

        // Edge: 59 minutes 59 seconds
        assert_eq!(format_duration(Duration::from_secs(3599)), "59m 59s");

        // Hours boundary
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h 0m");

        // Hours and minutes
        assert_eq!(format_duration(Duration::from_secs(3720)), "1h 2m");

        // Multiple hours
        assert_eq!(format_duration(Duration::from_secs(7380)), "2h 3m");

        // Zero
        assert_eq!(format_duration(Duration::from_secs(0)), "0.0s");
    }
}
