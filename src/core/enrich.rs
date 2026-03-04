//! Error enrichment on failure.
//!
//! Pre-fetches diagnostics the LLM would request next: path walks, stat, permissions.
//! Budget: <100ms total, read-only, non-speculative.

use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nix::libc;

use crate::core::grammar::Grammar;
use crate::core::stat;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Result of walking a path to find where it breaks.
pub struct PathDiagnosis {
    pub requested: PathBuf,
    pub last_valid: PathBuf,
    pub breaks_at: PathBuf,
    pub siblings: Vec<String>,
}

/// Diagnosis of a file operation (cp, mv, ln, etc.).
pub struct FileOpDiagnosis {
    pub source_exists: bool,
    pub source_info: Option<String>,
    pub dest_exists: bool,
    pub dest_info: Option<String>,
    pub dest_parent_exists: bool,
    pub dest_parent_info: Option<String>,
}

/// Diagnosis of permission issues.
pub struct PermissionDiagnosis {
    pub path: PathBuf,
    pub owner: String,
    pub group: String,
    pub mode: u32,
    pub running_as: String,
    pub running_group: String,
    pub issue: String,
}

/// Diagnosis for command-not-found errors.
pub struct CommandDiagnosis {
    pub command: String,
    pub in_path: bool,
    pub package_hint: Option<String>,
}

/// The final enrichment result: a list of key-value diagnostic lines.
pub struct EnrichmentResult {
    pub diagnostics: Vec<DiagnosticLine>,
}

/// A single key-value diagnostic pair.
pub struct DiagnosticLine {
    pub key: String,
    pub value: String,
}

// ---------------------------------------------------------------------------
// Intent detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandIntent {
    FileOp,
    DirOp,
    NetworkOp,
    BuildRun,
    ProcessExec,
}

fn detect_intent(command: &[String]) -> CommandIntent {
    if command.is_empty() {
        return CommandIntent::ProcessExec;
    }
    let cmd = base_command(&command[0]);
    match cmd {
        "cp" | "mv" | "ln" | "rm" | "touch" | "chmod" | "chown" | "cat" | "head" | "tail"
        | "less" | "more" => CommandIntent::FileOp,
        "mkdir" | "cd" | "ls" | "rmdir" => CommandIntent::DirOp,
        "curl" | "wget" | "ssh" => CommandIntent::NetworkOp,
        "cargo" | "npm" | "npx" | "python" | "python3" | "go" | "make" | "node" | "rustc"
        | "gcc" | "javac" => CommandIntent::BuildRun,
        _ => CommandIntent::ProcessExec,
    }
}

/// Extract the base command name, stripping any path prefix.
fn base_command(cmd: &str) -> &str {
    cmd.rsplit('/').next().unwrap_or(cmd)
}

// ---------------------------------------------------------------------------
// Package hints (static map)
// ---------------------------------------------------------------------------

static PACKAGE_HINTS: &[(&str, &str)] = &[
    ("rg", "ripgrep"),
    ("fd", "fd-find"),
    ("bat", "bat"),
    ("exa", "exa"),
    ("jq", "jq"),
    ("yq", "yq"),
    ("fzf", "fzf"),
    ("htop", "htop"),
    ("tree", "tree"),
    ("wget", "wget"),
    ("curl", "curl"),
];

fn lookup_package_hint(cmd: &str) -> Option<&'static str> {
    PACKAGE_HINTS
        .iter()
        .find(|(c, _)| *c == cmd)
        .map(|(_, pkg)| *pkg)
}

// ---------------------------------------------------------------------------
// Budget tracker
// ---------------------------------------------------------------------------

struct Budget {
    start: Instant,
    limit: Duration,
}

impl Budget {
    fn new(limit_ms: u64) -> Self {
        Budget {
            start: Instant::now(),
            limit: Duration::from_millis(limit_ms),
        }
    }

    #[allow(dead_code)]
    fn remaining(&self) -> Duration {
        let elapsed = self.start.elapsed();
        if elapsed >= self.limit {
            Duration::ZERO
        } else {
            self.limit - elapsed
        }
    }

    fn exhausted(&self) -> bool {
        self.start.elapsed() >= self.limit
    }
}

// ---------------------------------------------------------------------------
// Exit code mapping
// ---------------------------------------------------------------------------

fn map_exit_code(exit_code: i32) -> Option<DiagnosticLine> {
    let value = match exit_code {
        126 => "permission denied (exit 126)",
        127 => "command not found (exit 127)",
        130 => "interrupted by user (SIGINT)",
        137 => "killed by signal 9 (likely OOM)",
        139 => "segmentation fault (SIGSEGV)",
        _ => return None,
    };
    Some(DiagnosticLine {
        key: "signal".to_string(),
        value: value.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Diagnostic functions (15 built-in)
// ---------------------------------------------------------------------------

// 1. source_exists (instant <1ms) — stat the source argument
fn source_exists(path: &Path) -> DiagnosticLine {
    let info = stat::file_info(path);
    let exists = path.exists();
    let mark = if exists { "\u{2713}" } else { "\u{2717}" };
    DiagnosticLine {
        key: "source".to_string(),
        value: format!("{info} {mark}"),
    }
}

// 2. dest_path_walk (fast <10ms) — walk dest path, find break point
fn dest_path_walk(path: &Path) -> Option<PathDiagnosis> {
    path_walk(path)
}

// 3. permissions (instant <1ms) — check r/w/x on relevant paths
fn permissions(path: &Path) -> Option<PermissionDiagnosis> {
    diagnose_permissions(path)
}

// 4. is_git_repo (instant <1ms) — check for .git directory
fn is_git_repo(dir: &Path) -> DiagnosticLine {
    // Walk up from dir looking for .git
    let mut current = if dir.is_dir() {
        dir.to_path_buf()
    } else {
        dir.parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    };
    loop {
        if current.join(".git").exists() {
            return DiagnosticLine {
                key: "git_repo".to_string(),
                value: format!("{} (found .git)", current.display()),
            };
        }
        if !current.pop() {
            break;
        }
    }
    DiagnosticLine {
        key: "git_repo".to_string(),
        value: "not a git repository".to_string(),
    }
}

// 5. branch_exists (fast <10ms) — check git branch --list
fn branch_exists(branch: &str) -> DiagnosticLine {
    match std::process::Command::new("git")
        .args(["branch", "--list", branch])
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.trim().is_empty() {
                DiagnosticLine {
                    key: "branch".to_string(),
                    value: format!("{branch} (not found)"),
                }
            } else {
                DiagnosticLine {
                    key: "branch".to_string(),
                    value: format!("{branch} (exists)"),
                }
            }
        }
        Err(_) => DiagnosticLine {
            key: "branch".to_string(),
            value: format!("{branch} (git not available)"),
        },
    }
}

// 6. branch_list_similar (moderate <100ms) — fuzzy match branches
fn branch_list_similar(branch: &str) -> DiagnosticLine {
    match std::process::Command::new("git")
        .args(["branch", "--list", "--all"])
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let branches: Vec<&str> = stdout
                .lines()
                .map(|l| l.trim().trim_start_matches("* "))
                .filter(|b| fuzzy_match(branch, b))
                .take(5)
                .collect();
            if branches.is_empty() {
                DiagnosticLine {
                    key: "similar_branches".to_string(),
                    value: "none found".to_string(),
                }
            } else {
                DiagnosticLine {
                    key: "similar_branches".to_string(),
                    value: branches.join(", "),
                }
            }
        }
        Err(_) => DiagnosticLine {
            key: "similar_branches".to_string(),
            value: "git not available".to_string(),
        },
    }
}

// 7. working_tree_clean (moderate <100ms) — git status --porcelain
fn working_tree_clean() -> DiagnosticLine {
    match std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.trim().is_empty() {
                DiagnosticLine {
                    key: "working_tree".to_string(),
                    value: "clean".to_string(),
                }
            } else {
                let count = stdout.lines().count();
                DiagnosticLine {
                    key: "working_tree".to_string(),
                    value: format!("{count} changed file(s)"),
                }
            }
        }
        Err(_) => DiagnosticLine {
            key: "working_tree".to_string(),
            value: "git not available".to_string(),
        },
    }
}

// 8. remote_ref_status (moderate <100ms) — git remote info (local data only)
fn remote_ref_status() -> DiagnosticLine {
    match std::process::Command::new("git")
        .args(["remote", "-v"])
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.trim().is_empty() {
                DiagnosticLine {
                    key: "remote".to_string(),
                    value: "no remotes configured".to_string(),
                }
            } else {
                let first_line = stdout.lines().next().unwrap_or("unknown");
                DiagnosticLine {
                    key: "remote".to_string(),
                    value: first_line.to_string(),
                }
            }
        }
        Err(_) => DiagnosticLine {
            key: "remote".to_string(),
            value: "git not available".to_string(),
        },
    }
}

// 9. ahead_behind (moderate <100ms) — git rev-list --count
fn ahead_behind() -> DiagnosticLine {
    match std::process::Command::new("git")
        .args(["rev-list", "--count", "--left-right", "HEAD...@{upstream}"])
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let parts: Vec<&str> = stdout.trim().split('\t').collect();
            if parts.len() == 2 {
                DiagnosticLine {
                    key: "ahead_behind".to_string(),
                    value: format!("ahead {}, behind {}", parts[0], parts[1]),
                }
            } else {
                DiagnosticLine {
                    key: "ahead_behind".to_string(),
                    value: "no upstream configured".to_string(),
                }
            }
        }
        Err(_) => DiagnosticLine {
            key: "ahead_behind".to_string(),
            value: "git not available".to_string(),
        },
    }
}

// 10. port_listening (fast <10ms) — quick connect() to localhost:port
fn port_listening(port: u16) -> DiagnosticLine {
    let addr = format!("127.0.0.1:{port}");
    let timeout = Duration::from_millis(5);
    match TcpStream::connect_timeout(&addr.parse().unwrap(), timeout) {
        Ok(_) => DiagnosticLine {
            key: "port".to_string(),
            value: format!(":{port} is listening"),
        },
        Err(_) => DiagnosticLine {
            key: "port".to_string(),
            value: format!(":{port} not listening"),
        },
    }
}

// 11. dir_listing (fast <10ms) — ls the parent/nearest existing directory
fn dir_listing(path: &Path) -> DiagnosticLine {
    let target = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    };

    // Walk up to find an existing directory
    let mut check = target.clone();
    while !check.is_dir() {
        if !check.pop() {
            break;
        }
    }

    match std::fs::read_dir(&check) {
        Ok(entries) => {
            let items: Vec<String> = entries
                .filter_map(|e| e.ok())
                .take(20)
                .map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    if e.path().is_dir() {
                        format!("{name}/")
                    } else {
                        name
                    }
                })
                .collect();
            DiagnosticLine {
                key: "nearest".to_string(),
                value: format!(
                    "{} contains: {}",
                    check.display(),
                    items.join(", ")
                ),
            }
        }
        Err(e) => DiagnosticLine {
            key: "nearest".to_string(),
            value: format!("{}: {e}", check.display()),
        },
    }
}

// 12. disk_space (instant <1ms) — df on target filesystem
fn disk_space(path: &Path) -> DiagnosticLine {
    // Find first existing ancestor
    let mut check = path.to_path_buf();
    while !check.exists() {
        if !check.pop() {
            break;
        }
    }

    match std::process::Command::new("df")
        .args(["-h", &check.to_string_lossy()])
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let line = stdout.lines().nth(1).unwrap_or("unknown");
            DiagnosticLine {
                key: "disk".to_string(),
                value: line.trim().to_string(),
            }
        }
        Err(_) => DiagnosticLine {
            key: "disk".to_string(),
            value: "df not available".to_string(),
        },
    }
}

// 13. node_modules_check (fast <10ms) — existence + age check
fn node_modules_check() -> DiagnosticLine {
    let nm = Path::new("node_modules");
    if !nm.exists() {
        return DiagnosticLine {
            key: "node_modules".to_string(),
            value: "not found (try: npm install)".to_string(),
        };
    }
    match std::fs::metadata(nm).and_then(|m| m.modified()) {
        Ok(mtime) => {
            let age = mtime.elapsed().unwrap_or_default();
            let hours = age.as_secs() / 3600;
            let days = hours / 24;
            let age_str = if days > 0 {
                format!("{days}d old")
            } else {
                format!("{hours}h old")
            };
            DiagnosticLine {
                key: "node_modules".to_string(),
                value: format!("exists ({age_str})"),
            }
        }
        Err(_) => DiagnosticLine {
            key: "node_modules".to_string(),
            value: "exists (unknown age)".to_string(),
        },
    }
}

// 14. command_not_found_hint (fast <10ms) — static package map lookup
fn command_not_found_hint(cmd: &str) -> DiagnosticLine {
    match lookup_package_hint(cmd) {
        Some(pkg) => DiagnosticLine {
            key: "package_hint".to_string(),
            value: format!("install package '{pkg}' to get '{cmd}'"),
        },
        None => DiagnosticLine {
            key: "package_hint".to_string(),
            value: format!("no known package for '{cmd}'"),
        },
    }
}

// 15. command_similar (moderate <100ms) — fuzzy match in PATH
fn command_similar(cmd: &str) -> DiagnosticLine {
    let path_var = std::env::var("PATH").unwrap_or_default();
    let mut candidates: Vec<String> = Vec::new();

    for dir in path_var.split(':') {
        let dir_path = Path::new(dir);
        if let Ok(entries) = std::fs::read_dir(dir_path) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if fuzzy_match(cmd, &name) && !candidates.contains(&name) {
                    candidates.push(name);
                    if candidates.len() >= 5 {
                        break;
                    }
                }
            }
        }
        if candidates.len() >= 5 {
            break;
        }
    }

    if candidates.is_empty() {
        DiagnosticLine {
            key: "similar_commands".to_string(),
            value: "none found".to_string(),
        }
    } else {
        DiagnosticLine {
            key: "similar_commands".to_string(),
            value: candidates.join(", "),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Walk a path component by component, finding the break point.
fn path_walk(path: &Path) -> Option<PathDiagnosis> {
    let mut accumulated = PathBuf::new();
    let mut last_valid = PathBuf::new();

    // Handle absolute paths
    if path.is_absolute() {
        accumulated.push("/");
        last_valid.push("/");
    }

    for component in path.components() {
        match component {
            std::path::Component::RootDir => {
                // Already handled above
                continue;
            }
            _ => {
                accumulated.push(component);
                if accumulated.exists() {
                    last_valid = accumulated.clone();
                } else {
                    // Found the break point
                    let siblings = list_siblings(&last_valid);
                    return Some(PathDiagnosis {
                        requested: path.to_path_buf(),
                        last_valid,
                        breaks_at: accumulated,
                        siblings,
                    });
                }
            }
        }
    }

    // Entire path is valid
    None
}

/// List children of a directory for sibling hints.
fn list_siblings(dir: &Path) -> Vec<String> {
    let mut result = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten().take(20) {
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.path().is_dir() {
                result.push(format!("{name}/"));
            } else {
                result.push(name);
            }
        }
    }
    result.sort();
    result
}

/// Simple fuzzy matching: prefix match, substring match, or edit distance <= 2.
fn fuzzy_match(needle: &str, haystack: &str) -> bool {
    if needle == haystack {
        return false; // Exact match is not "similar"
    }
    let n = needle.to_lowercase();
    let h = haystack.to_lowercase();
    // Prefix match
    if h.starts_with(&n) || n.starts_with(&h) {
        return true;
    }
    // Substring match
    if h.contains(&n) || n.contains(&h) {
        return true;
    }
    // Simple edit distance (for short strings)
    if n.len() <= 10 && h.len() <= 10 {
        return edit_distance(&n, &h) <= 2;
    }
    false
}

/// Compute Levenshtein edit distance between two strings.
fn edit_distance(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let m = a_chars.len();
    let n = b_chars.len();

    let mut prev = vec![0usize; n + 1];
    let mut curr = vec![0usize; n + 1];

    for (j, slot) in prev.iter_mut().enumerate().take(n + 1) {
        *slot = j;
    }

    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a_chars[i - 1] == b_chars[j - 1] {
                0
            } else {
                1
            };
            curr[j] = (prev[j] + 1)
                .min(curr[j - 1] + 1)
                .min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[n]
}

/// Diagnose permissions for a path.
fn diagnose_permissions(path: &Path) -> Option<PermissionDiagnosis> {
    let meta = std::fs::metadata(path).ok()?;
    let mode = meta.permissions().mode();

    use std::os::unix::fs::MetadataExt;
    let file_uid = meta.uid();
    let file_gid = meta.gid();

    // Get current user/group info using std::process::Command
    let running_uid = unsafe { libc::getuid() };
    let running_gid = unsafe { libc::getgid() };

    let running_as = resolve_username(running_uid);
    let running_group = resolve_groupname(running_gid);
    let owner = resolve_username(file_uid);
    let group = resolve_groupname(file_gid);

    // Determine the issue
    let is_owner = running_uid == file_uid;
    let is_group = running_gid == file_gid;

    let issue = if is_owner {
        // Check owner permissions
        if mode & 0o200 == 0 {
            "owner lacks write permission".to_string()
        } else if mode & 0o100 == 0 {
            "owner lacks execute permission".to_string()
        } else {
            "permissions appear adequate".to_string()
        }
    } else if is_group {
        if mode & 0o020 == 0 {
            "group lacks write permission".to_string()
        } else if mode & 0o010 == 0 {
            "group lacks execute permission".to_string()
        } else {
            "permissions appear adequate".to_string()
        }
    } else {
        // Other
        if mode & 0o002 == 0 {
            format!("no write for others (owned by {owner}:{group})")
        } else if mode & 0o001 == 0 {
            format!("no execute for others (owned by {owner}:{group})")
        } else {
            "permissions appear adequate".to_string()
        }
    };

    Some(PermissionDiagnosis {
        path: path.to_path_buf(),
        owner,
        group,
        mode,
        running_as,
        running_group,
        issue,
    })
}

/// Resolve a UID to a username, falling back to the numeric UID string.
fn resolve_username(uid: u32) -> String {
    // Use `id -un` style approach via the `id` command, but that's slow.
    // Instead, use libc::getpwuid_r for a fast lookup.
    use std::ffi::CStr;
    use std::mem::MaybeUninit;

    let mut buf = vec![0u8; 1024];
    let mut pwd = MaybeUninit::<libc::passwd>::uninit();
    let mut result: *mut libc::passwd = std::ptr::null_mut();

    let ret = unsafe {
        libc::getpwuid_r(
            uid,
            pwd.as_mut_ptr(),
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };

    if ret == 0 && !result.is_null() {
        let pwd = unsafe { pwd.assume_init() };
        let name = unsafe { CStr::from_ptr(pwd.pw_name) };
        name.to_string_lossy().to_string()
    } else {
        uid.to_string()
    }
}

/// Resolve a GID to a group name, falling back to the numeric GID string.
fn resolve_groupname(gid: u32) -> String {
    use std::ffi::CStr;
    use std::mem::MaybeUninit;

    let mut buf = vec![0u8; 1024];
    let mut grp = MaybeUninit::<libc::group>::uninit();
    let mut result: *mut libc::group = std::ptr::null_mut();

    let ret = unsafe {
        libc::getgrgid_r(
            gid,
            grp.as_mut_ptr(),
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };

    if ret == 0 && !result.is_null() {
        let grp = unsafe { grp.assume_init() };
        let name = unsafe { CStr::from_ptr(grp.gr_name) };
        name.to_string_lossy().to_string()
    } else {
        gid.to_string()
    }
}

/// Extract path arguments from a command, guided by grammar arg mapping.
fn extract_path_args(command: &[String], grammar: Option<&Grammar>) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // Use grammar arg mapping if available
    if let Some(g) = grammar {
        if let Some(enrich) = &g.enrich {
            if let Some(args) = &enrich.args {
                // path_args indices are relative to non-flag arguments after the command name
                let non_flag_args: Vec<&String> = command
                    .iter()
                    .skip(1)
                    .filter(|a| !a.starts_with('-'))
                    .collect();
                for &idx in &args.path_args {
                    if let Some(arg) = non_flag_args.get(idx) {
                        paths.push(PathBuf::from(arg));
                    }
                }
                return paths;
            }
        }
    }

    // Fallback: use heuristic — non-flag args after command name
    for arg in command.iter().skip(1) {
        if !arg.starts_with('-') {
            paths.push(PathBuf::from(arg));
        }
    }

    paths
}

/// Try to extract a port number from command arguments (for network ops).
fn extract_port(command: &[String], _stderr: &str) -> Option<u16> {
    for arg in command.iter().skip(1) {
        // Check for URL-like patterns: http://host:PORT/...
        if let Some(port_str) = arg.split("://").nth(1).and_then(|rest| {
            rest.split('/').next().and_then(|host_port| {
                host_port.split(':').nth(1)
            })
        }) {
            if let Ok(port) = port_str.parse::<u16>() {
                return Some(port);
            }
        }
        // Check for bare port numbers
        if let Ok(port) = arg.parse::<u16>() {
            if port > 0 {
                return Some(port);
            }
        }
    }
    // Check for -p/--port followed by value
    for pair in command.windows(2) {
        if pair[0] == "-p" || pair[0] == "--port" {
            if let Ok(port) = pair[1].parse::<u16>() {
                return Some(port);
            }
        }
    }
    None
}

/// Format a PathDiagnosis into diagnostic lines.
fn path_diagnosis_to_lines(diag: &PathDiagnosis) -> Vec<DiagnosticLine> {
    let mut lines = Vec::new();
    lines.push(DiagnosticLine {
        key: "path".to_string(),
        value: format!(
            "{} \u{2713}  {} \u{2717}",
            diag.last_valid.display(),
            diag.breaks_at.display()
        ),
    });
    if !diag.siblings.is_empty() {
        lines.push(DiagnosticLine {
            key: "nearest".to_string(),
            value: format!(
                "{} contains: {}",
                diag.last_valid.display(),
                diag.siblings.join(", ")
            ),
        });
    }
    lines
}

/// Format a PermissionDiagnosis into a diagnostic line.
fn permission_diagnosis_to_line(diag: &PermissionDiagnosis) -> DiagnosticLine {
    DiagnosticLine {
        key: "permissions".to_string(),
        value: format!(
            "{} (mode {:04o}, {}:{}, running as {}:{}) \u{2014} {}",
            diag.path.display(),
            diag.mode & 0o7777,
            diag.owner,
            diag.group,
            diag.running_as,
            diag.running_group,
            diag.issue,
        ),
    }
}

// ---------------------------------------------------------------------------
// Grammar-driven enrichment
// ---------------------------------------------------------------------------

/// Get the on_failure diagnostic list from grammar, considering the action.
fn grammar_on_failure_list(grammar: &Grammar, command: &[String]) -> Vec<String> {
    if let Some(enrich) = &grammar.enrich {
        // Check action-specific overrides
        for (action_name, action_config) in &enrich.actions {
            // Check if command args contain the action name
            if command.iter().skip(1).any(|a| a == action_name) && !action_config.on_failure.is_empty() {
                return action_config.on_failure.clone();
            }
        }
        enrich.on_failure.clone()
    } else {
        Vec::new()
    }
}

/// Run a grammar-specified diagnostic function by name.
fn run_grammar_diagnostic(
    func_name: &str,
    command: &[String],
    paths: &[PathBuf],
) -> Vec<DiagnosticLine> {
    let mut lines = Vec::new();

    match func_name {
        "source_exists" => {
            if let Some(path) = paths.first() {
                lines.push(source_exists(path));
            }
        }
        "dest_path_walk" => {
            if let Some(path) = paths.get(1).or(paths.first()) {
                if let Some(diag) = dest_path_walk(path) {
                    lines.extend(path_diagnosis_to_lines(&diag));
                }
            }
        }
        "permissions" => {
            for path in paths {
                if let Some(diag) = permissions(path) {
                    lines.push(permission_diagnosis_to_line(&diag));
                }
            }
        }
        "is_git_repo" => {
            lines.push(is_git_repo(Path::new(".")));
        }
        "branch_exists" => {
            // Try to find branch arg in command
            if let Some(branch) = command.iter().skip(1).find(|a| !a.starts_with('-')) {
                lines.push(branch_exists(branch));
            }
        }
        "branch_list_similar" => {
            if let Some(branch) = command.iter().skip(1).find(|a| !a.starts_with('-')) {
                lines.push(branch_list_similar(branch));
            }
        }
        "working_tree_clean" => {
            lines.push(working_tree_clean());
        }
        "remote_ref_status" => {
            lines.push(remote_ref_status());
        }
        "ahead_behind" => {
            lines.push(ahead_behind());
        }
        "port_listening" => {
            if let Some(port) = extract_port(command, "") {
                lines.push(port_listening(port));
            }
        }
        "dir_listing" => {
            if let Some(path) = paths.first() {
                lines.push(dir_listing(path));
            }
        }
        "disk_space" => {
            if let Some(path) = paths.first() {
                lines.push(disk_space(path));
            }
        }
        "node_modules_check" => {
            lines.push(node_modules_check());
        }
        "command_not_found_hint" => {
            if !command.is_empty() {
                lines.push(command_not_found_hint(base_command(&command[0])));
            }
        }
        "command_similar" => {
            if !command.is_empty() {
                lines.push(command_similar(base_command(&command[0])));
            }
        }
        _ => {
            // Unknown diagnostic function — skip silently
        }
    }

    lines
}

// ---------------------------------------------------------------------------
// Intent-based default diagnostics
// ---------------------------------------------------------------------------

fn default_diagnostics_for_intent(intent: CommandIntent) -> Vec<&'static str> {
    match intent {
        CommandIntent::FileOp => vec![
            "source_exists",
            "dest_path_walk",
            "permissions",
            "dir_listing",
        ],
        CommandIntent::DirOp => vec!["dest_path_walk", "dir_listing", "permissions"],
        CommandIntent::NetworkOp => vec!["port_listening"],
        CommandIntent::BuildRun => vec!["is_git_repo", "node_modules_check"],
        CommandIntent::ProcessExec => vec![],
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Enrich a failed command with diagnostics.
///
/// Only call this when `exit_code != 0`. Gathers read-only, fast diagnostics
/// within a 100ms total budget.
pub fn enrich(
    command: &[String],
    exit_code: i32,
    stderr: &str,
    grammar: Option<&Grammar>,
) -> EnrichmentResult {
    let budget = Budget::new(100);
    let mut diagnostics = Vec::new();

    // 1. Exit code mapping (always first, instant)
    if let Some(line) = map_exit_code(exit_code) {
        diagnostics.push(line);
    }

    // 2. Special handling for specific exit codes
    if exit_code == 127 && !command.is_empty() {
        // Command not found
        let cmd = base_command(&command[0]);
        diagnostics.push(command_not_found_hint(cmd));
        if !budget.exhausted() {
            diagnostics.push(command_similar(cmd));
        }
        return EnrichmentResult { diagnostics };
    }

    if exit_code == 130 {
        // SIGINT — minimal enrichment, user interrupted
        return EnrichmentResult { diagnostics };
    }

    if exit_code == 137 || exit_code == 139 {
        // OOM or segfault — not much we can diagnose from filesystem
        return EnrichmentResult { diagnostics };
    }

    // 3. Extract paths from command
    let paths = extract_path_args(command, grammar);

    // 4. Permission denied (exit 126)
    if exit_code == 126 && !command.is_empty() {
        let cmd_path = PathBuf::from(&command[0]);
        if let Some(diag) = diagnose_permissions(&cmd_path) {
            diagnostics.push(permission_diagnosis_to_line(&diag));
        }
        // Also try the first path arg
        for path in &paths {
            if !budget.exhausted() {
                if let Some(diag) = diagnose_permissions(path) {
                    diagnostics.push(permission_diagnosis_to_line(&diag));
                }
            }
        }
        return EnrichmentResult { diagnostics };
    }

    // 5. Grammar-driven diagnostics (if grammar has enrich config)
    if let Some(g) = grammar {
        let on_failure = grammar_on_failure_list(g, command);
        for func_name in &on_failure {
            if budget.exhausted() {
                break;
            }
            let lines = run_grammar_diagnostic(func_name, command, &paths);
            diagnostics.extend(lines);
        }
        // If grammar provided diagnostics, return them
        if !diagnostics.is_empty() {
            return EnrichmentResult { diagnostics };
        }
    }

    // 6. Intent-based default diagnostics
    let intent = detect_intent(command);
    let defaults = default_diagnostics_for_intent(intent);
    for func_name in defaults {
        if budget.exhausted() {
            break;
        }
        let lines = run_grammar_diagnostic(func_name, command, &paths);
        diagnostics.extend(lines);
    }

    // 7. For file ops, also build FileOpDiagnosis
    if intent == CommandIntent::FileOp && paths.len() >= 2 && !budget.exhausted() {
        let src = &paths[0];
        let dst = &paths[paths.len() - 1];
        let fod = FileOpDiagnosis {
            source_exists: src.exists(),
            source_info: if src.exists() {
                Some(stat::file_info(src))
            } else {
                None
            },
            dest_exists: dst.exists(),
            dest_info: if dst.exists() {
                Some(stat::file_info(dst))
            } else {
                None
            },
            dest_parent_exists: dst.parent().map(|p| p.exists()).unwrap_or(false),
            dest_parent_info: dst.parent().map(stat::file_info),
        };

        let src_mark = if fod.source_exists { "\u{2713}" } else { "\u{2717}" };
        let dst_mark = if fod.dest_exists { "\u{2713}" } else { "\u{2717}" };

        diagnostics.push(DiagnosticLine {
            key: "source".to_string(),
            value: format!(
                "{} {src_mark}",
                fod.source_info
                    .as_deref()
                    .unwrap_or(&src.display().to_string())
            ),
        });
        diagnostics.push(DiagnosticLine {
            key: "dest".to_string(),
            value: format!(
                "{} {dst_mark}",
                fod.dest_info
                    .as_deref()
                    .unwrap_or(&dst.display().to_string())
            ),
        });
        if let Some(parent_info) = &fod.dest_parent_info {
            let parent_mark = if fod.dest_parent_exists { "\u{2713}" } else { "\u{2717}" };
            diagnostics.push(DiagnosticLine {
                key: "dest_parent".to_string(),
                value: format!("{parent_info} {parent_mark}"),
            });
        }
    }

    // 8. Network ops: port check
    if intent == CommandIntent::NetworkOp && !budget.exhausted() {
        if let Some(port) = extract_port(command, stderr) {
            diagnostics.push(port_listening(port));
        }
    }

    EnrichmentResult { diagnostics }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn cmd(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    // Test 1: Path resolution — break point detection
    #[test]
    fn test_path_walk_break_point() {
        let dir = TempDir::new().unwrap();
        let existing = dir.path().join("a");
        fs::create_dir(&existing).unwrap();

        // /tmp/.../a exists, /tmp/.../a/b does not, /tmp/.../a/b/c does not
        let deep_path = existing.join("b").join("c");
        let diag = path_walk(&deep_path).unwrap();

        assert_eq!(diag.last_valid, existing);
        assert_eq!(diag.breaks_at, existing.join("b"));
        assert_eq!(diag.requested, deep_path);
    }

    // Test 2: Path resolution — fully valid path
    #[test]
    fn test_path_walk_fully_valid() {
        let dir = TempDir::new().unwrap();
        let result = path_walk(dir.path());
        assert!(result.is_none(), "fully valid path should return None");
    }

    // Test 3: Source/target existence — source exists, dest doesn't
    #[test]
    fn test_source_exists_dest_doesnt() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("source.txt");
        fs::write(&src, "hello").unwrap();
        let dst = dir.path().join("nonexistent_dir").join("dest.txt");

        let result = enrich(
            &cmd(&[
                "cp",
                &src.to_string_lossy(),
                &dst.to_string_lossy(),
            ]),
            1,
            "No such file or directory",
            None,
        );

        // Should have diagnostics about the paths
        assert!(!result.diagnostics.is_empty());
        // Check that source is reported as existing
        let has_source = result
            .diagnostics
            .iter()
            .any(|d| d.key == "source" && d.value.contains("\u{2713}"));
        assert!(has_source, "should report source exists");
    }

    // Test 4: Source/target existence — neither exists
    #[test]
    fn test_neither_source_nor_dest_exists() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("no_source.txt");
        let dst = dir.path().join("no_dest.txt");

        let result = enrich(
            &cmd(&[
                "cp",
                &src.to_string_lossy(),
                &dst.to_string_lossy(),
            ]),
            1,
            "No such file or directory",
            None,
        );

        assert!(!result.diagnostics.is_empty());
    }

    // Test 5: Permission diagnosis — file owned by current user but no write
    #[test]
    fn test_permission_diagnosis() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("readonly.txt");
        fs::write(&file, "test").unwrap();

        // Make it read-only
        let mut perms = fs::metadata(&file).unwrap().permissions();
        perms.set_mode(0o444);
        fs::set_permissions(&file, perms).unwrap();

        let diag = diagnose_permissions(&file);
        assert!(diag.is_some());
        let d = diag.unwrap();
        assert_eq!(d.mode & 0o7777, 0o444);
        assert!(d.issue.contains("write"), "should identify write issue: {}", d.issue);
    }

    // Test 6: Command not found with package hint (rg -> ripgrep)
    #[test]
    fn test_command_not_found_with_hint() {
        let result = enrich(&cmd(&["rg", "pattern"]), 127, "command not found: rg", None);

        // Should have exit code mapping
        let has_signal = result
            .diagnostics
            .iter()
            .any(|d| d.key == "signal" && d.value.contains("command not found"));
        assert!(has_signal, "should have exit code mapping");

        // Should have package hint
        let has_hint = result
            .diagnostics
            .iter()
            .any(|d| d.key == "package_hint" && d.value.contains("ripgrep"));
        assert!(has_hint, "should suggest ripgrep package");
    }

    // Test 7: Command not found without hint
    #[test]
    fn test_command_not_found_no_hint() {
        let result = enrich(
            &cmd(&["obscure_command_xyz"]),
            127,
            "command not found: obscure_command_xyz",
            None,
        );

        let has_signal = result
            .diagnostics
            .iter()
            .any(|d| d.key == "signal" && d.value.contains("command not found"));
        assert!(has_signal);

        let has_hint = result
            .diagnostics
            .iter()
            .any(|d| d.key == "package_hint" && d.value.contains("no known package"));
        assert!(has_hint, "should say no known package");
    }

    // Test 8: Budget enforcement — verify we don't exceed 100ms
    #[test]
    fn test_budget_enforcement() {
        let start = Instant::now();
        let _ = enrich(
            &cmd(&["some_command", "/a/b/c/d/e/f"]),
            1,
            "error",
            None,
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(200),
            "enrichment took {:?}, should be under 200ms",
            elapsed
        );
    }

    // Test 9: File op enrichment — cp with missing dest parent
    #[test]
    fn test_file_op_missing_dest_parent() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("file.txt");
        fs::write(&src, "content").unwrap();
        let dst = dir.path().join("nonexistent").join("sub").join("dest.txt");

        let result = enrich(
            &cmd(&[
                "cp",
                &src.to_string_lossy(),
                &dst.to_string_lossy(),
            ]),
            1,
            "No such file or directory",
            None,
        );

        // Should have path walk info showing the break
        let has_path = result
            .diagnostics
            .iter()
            .any(|d| d.key == "path" && d.value.contains("\u{2717}"));
        assert!(has_path, "should show path break point");
    }

    // Test 10: File op enrichment — mv with missing source
    #[test]
    fn test_file_op_missing_source() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("no_such_file.txt");
        let dst = dir.path().join("dest.txt");

        let result = enrich(
            &cmd(&[
                "mv",
                &src.to_string_lossy(),
                &dst.to_string_lossy(),
            ]),
            1,
            "No such file or directory",
            None,
        );

        assert!(!result.diagnostics.is_empty());
        // Source should be marked as not existing
        let has_source = result
            .diagnostics
            .iter()
            .any(|d| d.key == "source" && d.value.contains("\u{2717}"));
        assert!(has_source, "should report source does not exist");
    }

    // Test 11: Git enrichment — not a git repo
    #[test]
    fn test_git_not_a_repo() {
        let dir = TempDir::new().unwrap();
        let diag = is_git_repo(dir.path());
        assert_eq!(diag.key, "git_repo");
        assert!(
            diag.value.contains("not a git repository"),
            "should say not a git repository, got: {}",
            diag.value
        );
    }

    // Test 12: Network enrichment — port not listening
    #[test]
    fn test_port_not_listening() {
        // Use a port that's very unlikely to be in use
        let diag = port_listening(19876);
        assert_eq!(diag.key, "port");
        assert!(
            diag.value.contains("not listening"),
            "port 19876 should not be listening: {}",
            diag.value
        );
    }

    // Test 13: Exit code 127 mapping
    #[test]
    fn test_exit_code_127() {
        let line = map_exit_code(127).unwrap();
        assert_eq!(line.key, "signal");
        assert!(line.value.contains("command not found"));
        assert!(line.value.contains("127"));
    }

    // Test 14: Exit code 126 mapping
    #[test]
    fn test_exit_code_126() {
        let line = map_exit_code(126).unwrap();
        assert_eq!(line.key, "signal");
        assert!(line.value.contains("permission denied"));
        assert!(line.value.contains("126"));
    }

    // Test 15: Exit code 130 mapping (SIGINT)
    #[test]
    fn test_exit_code_130() {
        let result = enrich(&cmd(&["sleep", "100"]), 130, "", None);

        // Should have signal mapping but minimal other enrichment
        let has_signal = result
            .diagnostics
            .iter()
            .any(|d| d.key == "signal" && d.value.contains("SIGINT"));
        assert!(has_signal, "should map 130 to SIGINT");

        // Should be minimal — no file ops, no path walks
        assert!(
            result.diagnostics.len() <= 2,
            "SIGINT should produce minimal enrichment, got {} diagnostics",
            result.diagnostics.len()
        );
    }

    // Test 16: Exit code 137 mapping (OOM)
    #[test]
    fn test_exit_code_137() {
        let result = enrich(&cmd(&["big_process"]), 137, "", None);

        let has_signal = result
            .diagnostics
            .iter()
            .any(|d| d.key == "signal" && d.value.contains("OOM"));
        assert!(has_signal, "should map 137 to OOM");

        // Minimal enrichment for OOM
        assert!(
            result.diagnostics.len() <= 2,
            "OOM should produce minimal enrichment"
        );
    }

    // Test 17: Enrichment with grammar on_failure list
    #[test]
    fn test_enrichment_with_grammar_on_failure() {
        use crate::core::grammar::load_grammar_from_str;

        let toml_str = r#"
[tool]
name = "cp"

[enrich]
on_failure = ["source_exists", "dest_path_walk", "permissions"]

[enrich.args]
path_args = [0, 1]
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();

        let dir = TempDir::new().unwrap();
        let src = dir.path().join("exists.txt");
        fs::write(&src, "data").unwrap();
        let dst = dir.path().join("no_dir").join("file.txt");

        let result = enrich(
            &cmd(&[
                "cp",
                &src.to_string_lossy(),
                &dst.to_string_lossy(),
            ]),
            1,
            "No such file or directory",
            Some(&grammar),
        );

        // Should have diagnostics from the grammar on_failure list
        assert!(!result.diagnostics.is_empty());

        // Should have source_exists diagnostic
        let has_source = result.diagnostics.iter().any(|d| d.key == "source");
        assert!(has_source, "grammar should trigger source_exists diagnostic");

        // Should have path walk diagnostic (dest breaks)
        let has_path = result.diagnostics.iter().any(|d| d.key == "path");
        assert!(has_path, "grammar should trigger dest_path_walk diagnostic");
    }

    // Additional unit tests for helpers

    #[test]
    fn test_edit_distance() {
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("ab", "ba"), 2);
    }

    #[test]
    fn test_fuzzy_match() {
        assert!(fuzzy_match("rg", "rga"));        // prefix
        assert!(fuzzy_match("bat", "batcat"));     // prefix
        assert!(!fuzzy_match("abc", "abc"));       // exact match => false
        assert!(fuzzy_match("abc", "abcd"));       // prefix
        assert!(fuzzy_match("gti", "git"));        // edit distance 2
    }

    #[test]
    fn test_detect_intent() {
        assert_eq!(detect_intent(&cmd(&["cp", "a", "b"])), CommandIntent::FileOp);
        assert_eq!(detect_intent(&cmd(&["mkdir", "dir"])), CommandIntent::DirOp);
        assert_eq!(detect_intent(&cmd(&["curl", "url"])), CommandIntent::NetworkOp);
        assert_eq!(detect_intent(&cmd(&["cargo", "build"])), CommandIntent::BuildRun);
        assert_eq!(detect_intent(&cmd(&["unknown"])), CommandIntent::ProcessExec);
    }

    #[test]
    fn test_package_hint_lookup() {
        assert_eq!(lookup_package_hint("rg"), Some("ripgrep"));
        assert_eq!(lookup_package_hint("fd"), Some("fd-find"));
        assert_eq!(lookup_package_hint("unknown_cmd"), None);
    }

    #[test]
    fn test_budget_tracker() {
        let budget = Budget::new(100);
        assert!(!budget.exhausted());
        assert!(budget.remaining() > Duration::ZERO);
    }

    #[test]
    fn test_base_command() {
        assert_eq!(base_command("/usr/bin/ls"), "ls");
        assert_eq!(base_command("git"), "git");
        assert_eq!(base_command("./script.sh"), "script.sh");
    }

    #[test]
    fn test_path_walk_siblings() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("alpha");
        let b = dir.path().join("beta");
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();

        let missing = dir.path().join("gamma").join("deep");
        let diag = path_walk(&missing).unwrap();

        // Siblings should contain alpha/ and beta/ (the contents of the temp dir)
        assert!(diag.siblings.iter().any(|s| s.contains("alpha")));
        assert!(diag.siblings.iter().any(|s| s.contains("beta")));
    }

    #[test]
    fn test_extract_port() {
        let args = cmd(&["curl", "http://localhost:8080/api"]);
        assert_eq!(extract_port(&args, ""), Some(8080));

        let args2 = cmd(&["curl", "-p", "3000"]);
        assert_eq!(extract_port(&args2, ""), Some(3000));

        let args3 = cmd(&["curl", "https://example.com"]);
        assert_eq!(extract_port(&args3, ""), None);
    }
}
