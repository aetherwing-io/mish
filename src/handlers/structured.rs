//! Structured handler — execute, parse machine-readable output, format.
//!
//! Handles git status, docker ps, etc. May inject --porcelain/--format json.
//! Falls back to Generic(raw_output) for unrecognized commands.

use std::process::Command;

/// Result of a structured command execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredResult {
    /// The parsed structured data.
    pub parsed: StructuredData,
    /// Process exit code.
    pub exit_code: i32,
}

/// Parsed output from a structured command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StructuredData {
    /// Parsed `git status --porcelain=v2` output.
    GitStatus(GitStatusInfo),
    /// Parsed `docker ps --format json` output.
    DockerPs(Vec<DockerContainer>),
    /// Fallback: raw output for unrecognized commands.
    Generic(String),
}

/// Parsed git status information from porcelain v2 format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatusInfo {
    /// Current branch name.
    pub branch: String,
    /// Number of modified files.
    pub modified: u32,
    /// Number of added (staged new) files.
    pub added: u32,
    /// Number of deleted files.
    pub deleted: u32,
    /// Number of untracked files.
    pub untracked: u32,
}

/// A Docker container from `docker ps --format json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerContainer {
    /// Container ID.
    pub id: String,
    /// Container name.
    pub name: String,
    /// Container status.
    pub status: String,
    /// Container image.
    pub image: String,
}

// ---------------------------------------------------------------------------
// Parsing helpers (pure functions — testable without command execution)
// ---------------------------------------------------------------------------

/// Parse `git status --porcelain=v2` output into GitStatusInfo.
///
/// Porcelain v2 format:
/// - `# branch.head <name>` — branch name
/// - `1 XY ...` — ordinary changed entry
///   - X = index status, Y = worktree status
///   - `.` = not modified, `M` = modified, `A` = added, `D` = deleted
/// - `? path` — untracked file
pub fn parse_git_porcelain_v2(output: &str) -> GitStatusInfo {
    let mut branch = String::new();
    let mut modified: u32 = 0;
    let mut added: u32 = 0;
    let mut deleted: u32 = 0;
    let mut untracked: u32 = 0;

    for line in output.lines() {
        if line.starts_with("# branch.head ") {
            branch = line.strip_prefix("# branch.head ").unwrap_or("").to_string();
        } else if line.starts_with('?') {
            untracked += 1;
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            // Ordinary (1) or rename/copy (2) entry: "1 XY ..."
            let chars: Vec<char> = line.chars().collect();
            if chars.len() >= 4 {
                let x = chars[2]; // index status
                let y = chars[3]; // worktree status

                // Count index changes (staged)
                match x {
                    'A' => added += 1,
                    'D' => deleted += 1,
                    'M' => modified += 1,
                    _ => {}
                }

                // Count worktree changes (unstaged) — only if index didn't
                // already count this entry for the same type
                match y {
                    'M' => {
                        if x != 'M' {
                            modified += 1;
                        }
                    }
                    'D' => {
                        if x != 'D' {
                            deleted += 1;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    GitStatusInfo {
        branch,
        modified,
        added,
        deleted,
        untracked,
    }
}

/// Parse `docker ps --format json` output into a Vec of DockerContainer.
///
/// Docker outputs one JSON object per line.
pub fn parse_docker_json(output: &str) -> Vec<DockerContainer> {
    let mut containers = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse each line as a JSON object
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            let id = val
                .get("ID")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = val
                .get("Names")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let status = val
                .get("Status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let image = val
                .get("Image")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            containers.push(DockerContainer {
                id,
                name,
                status,
                image,
            });
        }
    }

    containers
}

// ---------------------------------------------------------------------------
// Pipeline-callable formatting
// ---------------------------------------------------------------------------

/// Parse already-captured output into structured data — without executing
/// the command. Used by the MCP pipeline path where the command has already
/// run and we just need to parse the output.
pub fn parse_structured(cmd: &str, args: &[String], output: &str, exit_code: i32) -> StructuredResult {
    if is_git_status(cmd, args) {
        StructuredResult {
            parsed: StructuredData::GitStatus(parse_git_porcelain_v2(output)),
            exit_code,
        }
    } else if is_docker_ps(cmd, args) {
        StructuredResult {
            parsed: StructuredData::DockerPs(parse_docker_json(output)),
            exit_code,
        }
    } else {
        StructuredResult {
            parsed: StructuredData::Generic(output.to_string()),
            exit_code,
        }
    }
}

// ---------------------------------------------------------------------------
// Command execution
// ---------------------------------------------------------------------------

/// Execute a structured command: detect the tool, inject machine-readable
/// flags, parse the output, and return structured data.
pub fn handle(args: &[String]) -> Result<StructuredResult, Box<dyn std::error::Error>> {
    if args.is_empty() {
        return Err("structured: empty command".into());
    }

    let cmd_name = &args[0];

    // Detect which structured handler to use based on command name
    if is_git_status(cmd_name, args) {
        handle_git_status(args)
    } else if is_docker_ps(cmd_name, args) {
        handle_docker_ps(args)
    } else {
        handle_generic(args)
    }
}

/// Check if args represent a `git status` invocation.
fn is_git_status(cmd: &str, args: &[String]) -> bool {
    (cmd == "git" || cmd.ends_with("/git"))
        && args.len() >= 2
        && args[1] == "status"
}

/// Check if args represent a `docker ps` invocation.
fn is_docker_ps(cmd: &str, args: &[String]) -> bool {
    (cmd == "docker" || cmd.ends_with("/docker"))
        && args.len() >= 2
        && args[1] == "ps"
}

/// Execute `git status` with `--porcelain=v2` injected.
fn handle_git_status(args: &[String]) -> Result<StructuredResult, Box<dyn std::error::Error>> {
    let mut cmd_args: Vec<String> = args.to_vec();

    // Inject --porcelain=v2 if not already present
    let has_porcelain = cmd_args.iter().any(|a| a.starts_with("--porcelain"));
    if !has_porcelain {
        // Insert after "status"
        let status_idx = cmd_args.iter().position(|a| a == "status").unwrap_or(1);
        cmd_args.insert(status_idx + 1, "--porcelain=v2".to_string());
    }

    // Also inject --branch if not present (to get branch info)
    let has_branch = cmd_args.iter().any(|a| a == "--branch" || a == "-b");
    if !has_branch {
        let porcelain_idx = cmd_args
            .iter()
            .position(|a| a.starts_with("--porcelain"))
            .unwrap_or(2);
        cmd_args.insert(porcelain_idx + 1, "--branch".to_string());
    }

    let output = Command::new(&cmd_args[0])
        .args(&cmd_args[1..])
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let exit_code = output.status.code().unwrap_or(-1);

    let info = parse_git_porcelain_v2(&stdout);

    Ok(StructuredResult {
        parsed: StructuredData::GitStatus(info),
        exit_code,
    })
}

/// Execute `docker ps` with `--format json` injected.
fn handle_docker_ps(args: &[String]) -> Result<StructuredResult, Box<dyn std::error::Error>> {
    let mut cmd_args: Vec<String> = args.to_vec();

    // Inject --format json if not already present
    let has_format = cmd_args.iter().any(|a| a.starts_with("--format"));
    if !has_format {
        cmd_args.push("--format".to_string());
        cmd_args.push("json".to_string());
    }

    let output = Command::new(&cmd_args[0])
        .args(&cmd_args[1..])
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let exit_code = output.status.code().unwrap_or(-1);

    let containers = parse_docker_json(&stdout);

    Ok(StructuredResult {
        parsed: StructuredData::DockerPs(containers),
        exit_code,
    })
}

/// Fallback: run command normally, return Generic(raw output).
fn handle_generic(args: &[String]) -> Result<StructuredResult, Box<dyn std::error::Error>> {
    let output = Command::new(&args[0])
        .args(&args[1..])
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let exit_code = output.status.code().unwrap_or(-1);

    Ok(StructuredResult {
        parsed: StructuredData::Generic(stdout.into_owned()),
        exit_code,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Test 3: git status parsing — porcelain v2 format
    #[test]
    fn test_git_status_parsing() {
        let porcelain_output = "\
# branch.oid abc123def456
# branch.head main
1 .M N... 100644 100644 100644 abc123 def456 src/main.rs
1 A. N... 000000 100644 100644 000000 abc123 src/new_file.rs
1 D. N... 100644 000000 000000 abc123 000000 src/old_file.rs
1 .M N... 100644 100644 100644 abc123 def456 src/lib.rs
? untracked_file.txt
? another_untracked.rs
";

        let info = parse_git_porcelain_v2(porcelain_output);

        assert_eq!(info.branch, "main");
        assert_eq!(info.modified, 2, "expected 2 modified files (.M entries)");
        assert_eq!(info.added, 1, "expected 1 added file (A. entry)");
        assert_eq!(info.deleted, 1, "expected 1 deleted file (D. entry)");
        assert_eq!(info.untracked, 2, "expected 2 untracked files (? entries)");
    }

    // Test 4: docker ps parsing — JSON format
    #[test]
    fn test_docker_ps_parsing() {
        let docker_output = r#"{"ID":"abc123","Names":"web-app","Status":"Up 2 hours","Image":"nginx:latest"}
{"ID":"def456","Names":"db","Status":"Up 3 hours","Image":"postgres:15"}
{"ID":"ghi789","Names":"cache","Status":"Exited (0) 1 hour ago","Image":"redis:7"}
"#;

        let containers = parse_docker_json(docker_output);

        assert_eq!(containers.len(), 3);

        assert_eq!(containers[0].id, "abc123");
        assert_eq!(containers[0].name, "web-app");
        assert_eq!(containers[0].status, "Up 2 hours");
        assert_eq!(containers[0].image, "nginx:latest");

        assert_eq!(containers[1].id, "def456");
        assert_eq!(containers[1].name, "db");
        assert_eq!(containers[1].status, "Up 3 hours");
        assert_eq!(containers[1].image, "postgres:15");

        assert_eq!(containers[2].id, "ghi789");
        assert_eq!(containers[2].name, "cache");
        assert_eq!(containers[2].status, "Exited (0) 1 hour ago");
        assert_eq!(containers[2].image, "redis:7");
    }

    // Test 5: structured fallback to Generic — unknown command
    #[test]
    fn test_structured_fallback_generic() {
        // Use echo as a stand-in for an unknown structured command
        let args: Vec<String> = vec!["echo", "hello world"]
            .into_iter()
            .map(String::from)
            .collect();

        let result = handle(&args).expect("handle should succeed for echo");

        match &result.parsed {
            StructuredData::Generic(output) => {
                assert!(
                    output.contains("hello world"),
                    "generic fallback should contain command output, got: {:?}",
                    output
                );
            }
            other => panic!(
                "expected Generic variant for unknown command, got: {:?}",
                other
            ),
        }
        assert_eq!(result.exit_code, 0);
    }

    // Test 8: git status with no changes
    #[test]
    fn test_git_status_no_changes() {
        let porcelain_output = "\
# branch.oid abc123def456
# branch.head feature/clean
";

        let info = parse_git_porcelain_v2(porcelain_output);

        assert_eq!(info.branch, "feature/clean");
        assert_eq!(info.modified, 0);
        assert_eq!(info.added, 0);
        assert_eq!(info.deleted, 0);
        assert_eq!(info.untracked, 0);
    }

    // Additional: empty porcelain output
    #[test]
    fn test_git_status_empty_output() {
        let info = parse_git_porcelain_v2("");
        assert_eq!(info.branch, "");
        assert_eq!(info.modified, 0);
        assert_eq!(info.added, 0);
        assert_eq!(info.deleted, 0);
        assert_eq!(info.untracked, 0);
    }

    // Additional: docker ps empty output (no containers)
    #[test]
    fn test_docker_ps_empty() {
        let containers = parse_docker_json("");
        assert!(containers.is_empty());
    }

    // Additional: docker ps with blank lines
    #[test]
    fn test_docker_ps_blank_lines() {
        let docker_output = r#"
{"ID":"abc","Names":"test","Status":"Up","Image":"alpine"}

"#;
        let containers = parse_docker_json(docker_output);
        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].id, "abc");
    }

    // Additional: git status with both index and worktree changes on same file
    #[test]
    fn test_git_status_both_index_and_worktree() {
        // MM = modified in both index and worktree
        let porcelain_output = "\
# branch.head develop
1 MM N... 100644 100644 100644 abc123 def456 src/both.rs
";
        let info = parse_git_porcelain_v2(porcelain_output);
        assert_eq!(info.branch, "develop");
        // MM: M in index = 1 modified, M in worktree but same type so not double-counted
        assert_eq!(info.modified, 1);
    }

    // Additional: is_git_status detection
    #[test]
    fn test_is_git_status_detection() {
        assert!(is_git_status(
            "git",
            &["git".to_string(), "status".to_string()]
        ));
        assert!(!is_git_status(
            "git",
            &["git".to_string(), "log".to_string()]
        ));
        assert!(!is_git_status("docker", &["docker".to_string()]));
    }

    // Additional: is_docker_ps detection
    #[test]
    fn test_is_docker_ps_detection() {
        assert!(is_docker_ps(
            "docker",
            &["docker".to_string(), "ps".to_string()]
        ));
        assert!(!is_docker_ps(
            "docker",
            &["docker".to_string(), "run".to_string()]
        ));
        assert!(!is_docker_ps("git", &["git".to_string()]));
    }

    // -----------------------------------------------------------------------
    // parse_structured() — pipeline-callable function tests
    // -----------------------------------------------------------------------

    // Test: parse_structured detects git status and parses porcelain v2
    #[test]
    fn test_parse_structured_git_status() {
        let args = vec!["git".to_string(), "status".to_string()];
        let output = "\
# branch.oid abc123
# branch.head main
1 .M N... 100644 100644 100644 abc123 def456 src/main.rs
? untracked.txt
";
        let result = parse_structured("git", &args, output, 0);
        assert_eq!(result.exit_code, 0);
        match result.parsed {
            StructuredData::GitStatus(info) => {
                assert_eq!(info.branch, "main");
                assert_eq!(info.modified, 1);
                assert_eq!(info.untracked, 1);
            }
            other => panic!("expected GitStatus, got: {:?}", other),
        }
    }

    // Test: parse_structured detects docker ps and parses JSON
    #[test]
    fn test_parse_structured_docker_ps() {
        let args = vec!["docker".to_string(), "ps".to_string()];
        let output = r#"{"ID":"abc","Names":"web","Status":"Up","Image":"nginx"}"#;
        let result = parse_structured("docker", &args, output, 0);
        assert_eq!(result.exit_code, 0);
        match result.parsed {
            StructuredData::DockerPs(containers) => {
                assert_eq!(containers.len(), 1);
                assert_eq!(containers[0].id, "abc");
                assert_eq!(containers[0].name, "web");
            }
            other => panic!("expected DockerPs, got: {:?}", other),
        }
    }

    // Test: parse_structured falls back to Generic for unknown commands
    #[test]
    fn test_parse_structured_generic_fallback() {
        let args = vec!["unknown".to_string(), "cmd".to_string()];
        let output = "some raw output";
        let result = parse_structured("unknown", &args, output, 42);
        assert_eq!(result.exit_code, 42);
        match result.parsed {
            StructuredData::Generic(s) => {
                assert_eq!(s, "some raw output");
            }
            other => panic!("expected Generic, got: {:?}", other),
        }
    }

    // Test: parse_structured with non-zero exit code
    #[test]
    fn test_parse_structured_nonzero_exit() {
        let args = vec!["git".to_string(), "status".to_string()];
        let result = parse_structured("git", &args, "", 128);
        assert_eq!(result.exit_code, 128);
    }

    // Test: parse_structured with git path prefix
    #[test]
    fn test_parse_structured_git_path_prefix() {
        let args = vec!["/usr/bin/git".to_string(), "status".to_string()];
        let output = "# branch.head feature\n";
        let result = parse_structured("/usr/bin/git", &args, output, 0);
        match result.parsed {
            StructuredData::GitStatus(info) => {
                assert_eq!(info.branch, "feature");
            }
            other => panic!("expected GitStatus, got: {:?}", other),
        }
    }

    // Test: parse_structured with empty output
    #[test]
    fn test_parse_structured_empty_output() {
        let args = vec!["git".to_string(), "status".to_string()];
        let result = parse_structured("git", &args, "", 0);
        match result.parsed {
            StructuredData::GitStatus(info) => {
                assert_eq!(info.branch, "");
                assert_eq!(info.modified, 0);
            }
            other => panic!("expected GitStatus, got: {:?}", other),
        }
    }
}
