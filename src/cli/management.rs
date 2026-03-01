//! CLI management commands: `mish ps`, `mish logs`, `mish config check`.
//!
//! Out-of-band management for monitoring and configuring the mish server.

use std::fmt;
use std::io::BufRead;
use std::path::Path;

use crate::config::{validate_config, MishConfig};
use crate::shutdown::ShutdownManager;

// ---------------------------------------------------------------------------
// ManagementError
// ---------------------------------------------------------------------------

/// Errors from CLI management commands.
#[derive(Debug)]
pub enum ManagementError {
    Io(std::io::Error),
    Config(Vec<String>),
    NotFound(String),
}

impl fmt::Display for ManagementError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManagementError::Io(e) => write!(f, "I/O error: {e}"),
            ManagementError::Config(errors) => {
                write!(f, "config errors: {}", errors.join("; "))
            }
            ManagementError::NotFound(msg) => write!(f, "not found: {msg}"),
        }
    }
}

impl std::error::Error for ManagementError {}

impl From<std::io::Error> for ManagementError {
    fn from(e: std::io::Error) -> Self {
        ManagementError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// ServerInstance — represents a running mish process
// ---------------------------------------------------------------------------

/// A running (or stale) mish server instance discovered from PID files.
#[derive(Debug, Clone)]
pub struct ServerInstance {
    pub pid: u32,
    pub alive: bool,
}

// ---------------------------------------------------------------------------
// config check
// ---------------------------------------------------------------------------

/// Validate mish configuration file.
///
/// Returns `Ok(())` if valid, `Err(ManagementError::Config(errors))` otherwise.
pub fn config_check(path: Option<&str>) -> Result<(), ManagementError> {
    let config_path = path.unwrap_or("~/.config/mish/mish.toml");
    let expanded = expand_tilde(config_path);
    if !Path::new(&expanded).exists() && path.is_none() {
        // No config file at default location — mish uses valid defaults.
        return Ok(());
    }
    validate_config(config_path).map_err(ManagementError::Config)
}

/// Run `mish config check` command, printing results to stdout.
/// Returns exit code (0 = valid, 1 = errors).
pub fn cmd_config_check(path: Option<&str>) -> i32 {
    match config_check(path) {
        Ok(()) => {
            println!("+ config valid");
            0
        }
        Err(ManagementError::Config(errors)) => {
            println!("! config errors:");
            for e in &errors {
                println!("  - {e}");
            }
            1
        }
        Err(e) => {
            println!("! {e}");
            1
        }
    }
}

// ---------------------------------------------------------------------------
// ps — list server instances
// ---------------------------------------------------------------------------

/// List discovered mish server instances from PID files.
pub fn list_server_instances() -> Vec<ServerInstance> {
    let dir = ShutdownManager::pid_dir();
    let dir_path = Path::new(&dir);

    if !dir_path.exists() {
        return Vec::new();
    }

    let mut instances = Vec::new();

    if let Ok(entries) = std::fs::read_dir(dir_path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("pid") {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(pid) = content.trim().parse::<u32>() {
                        let stale = ShutdownManager::check_stale_pid(
                            &path.to_string_lossy(),
                        );
                        instances.push(ServerInstance {
                            pid,
                            alive: !stale,
                        });
                    }
                }
            }
        }
    }

    instances.sort_by_key(|i| i.pid);
    instances
}

/// Format server instances as a displayable table.
pub fn format_ps(instances: &[ServerInstance]) -> String {
    if instances.is_empty() {
        return "No mish server instances found.".to_string();
    }

    let mut lines = vec![format!("{:<10} {:<10}", "PID", "STATUS")];
    for inst in instances {
        let status = if inst.alive { "running" } else { "stale" };
        lines.push(format!("{:<10} {:<10}", inst.pid, status));
    }
    lines.join("\n")
}

/// Run `mish ps` command, printing results to stdout.
/// Returns exit code 0.
pub fn cmd_ps() -> i32 {
    let instances = list_server_instances();
    println!("{}", format_ps(&instances));
    0
}

// ---------------------------------------------------------------------------
// logs — tail audit log
// ---------------------------------------------------------------------------

/// Read the last `n` lines from a file.
pub fn read_last_n_lines(path: &str, n: usize) -> Result<Vec<String>, std::io::Error> {
    let expanded = expand_tilde(path);
    let file = std::fs::File::open(&expanded)?;
    let reader = std::io::BufReader::new(file);

    let all_lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;

    if all_lines.len() <= n {
        Ok(all_lines)
    } else {
        Ok(all_lines[all_lines.len() - n..].to_vec())
    }
}

/// Run `mish logs` command, printing last N audit log entries.
/// Returns exit code 0 on success, 1 on error.
pub fn cmd_logs(n: usize, config: &MishConfig) -> i32 {
    match read_last_n_lines(&config.audit.log_path, n) {
        Ok(lines) => {
            if lines.is_empty() {
                println!("(audit log empty)");
            } else {
                for line in &lines {
                    println!("{line}");
                }
            }
            0
        }
        Err(e) => {
            eprintln!("mish: cannot read audit log: {e}");
            1
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

use crate::util::expand_tilde;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    // ── Test 1: config_check with default path returns Ok ──

    #[test]
    fn test_config_check_default() {
        // When no config file exists, load_config returns defaults which are valid
        let result = config_check(None);
        assert!(
            result.is_ok(),
            "Default config should be valid: {:?}",
            result.err()
        );
    }

    // ── Test 2: config_check with valid config file ──

    #[test]
    fn test_config_check_valid_file() {
        let mut tmpfile = NamedTempFile::new().unwrap();
        write!(
            tmpfile,
            r#"
[server]
max_sessions = 5
max_processes = 20
"#
        )
        .unwrap();

        let result = config_check(Some(tmpfile.path().to_str().unwrap()));
        assert!(result.is_ok());
    }

    // ── Test 3: config_check with invalid config returns errors ──

    #[test]
    fn test_config_check_invalid() {
        let mut tmpfile = NamedTempFile::new().unwrap();
        write!(
            tmpfile,
            r#"
[server]
max_sessions = 0
max_processes = 0
"#
        )
        .unwrap();

        let result = config_check(Some(tmpfile.path().to_str().unwrap()));
        assert!(result.is_err());

        if let Err(ManagementError::Config(errors)) = result {
            assert!(
                !errors.is_empty(),
                "Should have validation errors"
            );
        } else {
            panic!("Expected Config error variant");
        }
    }

    // ── Test 4: format_ps with empty instances ──

    #[test]
    fn test_format_ps_empty() {
        let output = format_ps(&[]);
        assert!(output.contains("No mish server instances"));
    }

    // ── Test 5: format_ps with instances ──

    #[test]
    fn test_format_ps_with_instances() {
        let instances = vec![
            ServerInstance { pid: 1234, alive: true },
            ServerInstance { pid: 5678, alive: false },
        ];
        let output = format_ps(&instances);
        assert!(output.contains("PID"));
        assert!(output.contains("STATUS"));
        assert!(output.contains("1234"));
        assert!(output.contains("running"));
        assert!(output.contains("5678"));
        assert!(output.contains("stale"));
    }

    // ── Test 6: read_last_n_lines reads correct number ──

    #[test]
    fn test_read_last_n_lines() {
        let mut tmpfile = NamedTempFile::new().unwrap();
        for i in 1..=10 {
            writeln!(tmpfile, "line {i}").unwrap();
        }
        tmpfile.flush().unwrap();

        let lines = read_last_n_lines(
            tmpfile.path().to_str().unwrap(),
            3,
        )
        .unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "line 8");
        assert_eq!(lines[1], "line 9");
        assert_eq!(lines[2], "line 10");
    }

    // ── Test 7: read_last_n_lines with fewer lines than requested ──

    #[test]
    fn test_read_last_n_lines_fewer() {
        let mut tmpfile = NamedTempFile::new().unwrap();
        writeln!(tmpfile, "only one").unwrap();
        tmpfile.flush().unwrap();

        let lines = read_last_n_lines(
            tmpfile.path().to_str().unwrap(),
            50,
        )
        .unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "only one");
    }

    // ── Test 8: read_last_n_lines with empty file ──

    #[test]
    fn test_read_last_n_lines_empty() {
        let tmpfile = NamedTempFile::new().unwrap();
        let lines = read_last_n_lines(
            tmpfile.path().to_str().unwrap(),
            10,
        )
        .unwrap();
        assert!(lines.is_empty());
    }

    // ── Test 9: read_last_n_lines with nonexistent file ──

    #[test]
    fn test_read_last_n_lines_nonexistent() {
        let result = read_last_n_lines("/tmp/mish_nonexistent_test_file_zzz", 10);
        assert!(result.is_err());
    }

    // ── Test 10: ManagementError Display formatting ──

    #[test]
    fn test_management_error_display() {
        let io_err = ManagementError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "not found",
        ));
        assert!(format!("{io_err}").contains("not found"));

        let config_err = ManagementError::Config(vec![
            "err1".into(),
            "err2".into(),
        ]);
        let msg = format!("{config_err}");
        assert!(msg.contains("err1"));
        assert!(msg.contains("err2"));

        let not_found = ManagementError::NotFound("audit log".into());
        assert!(format!("{not_found}").contains("audit log"));
    }

    // ── Test 11: cmd_config_check returns 0 for valid ──

    #[test]
    fn test_cmd_config_check_exit_code_valid() {
        let mut tmpfile = NamedTempFile::new().unwrap();
        write!(
            tmpfile,
            r#"
[server]
max_sessions = 5
max_processes = 20
"#
        )
        .unwrap();

        let code = cmd_config_check(Some(tmpfile.path().to_str().unwrap()));
        assert_eq!(code, 0);
    }

    // ── Test 12: cmd_config_check returns 1 for invalid ──

    #[test]
    fn test_cmd_config_check_exit_code_invalid() {
        let mut tmpfile = NamedTempFile::new().unwrap();
        write!(
            tmpfile,
            r#"
[server]
max_sessions = 0
"#
        )
        .unwrap();

        let code = cmd_config_check(Some(tmpfile.path().to_str().unwrap()));
        assert_eq!(code, 1);
    }

    // ── Test 13: list_server_instances with no PID dir ──

    #[test]
    fn test_list_server_instances_no_dir() {
        // This tests the real PID dir which may or may not exist.
        // If no mish instances are running, it returns empty or instances.
        // The function should not panic either way.
        let _instances = list_server_instances();
    }

    // ── Test 14: expand_tilde helper ──

    #[test]
    fn test_expand_tilde() {
        let home = std::env::var("HOME").unwrap_or_default();
        let expanded = expand_tilde("~/test/path");
        assert_eq!(expanded, format!("{home}/test/path"));

        let plain = expand_tilde("/absolute/path");
        assert_eq!(plain, "/absolute/path");
    }
}
