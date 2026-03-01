/// Dangerous handler — mode-aware.
///
/// CLI: warn on terminal -> prompt human -> maybe execute.
/// MCP: return structured warning -> policy engine -> LLM decides or escalates.

use crate::router::categories::DangerousPattern;

/// Result of handling a dangerous command.
pub struct DangerousResult {
    pub executed: bool,
    pub exit_code: Option<i32>,
    pub warning: String,
}

/// Check if a command matches any dangerous pattern.
///
/// Returns Some((warning_message, reason)) if the command matches a dangerous pattern,
/// or None if the command is not dangerous.
pub fn check_dangerous(
    args: &[String],
    dangerous_patterns: &[DangerousPattern],
) -> Option<(String, String)> {
    if args.is_empty() {
        return None;
    }

    let full_command = args.join(" ");

    for dp in dangerous_patterns {
        if dp.pattern.is_match(&full_command) {
            let warning = format_warning(&full_command, &dp.reason);
            return Some((warning, dp.reason.clone()));
        }
    }

    None
}

/// Format a dangerous command warning.
///
/// Output: "⚠ {cmd}: {reason} -- proceed? [y/N]"
pub fn format_warning(cmd: &str, reason: &str) -> String {
    format!("\u{26a0} {}: {} -- proceed? [y/N]", cmd, reason)
}

/// Read a yes/no confirmation from the user.
///
/// Returns true only if the user explicitly types "y" or "Y".
/// Default (empty input, "n", "N", or anything else) is false.
fn prompt_confirmation(warning: &str) -> bool {
    eprint!("{} ", warning);

    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }

    matches!(input.trim(), "y" | "Y" | "yes" | "Yes" | "YES")
}

/// Handle a dangerous command: check patterns, warn, prompt, maybe execute.
///
/// If the command matches a dangerous pattern, display a warning and prompt
/// the user for confirmation. If confirmed (or if the command is not dangerous),
/// execute it. Otherwise, abort.
pub fn handle(
    args: &[String],
    dangerous_patterns: &[DangerousPattern],
) -> Result<DangerousResult, Box<dyn std::error::Error>> {
    if args.is_empty() {
        return Err("No command provided".into());
    }

    // Check if the command matches any dangerous pattern
    if let Some((warning, _reason)) = check_dangerous(args, dangerous_patterns) {
        // Prompt user for confirmation
        if !prompt_confirmation(&warning) {
            return Ok(DangerousResult {
                executed: false,
                exit_code: None,
                warning,
            });
        }

        // User confirmed — execute the command
        let status = std::process::Command::new(&args[0])
            .args(&args[1..])
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()?;

        let exit_code = status.code().unwrap_or(-1);

        Ok(DangerousResult {
            executed: true,
            exit_code: Some(exit_code),
            warning,
        })
    } else {
        // Not dangerous — just execute
        let status = std::process::Command::new(&args[0])
            .args(&args[1..])
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()?;

        let exit_code = status.code().unwrap_or(-1);

        Ok(DangerousResult {
            executed: true,
            exit_code: Some(exit_code),
            warning: String::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    /// Helper: load the 9 dangerous patterns from the spec.
    fn test_patterns() -> Vec<DangerousPattern> {
        vec![
            DangerousPattern {
                pattern: Regex::new(r"rm\s+-rf").unwrap(),
                reason: "Force recursive delete".to_string(),
            },
            DangerousPattern {
                pattern: Regex::new(r"git\s+push\s+.*--force").unwrap(),
                reason: "Overwrites remote history".to_string(),
            },
            DangerousPattern {
                pattern: Regex::new(r"git\s+reset\s+--hard").unwrap(),
                reason: "Discards uncommitted changes".to_string(),
            },
            DangerousPattern {
                pattern: Regex::new(r"git\s+clean\s+.*-f").unwrap(),
                reason: "Removes untracked files".to_string(),
            },
            DangerousPattern {
                pattern: Regex::new(r"docker\s+system\s+prune").unwrap(),
                reason: "Removes all unused Docker data".to_string(),
            },
            DangerousPattern {
                pattern: Regex::new(r"(?i)DROP\s+TABLE").unwrap(),
                reason: "Drops database table".to_string(),
            },
            DangerousPattern {
                pattern: Regex::new(r"chmod\s+.*-R\s+777|chmod\s+.*777.*-R").unwrap(),
                reason: "Opens all permissions recursively".to_string(),
            },
            DangerousPattern {
                pattern: Regex::new(r"^dd\s+").unwrap(),
                reason: "Direct disk write".to_string(),
            },
            DangerousPattern {
                pattern: Regex::new(r"^mkfs\.").unwrap(),
                reason: "Create filesystem (overwrites partition)".to_string(),
            },
        ]
    }

    // Test 5: Dangerous pattern matching — all 9 patterns detected
    #[test]
    fn test_dangerous_pattern_matching() {
        let patterns = test_patterns();

        // 1. rm -rf
        let args = vec!["rm".to_string(), "-rf".to_string(), "/tmp/foo".to_string()];
        assert!(check_dangerous(&args, &patterns).is_some(), "rm -rf should match");

        // 2. git push --force
        let args = vec!["git".to_string(), "push".to_string(), "origin".to_string(), "main".to_string(), "--force".to_string()];
        assert!(check_dangerous(&args, &patterns).is_some(), "git push --force should match");

        // 3. git reset --hard
        let args = vec!["git".to_string(), "reset".to_string(), "--hard".to_string()];
        assert!(check_dangerous(&args, &patterns).is_some(), "git reset --hard should match");

        // 4. git clean -f
        let args = vec!["git".to_string(), "clean".to_string(), "-f".to_string()];
        assert!(check_dangerous(&args, &patterns).is_some(), "git clean -f should match");

        // 5. docker system prune
        let args = vec!["docker".to_string(), "system".to_string(), "prune".to_string()];
        assert!(check_dangerous(&args, &patterns).is_some(), "docker system prune should match");

        // 6. DROP TABLE
        let args = vec!["mysql".to_string(), "-e".to_string(), "DROP TABLE users".to_string()];
        assert!(check_dangerous(&args, &patterns).is_some(), "DROP TABLE should match");

        // 7. chmod -R 777
        let args = vec!["chmod".to_string(), "-R".to_string(), "777".to_string(), "/var".to_string()];
        assert!(check_dangerous(&args, &patterns).is_some(), "chmod -R 777 should match");

        // 8. dd
        let args = vec!["dd".to_string(), "if=/dev/zero".to_string(), "of=/dev/sda".to_string()];
        assert!(check_dangerous(&args, &patterns).is_some(), "dd should match");

        // 9. mkfs
        let args = vec!["mkfs.ext4".to_string(), "/dev/sda1".to_string()];
        assert!(check_dangerous(&args, &patterns).is_some(), "mkfs should match");
    }

    // Test 6: Warning display format — "⚠ cmd: reason -- proceed? [y/N]"
    #[test]
    fn test_warning_display_format() {
        let warning = format_warning("rm -rf /tmp/foo", "Force recursive delete");
        assert_eq!(
            warning,
            "\u{26a0} rm -rf /tmp/foo: Force recursive delete -- proceed? [y/N]"
        );

        let warning = format_warning(
            "git push origin main --force",
            "Overwrites remote history",
        );
        assert_eq!(
            warning,
            "\u{26a0} git push origin main --force: Overwrites remote history -- proceed? [y/N]"
        );
    }

    // Test 7: User confirmation flow — check_dangerous returns correct structure
    #[test]
    fn test_confirmation_flow() {
        let patterns = test_patterns();

        // Dangerous command: check_dangerous returns Some with warning and reason
        let args = vec!["rm".to_string(), "-rf".to_string(), "/".to_string()];
        let result = check_dangerous(&args, &patterns);
        assert!(result.is_some());
        let (warning, reason) = result.unwrap();
        assert!(warning.contains("\u{26a0}"));
        assert!(warning.contains("proceed? [y/N]"));
        assert_eq!(reason, "Force recursive delete");

        // Non-dangerous command: check_dangerous returns None (no confirmation needed)
        let args = vec!["ls".to_string(), "-la".to_string()];
        let result = check_dangerous(&args, &patterns);
        assert!(result.is_none());
    }

    // Test 8: Non-dangerous commands pass through — return None/no match
    #[test]
    fn test_non_dangerous_commands() {
        let patterns = test_patterns();

        // Safe commands should not match
        let safe_commands: Vec<Vec<String>> = vec![
            vec!["ls".to_string(), "-la".to_string()],
            vec!["cat".to_string(), "file.txt".to_string()],
            vec!["git".to_string(), "status".to_string()],
            vec!["git".to_string(), "push".to_string(), "origin".to_string(), "main".to_string()],
            vec!["git".to_string(), "reset".to_string(), "--soft".to_string(), "HEAD~1".to_string()],
            vec!["cp".to_string(), "a.txt".to_string(), "b.txt".to_string()],
            vec!["rm".to_string(), "file.txt".to_string()], // rm without -rf is fine
            vec!["docker".to_string(), "ps".to_string()],
            vec!["chmod".to_string(), "644".to_string(), "file.txt".to_string()],
        ];

        for cmd in &safe_commands {
            assert!(
                check_dangerous(cmd, &patterns).is_none(),
                "Command {:?} should NOT be dangerous",
                cmd
            );
        }

        // Empty args
        let empty: Vec<String> = vec![];
        assert!(check_dangerous(&empty, &patterns).is_none());
    }
}
