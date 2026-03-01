/// Narrate handler — file operation narration.
///
/// Inspect args -> stat files -> execute -> narrate result.
///
/// Narrate-category commands (cp, mv, rm, mkdir, chmod, chown, ln, touch, rmdir)
/// produce little or no stdout. The narrate handler adds context by statting files
/// before and after the operation, then emitting a one-line narration.
use std::path::Path;
use std::process::Command;

use crate::core::stat::{
    gather_post_flight, gather_pre_flight, human_size, NarratedResult, PostFlightInfo,
    PreFlightInfo,
};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Narrate handler entry point.
///
/// Parses the command name from `args[0]`, gathers pre-flight stats where
/// applicable, executes the command, gathers post-flight stats, and returns
/// a `NarratedResult` with a human-readable narration message.
pub fn handle(args: &[String]) -> Result<NarratedResult, Box<dyn std::error::Error>> {
    if args.is_empty() {
        return Err("narrate: no command provided".into());
    }

    let cmd = &args[0];
    let cmd_args = &args[1..];

    // Gather pre-flight stats for commands that have source/dest
    let (source, dest) = extract_source_dest(cmd, cmd_args);
    let pre = match (&source, &dest) {
        (Some(s), Some(d)) => Some(gather_pre_flight(Path::new(s), Path::new(d))),
        _ => None,
    };

    // Execute the command
    let output = Command::new(cmd).args(cmd_args).output()?;
    let exit_code = output.status.code().unwrap_or(-1);

    // Gather post-flight stats
    let post = match (&dest, &pre) {
        (Some(d), Some(p)) => Some(gather_post_flight(Path::new(d), p)),
        _ => None,
    };

    // Narrate
    let message = match cmd.as_str() {
        "cp" => narrate_cp(
            cmd_args,
            exit_code,
            pre.as_ref().unwrap_or(&empty_pre()),
            post.as_ref().unwrap_or(&empty_post()),
        ),
        "mv" => narrate_mv(
            cmd_args,
            exit_code,
            pre.as_ref().unwrap_or(&empty_pre()),
            post.as_ref().unwrap_or(&empty_post()),
        ),
        "rm" => narrate_rm(cmd_args, exit_code),
        "mkdir" => narrate_mkdir(cmd_args, exit_code),
        "chmod" => narrate_chmod(cmd_args, exit_code),
        "ln" => narrate_ln(cmd_args, exit_code),
        "touch" | "chown" | "rmdir" => narrate_generic(cmd, cmd_args, exit_code),
        _ => narrate_generic(cmd, cmd_args, exit_code),
    };

    Ok(NarratedResult {
        success: exit_code == 0,
        message,
        exit_code,
    })
}

// ---------------------------------------------------------------------------
// Per-command narrators
// ---------------------------------------------------------------------------

/// Narrate a `cp` command.
fn narrate_cp(
    args: &[String],
    exit_code: i32,
    pre: &PreFlightInfo,
    post: &PostFlightInfo,
) -> String {
    let (source, dest) = paths_from_args(args);
    if exit_code != 0 {
        return format!("! cp: {source} \u{2192} {dest} -- failed (exit {exit_code})");
    }

    let size = post
        .dest_size
        .map(human_size)
        .unwrap_or_else(|| "unknown size".to_string());

    let note = if pre.dest_exists {
        "overwritten"
    } else if post.size_match {
        "ok"
    } else {
        "ok"
    };

    format!("\u{2192} cp: {source} \u{2192} {dest} ({size}, {note})")
}

/// Narrate an `mv` command.
fn narrate_mv(
    args: &[String],
    exit_code: i32,
    pre: &PreFlightInfo,
    post: &PostFlightInfo,
) -> String {
    let (source, dest) = paths_from_args(args);
    if exit_code != 0 {
        return format!("! mv: {source} \u{2192} {dest} -- failed (exit {exit_code})");
    }

    let size = post
        .dest_size
        .map(human_size)
        .unwrap_or_else(|| "unknown size".to_string());

    let note = if pre.dest_exists {
        "overwritten"
    } else {
        "moved"
    };

    format!("\u{2192} mv: {source} \u{2192} {dest} ({size}, {note})")
}

/// Narrate an `rm` command.
fn narrate_rm(args: &[String], exit_code: i32) -> String {
    let targets = non_flag_args(args);
    let target = targets.first().map(|s| s.as_str()).unwrap_or("?");

    if exit_code != 0 {
        return format!("! rm: {target} -- failed (exit {exit_code})");
    }

    let recursive = args.iter().any(|a| a.contains('r') && a.starts_with('-'));
    let is_dir = args.iter().any(|a| a == "-d" || a == "--dir");

    if recursive {
        format!("\u{2192} rm: {target} (recursive, removed)")
    } else if is_dir {
        format!("\u{2192} rm: {target} (directory, removed)")
    } else {
        format!("\u{2192} rm: {target} (removed)")
    }
}

/// Narrate a `mkdir` command.
fn narrate_mkdir(args: &[String], exit_code: i32) -> String {
    let targets = non_flag_args(args);
    let target = targets.first().map(|s| s.as_str()).unwrap_or("?");

    if exit_code != 0 {
        return format!("! mkdir: {target} -- failed (exit {exit_code})");
    }

    let nested = args.iter().any(|a| a == "-p" || a == "--parents");

    if nested {
        format!("\u{2192} mkdir: {target} (nested, created)")
    } else {
        format!("\u{2192} mkdir: {target} (created)")
    }
}

/// Narrate a `chmod` command.
fn narrate_chmod(args: &[String], exit_code: i32) -> String {
    let non_flags = non_flag_args(args);
    let (mode, target) = if non_flags.len() >= 2 {
        (non_flags[0].as_str(), non_flags[1].as_str())
    } else {
        ("?", non_flags.first().map(|s| s.as_str()).unwrap_or("?"))
    };

    if exit_code != 0 {
        return format!("! chmod: {target} -- failed (exit {exit_code})");
    }

    format!("\u{2192} chmod: {target} \u{2192} {mode}")
}

/// Narrate an `ln` command.
fn narrate_ln(args: &[String], exit_code: i32) -> String {
    let (source, dest) = paths_from_args(args);

    if exit_code != 0 {
        return format!("! ln: {source} \u{2192} {dest} -- failed (exit {exit_code})");
    }

    let symlink = args.iter().any(|a| a == "-s" || a == "--symbolic");

    if symlink {
        format!("\u{2192} ln: {dest} \u{2192} {source} (symlink)")
    } else {
        format!("\u{2192} ln: {dest} \u{2192} {source} (hard link)")
    }
}

/// Generic fallback narrator for commands without special handling.
fn narrate_generic(command: &str, args: &[String], exit_code: i32) -> String {
    let targets = non_flag_args(args);
    let target = targets.first().map(|s| s.as_str()).unwrap_or("");

    if exit_code != 0 {
        if target.is_empty() {
            format!("! {command}: failed (exit {exit_code})")
        } else {
            format!("! {command}: {target} -- failed (exit {exit_code})")
        }
    } else if target.is_empty() {
        format!("\u{2192} {command}: ok")
    } else {
        format!("\u{2192} {command}: {target} (ok)")
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract source and destination paths from command arguments.
///
/// For commands like `cp src dst` or `mv src dst`, the last non-flag arg
/// is the destination and the second-to-last is the source.
fn extract_source_dest(cmd: &str, args: &[String]) -> (Option<String>, Option<String>) {
    match cmd {
        "cp" | "mv" | "ln" => {
            let non_flags = non_flag_args(args);
            if non_flags.len() >= 2 {
                let source = non_flags[non_flags.len() - 2].clone();
                let dest = non_flags[non_flags.len() - 1].clone();
                (Some(source), Some(dest))
            } else {
                (None, None)
            }
        }
        _ => (None, None),
    }
}

/// Extract the last two non-flag arguments as (source, dest) for display.
fn paths_from_args(args: &[String]) -> (String, String) {
    let non_flags = non_flag_args(args);
    if non_flags.len() >= 2 {
        (
            non_flags[non_flags.len() - 2].clone(),
            non_flags[non_flags.len() - 1].clone(),
        )
    } else if non_flags.len() == 1 {
        (non_flags[0].clone(), "?".to_string())
    } else {
        ("?".to_string(), "?".to_string())
    }
}

/// Filter args to only non-flag arguments (those not starting with `-`).
fn non_flag_args(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|a| !a.starts_with('-'))
        .cloned()
        .collect()
}

/// Empty pre-flight info for fallback.
fn empty_pre() -> PreFlightInfo {
    PreFlightInfo {
        source_size: None,
        source_mtime: None,
        source_permissions: None,
        dest_exists: false,
        dest_size: None,
        dest_mtime: None,
    }
}

/// Empty post-flight info for fallback.
fn empty_post() -> PostFlightInfo {
    PostFlightInfo {
        dest_size: None,
        dest_mtime: None,
        size_match: false,
        file_count: None,
        total_bytes: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Unit tests for narration logic (mock data, no process spawning)
    // -----------------------------------------------------------------------

    // Test 1: narrate cp success
    #[test]
    fn test_narrate_cp_success() {
        let pre = PreFlightInfo {
            source_size: Some(1024),
            source_mtime: None,
            source_permissions: Some(0o644),
            dest_exists: false,
            dest_size: None,
            dest_mtime: None,
        };
        let post = PostFlightInfo {
            dest_size: Some(1024),
            dest_mtime: None,
            size_match: true,
            file_count: None,
            total_bytes: None,
        };
        let args = vec!["src.txt".to_string(), "dst.txt".to_string()];
        let result = narrate_cp(&args, 0, &pre, &post);
        assert!(result.starts_with("\u{2192} cp:"));
        assert!(result.contains("src.txt"));
        assert!(result.contains("dst.txt"));
        assert!(result.contains("1.0 KB"));
        assert!(result.contains("ok"));
    }

    // Test 2: narrate cp failure
    #[test]
    fn test_narrate_cp_failure() {
        let pre = empty_pre();
        let post = empty_post();
        let args = vec!["src.txt".to_string(), "dst.txt".to_string()];
        let result = narrate_cp(&args, 1, &pre, &post);
        assert!(result.starts_with("! cp:"));
        assert!(result.contains("failed"));
        assert!(result.contains("exit 1"));
    }

    // Test 3: narrate cp overwrite
    #[test]
    fn test_narrate_cp_overwrite() {
        let pre = PreFlightInfo {
            source_size: Some(2048),
            source_mtime: None,
            source_permissions: Some(0o644),
            dest_exists: true, // dest already exists
            dest_size: Some(512),
            dest_mtime: None,
        };
        let post = PostFlightInfo {
            dest_size: Some(2048),
            dest_mtime: None,
            size_match: true,
            file_count: None,
            total_bytes: None,
        };
        let args = vec!["src.txt".to_string(), "dst.txt".to_string()];
        let result = narrate_cp(&args, 0, &pre, &post);
        assert!(result.contains("overwritten"));
    }

    // Test 4: narrate rm file
    #[test]
    fn test_narrate_rm_file() {
        let args = vec!["file.txt".to_string()];
        let result = narrate_rm(&args, 0);
        assert!(result.starts_with("\u{2192} rm:"));
        assert!(result.contains("file.txt"));
        assert!(result.contains("removed"));
        assert!(!result.contains("recursive"));
    }

    // Test 5: narrate rm dir
    #[test]
    fn test_narrate_rm_dir() {
        let args = vec!["-d".to_string(), "mydir".to_string()];
        let result = narrate_rm(&args, 0);
        assert!(result.contains("directory"));
        assert!(result.contains("removed"));
    }

    // Test 6: narrate rm recursive
    #[test]
    fn test_narrate_rm_recursive() {
        let args = vec!["-rf".to_string(), "mydir".to_string()];
        let result = narrate_rm(&args, 0);
        assert!(result.contains("recursive"));
        assert!(result.contains("removed"));
    }

    // Test 7: narrate mkdir single
    #[test]
    fn test_narrate_mkdir_single() {
        let args = vec!["newdir".to_string()];
        let result = narrate_mkdir(&args, 0);
        assert!(result.starts_with("\u{2192} mkdir:"));
        assert!(result.contains("newdir"));
        assert!(result.contains("created"));
        assert!(!result.contains("nested"));
    }

    // Test 8: narrate mkdir nested
    #[test]
    fn test_narrate_mkdir_nested() {
        let args = vec!["-p".to_string(), "a/b/c".to_string()];
        let result = narrate_mkdir(&args, 0);
        assert!(result.contains("nested"));
        assert!(result.contains("created"));
    }

    // Test 9: narrate chmod
    #[test]
    fn test_narrate_chmod() {
        let args = vec!["755".to_string(), "script.sh".to_string()];
        let result = narrate_chmod(&args, 0);
        assert!(result.starts_with("\u{2192} chmod:"));
        assert!(result.contains("script.sh"));
        assert!(result.contains("755"));
    }

    // Test 10: narrate mv
    #[test]
    fn test_narrate_mv() {
        let pre = PreFlightInfo {
            source_size: Some(4096),
            source_mtime: None,
            source_permissions: Some(0o644),
            dest_exists: false,
            dest_size: None,
            dest_mtime: None,
        };
        let post = PostFlightInfo {
            dest_size: Some(4096),
            dest_mtime: None,
            size_match: true,
            file_count: None,
            total_bytes: None,
        };
        let args = vec!["old.txt".to_string(), "new.txt".to_string()];
        let result = narrate_mv(&args, 0, &pre, &post);
        assert!(result.starts_with("\u{2192} mv:"));
        assert!(result.contains("old.txt"));
        assert!(result.contains("new.txt"));
        assert!(result.contains("4.0 KB"));
        assert!(result.contains("moved"));
    }

    // Test 11: narrate ln symlink
    #[test]
    fn test_narrate_ln_symlink() {
        let args = vec![
            "-s".to_string(),
            "/usr/bin/python3".to_string(),
            "/usr/local/bin/python".to_string(),
        ];
        let result = narrate_ln(&args, 0);
        assert!(result.starts_with("\u{2192} ln:"));
        assert!(result.contains("symlink"));
        assert!(result.contains("/usr/bin/python3"));
        assert!(result.contains("/usr/local/bin/python"));
    }

    // Test 12: generic fallback
    #[test]
    fn test_narrate_generic_fallback() {
        let args = vec!["target.txt".to_string()];
        let result = narrate_generic("touch", &args, 0);
        assert!(result.starts_with("\u{2192} touch:"));
        assert!(result.contains("target.txt"));
        assert!(result.contains("ok"));
    }

    // Test 12b: generic fallback failure
    #[test]
    fn test_narrate_generic_failure() {
        let args = vec!["target.txt".to_string()];
        let result = narrate_generic("chown", &args, 1);
        assert!(result.starts_with("! chown:"));
        assert!(result.contains("failed"));
        assert!(result.contains("exit 1"));
    }

    // -----------------------------------------------------------------------
    // Integration tests (actually run commands via handle())
    // -----------------------------------------------------------------------

    // Integration: handle() with real cp
    #[test]
    fn test_handle_cp_integration() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        fs::write(&src, "hello world").unwrap();

        let args: Vec<String> = vec![
            "cp".to_string(),
            src.to_string_lossy().to_string(),
            dst.to_string_lossy().to_string(),
        ];
        let result = handle(&args).unwrap();
        assert!(result.success);
        assert_eq!(result.exit_code, 0);
        assert!(result.message.starts_with("\u{2192} cp:"));
        assert!(dst.exists());
    }

    // Integration: handle() with real mv
    #[test]
    fn test_handle_mv_integration() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("before.txt");
        let dst = dir.path().join("after.txt");
        fs::write(&src, "content").unwrap();

        let args: Vec<String> = vec![
            "mv".to_string(),
            src.to_string_lossy().to_string(),
            dst.to_string_lossy().to_string(),
        ];
        let result = handle(&args).unwrap();
        assert!(result.success);
        assert!(!src.exists());
        assert!(dst.exists());
    }

    // Integration: handle() with real mkdir -p
    #[test]
    fn test_handle_mkdir_nested_integration() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a/b/c");

        let args: Vec<String> = vec![
            "mkdir".to_string(),
            "-p".to_string(),
            nested.to_string_lossy().to_string(),
        ];
        let result = handle(&args).unwrap();
        assert!(result.success);
        assert!(result.message.contains("nested"));
        assert!(nested.exists());
    }

    // Integration: handle() with real ln -s
    #[test]
    fn test_handle_ln_symlink_integration() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target.txt");
        let link = dir.path().join("link.txt");
        fs::write(&target, "target content").unwrap();

        let args: Vec<String> = vec![
            "ln".to_string(),
            "-s".to_string(),
            target.to_string_lossy().to_string(),
            link.to_string_lossy().to_string(),
        ];
        let result = handle(&args).unwrap();
        assert!(result.success);
        assert!(result.message.contains("symlink"));
        assert!(link.exists());
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    }

    // Integration: handle() with empty args returns error
    #[test]
    fn test_handle_empty_args() {
        let result = handle(&[]);
        assert!(result.is_err());
    }

    // Test: non_flag_args helper
    #[test]
    fn test_non_flag_args() {
        let args: Vec<String> = vec!["-r", "-f", "dir1", "--verbose", "dir2"]
            .into_iter()
            .map(String::from)
            .collect();
        let result = non_flag_args(&args);
        assert_eq!(result, vec!["dir1", "dir2"]);
    }

    // Test: extract_source_dest for cp
    #[test]
    fn test_extract_source_dest_cp() {
        let args: Vec<String> = vec!["-r", "src", "dst"]
            .into_iter()
            .map(String::from)
            .collect();
        let (source, dest) = extract_source_dest("cp", &args);
        assert_eq!(source, Some("src".to_string()));
        assert_eq!(dest, Some("dst".to_string()));
    }

    // Test: extract_source_dest for non-cp/mv/ln returns None
    #[test]
    fn test_extract_source_dest_rm() {
        let args: Vec<String> = vec!["file.txt"].into_iter().map(String::from).collect();
        let (source, dest) = extract_source_dest("rm", &args);
        assert!(source.is_none());
        assert!(dest.is_none());
    }

    // Test: mv overwrite narration
    #[test]
    fn test_narrate_mv_overwrite() {
        let pre = PreFlightInfo {
            source_size: Some(100),
            source_mtime: None,
            source_permissions: Some(0o644),
            dest_exists: true,
            dest_size: Some(50),
            dest_mtime: None,
        };
        let post = PostFlightInfo {
            dest_size: Some(100),
            dest_mtime: None,
            size_match: true,
            file_count: None,
            total_bytes: None,
        };
        let args = vec!["old.txt".to_string(), "existing.txt".to_string()];
        let result = narrate_mv(&args, 0, &pre, &post);
        assert!(result.contains("overwritten"));
    }

    // Test: ln hard link (no -s flag)
    #[test]
    fn test_narrate_ln_hard_link() {
        let args = vec!["source".to_string(), "link".to_string()];
        let result = narrate_ln(&args, 0);
        assert!(result.contains("hard link"));
        assert!(!result.contains("symlink"));
    }

    // Test: mkdir failure
    #[test]
    fn test_narrate_mkdir_failure() {
        let args = vec!["newdir".to_string()];
        let result = narrate_mkdir(&args, 1);
        assert!(result.starts_with("! mkdir:"));
        assert!(result.contains("failed"));
    }

    // Test: chmod failure
    #[test]
    fn test_narrate_chmod_failure() {
        let args = vec!["755".to_string(), "nofile".to_string()];
        let result = narrate_chmod(&args, 1);
        assert!(result.starts_with("! chmod:"));
        assert!(result.contains("failed"));
    }

    // Test: rm failure
    #[test]
    fn test_narrate_rm_failure() {
        let args = vec!["nofile.txt".to_string()];
        let result = narrate_rm(&args, 1);
        assert!(result.starts_with("! rm:"));
        assert!(result.contains("failed"));
    }

    // Test: ln failure
    #[test]
    fn test_narrate_ln_failure() {
        let args = vec![
            "-s".to_string(),
            "source".to_string(),
            "link".to_string(),
        ];
        let result = narrate_ln(&args, 1);
        assert!(result.starts_with("! ln:"));
        assert!(result.contains("failed"));
    }

    // Test: generic with no target
    #[test]
    fn test_narrate_generic_no_target() {
        let args: Vec<String> = vec!["--help".to_string()];
        let result = narrate_generic("sync", &args, 0);
        assert_eq!(result, "\u{2192} sync: ok");
    }
}
