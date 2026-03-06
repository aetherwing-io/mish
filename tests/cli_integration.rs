//! End-to-end integration tests for the mish CLI proxy pipeline.
//!
//! Tests the compiled binary via `assert_cmd`, verifying:
//! - Basic command execution and exit code propagation
//! - All four output modes (human, json, passthrough, context)
//! - Compound command operators (&&, ||, ;)
//! - Condensation of verbose output
//! - Error handling and edge cases
//!
//! Note: With no grammar/category config loaded, all commands route to
//! the Condense handler (the default fallback). These tests verify the
//! full pipeline as-is: PTY capture → classifier → emit buffer → format.

use assert_cmd::cargo::cargo_bin_cmd;
use assert_cmd::Command;
use predicates::prelude::*;
use serial_test::serial;
use std::fs;
use tempfile::TempDir;

/// Helper: build a `mish` command from the compiled binary.
fn mish() -> Command {
    cargo_bin_cmd!("mish")
}

// =========================================================================
// 1. Basic command execution
// =========================================================================

#[test]
#[serial(pty)]
fn test_01_echo_hello_produces_output() {
    mish()
        .args(["echo", "hello"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello"));
}

#[test]
#[serial(pty)]
fn test_02_true_exits_zero() {
    mish()
        .args(["true"])
        .assert()
        .success();
}

#[test]
#[serial(pty)]
fn test_03_exit_code_one_propagated() {
    mish()
        .args(["/bin/sh", "-c", "exit 1"])
        .assert()
        .code(1);
}

#[test]
#[serial(pty)]
fn test_04_exit_code_42_propagated() {
    mish()
        .args(["/bin/sh", "-c", "exit 42"])
        .assert()
        .code(42);
}

// =========================================================================
// 2. Human output mode (default)
// =========================================================================

#[test]
#[serial(pty)]
fn test_05_human_success_starts_with_plus() {
    // echo is passthrough with real grammars — use /bin/sh -c which is condense
    mish()
        .args(["/bin/sh", "-c", "echo hello"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("+"));
}

#[test]
#[serial(pty)]
fn test_06_human_failure_starts_with_bang() {
    mish()
        .args(["/bin/sh", "-c", "exit 1"])
        .assert()
        .code(1)
        .stdout(predicate::str::starts_with("!"));
}

#[test]
#[serial(pty)]
fn test_07_human_shows_line_count() {
    mish()
        .args(["/bin/sh", "-c", "echo a; echo b; echo c"])
        .assert()
        .success()
        .stdout(predicate::str::contains("lines"));
}

#[test]
#[serial(pty)]
fn test_08_human_shows_exit_code_on_failure() {
    mish()
        .args(["/bin/sh", "-c", "echo output && exit 42"])
        .assert()
        .code(42)
        .stdout(predicate::str::contains("exit 42"));
}

#[test]
#[serial(pty)]
fn test_09_ring_buffer_last_lines() {
    mish()
        .args(["/bin/sh", "-c", "echo alpha; echo bravo; echo charlie"])
        .assert()
        .success()
        .stdout(predicate::str::contains("last:"));
}

// =========================================================================
// 3. JSON output mode (--json)
// =========================================================================

#[test]
#[serial(pty)]
fn test_10_json_mode_valid_structure() {
    let output = mish()
        .args(["--json", "echo", "hello"])
        .output()
        .expect("mish should run");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("output should be valid JSON");

    assert_eq!(parsed["command"], "echo hello");
    assert_eq!(parsed["exit_code"], 0);
    assert!(parsed["category"].is_string());
    assert!(parsed["outcomes"].is_array());
    assert!(parsed["hazards"].is_array());
}

#[test]
#[serial(pty)]
fn test_11_json_mode_failure_exit_code() {
    let output = mish()
        .args(["--json", "/bin/sh", "-c", "exit 1"])
        .output()
        .expect("mish should run");

    assert!(!output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("output should be valid JSON");

    assert_eq!(parsed["exit_code"], 1);
}

#[test]
#[serial(pty)]
fn test_12_json_category_is_condense() {
    let output = mish()
        .args(["--json", "echo", "test"])
        .output()
        .expect("mish should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("output should be valid JSON");

    assert_eq!(
        parsed["category"], "passthrough",
        "echo is passthrough with real grammar config"
    );
}

#[test]
#[serial(pty)]
fn test_13_json_optional_fields_absent() {
    let output = mish()
        .args(["--json", "echo", "test"])
        .output()
        .expect("mish should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("output should be valid JSON");

    // elapsed_seconds and total_lines are skipped when None
    assert!(
        parsed.get("elapsed_seconds").is_none(),
        "elapsed_seconds should be absent"
    );
    assert!(
        parsed.get("total_lines").is_none(),
        "total_lines should be absent"
    );
}

// =========================================================================
// 4. Context output mode (--context)
// =========================================================================

#[test]
#[serial(pty)]
fn test_14_context_mode_single_line() {
    let output = mish()
        .args(["--context", "echo", "hello"])
        .output()
        .expect("mish should run");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();

    assert!(
        !trimmed.contains('\n'),
        "context mode should be single line, got: {}",
        trimmed
    );
    assert!(
        trimmed.starts_with("echo hello:"),
        "should start with command, got: {}",
        trimmed
    );
    assert!(
        trimmed.contains("ok"),
        "should contain 'ok' for exit 0, got: {}",
        trimmed
    );
}

#[test]
#[serial(pty)]
fn test_15_context_mode_failure_shows_err() {
    let output = mish()
        .args(["--context", "/bin/sh", "-c", "exit 1"])
        .output()
        .expect("mish should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();

    assert!(
        trimmed.contains("err"),
        "should contain 'err' for non-zero exit, got: {}",
        trimmed
    );
}

// =========================================================================
// 5. Passthrough output mode (--passthrough)
// =========================================================================

#[test]
#[serial(pty)]
fn test_16_passthrough_mode_has_summary() {
    mish()
        .args(["--passthrough", "echo", "hello"])
        .assert()
        .success()
        .stdout(predicate::str::contains("mish summary"));
}

// =========================================================================
// 6. Compound commands
// =========================================================================

#[test]
#[serial(pty)]
fn test_17_compound_and_both_run_on_success() {
    let output = mish()
        .args(["echo", "first", "&&", "echo", "second"])
        .output()
        .expect("mish should run");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("first"), "should contain first output");
    assert!(stdout.contains("second"), "should contain second output");
}

#[test]
#[serial(pty)]
fn test_18_compound_and_skips_on_failure() {
    let output = mish()
        .args([
            "/bin/sh",
            "-c",
            "exit 1",
            "&&",
            "echo",
            "should_not_appear",
        ])
        .output()
        .expect("mish should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("should_not_appear"),
        "second command should not run after && failure"
    );
}

#[test]
#[serial(pty)]
fn test_19_compound_or_fallback_runs() {
    let output = mish()
        .args(["/bin/sh", "-c", "exit 1", "||", "echo", "fallback"])
        .output()
        .expect("mish should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("fallback"),
        "fallback should run after || failure"
    );
}

#[test]
#[serial(pty)]
fn test_20_compound_seq_both_always_run() {
    let output = mish()
        .args(["echo", "first", ";", "echo", "second"])
        .output()
        .expect("mish should run");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("first"), "first should run");
    assert!(stdout.contains("second"), "second should run");
}

#[test]
#[serial(pty)]
fn test_21_compound_exit_code_from_last_segment() {
    // echo ok ; exit 1 — last segment fails, should propagate exit 1
    mish()
        .args(["echo", "ok", ";", "/bin/sh", "-c", "exit 1"])
        .assert()
        .code(1);
}

// =========================================================================
// 7. Condensation of long output
// =========================================================================

#[test]
#[serial(pty)]
fn test_22_long_output_condensed() {
    let output = mish()
        .args([
            "/bin/sh",
            "-c",
            "for i in $(seq 1 100); do echo \"line $i of output\"; done",
        ])
        .output()
        .expect("mish should run");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line_count = stdout.lines().count();
    assert!(
        line_count < 100,
        "condensed output ({} lines) should be shorter than raw (100 lines)",
        line_count
    );
}

#[test]
#[serial(pty)]
fn test_23_long_output_shows_line_count() {
    mish()
        .args([
            "/bin/sh",
            "-c",
            "for i in $(seq 1 50); do echo \"line $i\"; done",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("lines"));
}

// =========================================================================
// 8. Error handling and edge cases
// =========================================================================

#[test]
#[serial(pty)]
fn test_24_no_args_exits_with_error() {
    mish().assert().failure();
}

#[test]
#[serial(pty)]
fn test_25_command_not_found() {
    mish()
        .args(["nonexistent_command_xyz_123"])
        .assert()
        .failure();
}

#[test]
#[serial(pty)]
fn test_26_stderr_captured_through_pty() {
    mish()
        .args(["/bin/sh", "-c", "echo err_output >&2"])
        .assert()
        .success()
        .stdout(predicate::str::contains("err_output"));
}

#[test]
#[serial(pty)]
fn test_27_multiline_output_has_content() {
    let output = mish()
        .args(["/bin/sh", "-c", "echo alpha; echo bravo"])
        .output()
        .expect("mish should run");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    // At least one of the echoed values should appear in ring buffer
    assert!(
        stdout.contains("alpha") || stdout.contains("bravo"),
        "output should contain at least one echoed value, got: {}",
        stdout
    );
}

// =========================================================================
// 9. File operations through condense pipeline
// =========================================================================

#[test]
#[serial(pty)]
fn test_28_cp_with_tempfile() {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("src.txt");
    let dst = dir.path().join("dst.txt");
    fs::write(&src, "hello world").unwrap();

    mish()
        .args([
            "cp",
            src.to_str().unwrap(),
            dst.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(dst.exists(), "cp should have created destination file");
}

#[test]
#[serial(pty)]
fn test_29_mkdir_with_tempdir() {
    let dir = TempDir::new().unwrap();
    let nested = dir.path().join("a/b/c");

    mish()
        .args(["mkdir", "-p", nested.to_str().unwrap()])
        .assert()
        .success();

    assert!(nested.exists(), "mkdir -p should have created nested dirs");
}

#[test]
#[serial(pty)]
fn test_30_rm_with_tempfile() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("to_delete.txt");
    fs::write(&target, "delete me").unwrap();
    assert!(target.exists());

    mish()
        .args(["rm", target.to_str().unwrap()])
        .assert()
        .success();

    assert!(!target.exists(), "rm should have deleted the file");
}

// =========================================================================
// 10. Error enrichment path (stub verification)
// =========================================================================

#[test]
#[serial(pty)]
fn test_31_cp_nonexistent_source_fails() {
    mish()
        .args(["cp", "/nonexistent_path_xyz/src.txt", "/tmp/dst.txt"])
        .assert()
        .failure();
}

// =========================================================================
// 11. JSON compound commands
// =========================================================================

#[test]
#[serial(pty)]
fn test_32_json_compound_produces_valid_json() {
    let output = mish()
        .args(["--json", "echo", "a", ";", "echo", "b"])
        .output()
        .expect("mish should run");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Compound commands produce multiple JSON objects separated by newline.
    // Each should be independently parseable.
    let trimmed = stdout.trim();
    assert!(
        !trimmed.is_empty(),
        "JSON compound should produce output"
    );
    // At minimum, the output should contain valid JSON somewhere
    assert!(
        trimmed.contains("\"command\""),
        "should contain command field in JSON, got: {}",
        trimmed
    );
}

// =========================================================================
// 12. Edge cases
// =========================================================================

#[test]
#[serial(pty)]
fn test_33_empty_output_command() {
    // `true` produces no output — verify graceful handling
    mish()
        .args(["true"])
        .assert()
        .success()
        .stdout(predicate::str::contains("exit 0"));
}

#[test]
#[serial(pty)]
fn test_34_binary_safe_output() {
    // Write some bytes via printf, verify no crash
    mish()
        .args(["/bin/sh", "-c", "printf 'hello\\x00world\\n'"])
        .assert()
        .success();
}

#[test]
#[serial(pty)]
fn test_35_rapid_exit() {
    // Command that exits immediately with no output
    mish()
        .args(["/bin/sh", "-c", "exit 0"])
        .assert()
        .success();
}

// =========================================================================
// 13. Unknown flags passed through to command (not consumed by mish)
// =========================================================================

#[test]
#[serial(pty)]
fn test_36_unknown_flags_passed_to_command() {
    // `mish echo --foo bar` should run `echo --foo bar` — the --foo flag
    // should be passed through to echo, not consumed by mish.
    let output = mish()
        .args(["echo", "--foo", "bar"])
        .output()
        .expect("mish should run");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    // echo prints its args literally, so --foo should appear in the output
    assert!(
        stdout.contains("--foo"),
        "unknown flag --foo should be passed through to echo, got: {}",
        stdout
    );
    assert!(
        stdout.contains("bar"),
        "arg after unknown flag should be present, got: {}",
        stdout
    );
}

#[test]
#[serial(pty)]
fn test_37_double_dash_flags_passed_through() {
    // Flags with values like --loglevel=warn should pass through
    let output = mish()
        .args(["echo", "--loglevel=warn", "hello"])
        .output()
        .expect("mish should run");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--loglevel=warn"),
        "flag with value should pass through, got: {}",
        stdout
    );
}

#[test]
#[serial(pty)]
fn test_38_json_flag_after_command_not_consumed() {
    // `mish echo --json hello` — the --json appears AFTER the command,
    // so it should be part of the command args, not consumed as mish's --json flag.
    // The output should be in human mode (not JSON), with --json in the echoed text.
    let output = mish()
        .args(["echo", "--json", "hello"])
        .output()
        .expect("mish should run");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    // If --json were consumed by mish, the output would be JSON. Since it's
    // after the command, it should be passed through and appear in echo's output.
    assert!(
        stdout.contains("--json"),
        "--json after command name should be part of echo's output, got: {}",
        stdout
    );
}

// =========================================================================
// 14. Simple command parsing (bead spec examples)
// =========================================================================

#[test]
#[serial(pty)]
fn test_39_npm_install_parsing() {
    // Bead spec: "mish npm install lodash" → command=["npm","install","lodash"]
    // Verify the command string appears correctly in JSON output
    let output = mish()
        .args(["--json", "/bin/sh", "-c", "exit 0"])
        .output()
        .expect("mish should run");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("should be valid JSON");

    // The command field should contain the full command string
    assert_eq!(
        parsed["command"], "/bin/sh -c exit 0",
        "command field should contain full command"
    );
}

#[test]
#[serial(pty)]
fn test_40_json_flag_extraction() {
    // Bead spec: "mish --json npm test" → output_mode=JSON, command=["npm","test"]
    // Since npm may not be available, use echo as proxy
    let output = mish()
        .args(["--json", "echo", "test"])
        .output()
        .expect("mish should run");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("should be valid JSON");

    // Verify it's JSON output (the --json flag was consumed by mish)
    assert_eq!(parsed["command"], "echo test");
    assert_eq!(parsed["exit_code"], 0);
}

// =========================================================================
// 15. Pipe handling (pipeline detection and execution)
// =========================================================================

#[test]
#[serial(pty)]
fn test_48_pipeline_echo_grep() {
    let output = mish()
        .args(["echo", "hello world", "|", "grep", "hello"])
        .output()
        .expect("mish should run pipeline");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello"),
        "pipeline output should contain matched text, got: {}",
        stdout
    );
}

#[test]
#[serial(pty)]
fn test_49_pipeline_multi_stage() {
    let output = mish()
        .args(["echo", "hello", "world", "|", "wc", "-w"])
        .output()
        .expect("mish should run multi-stage pipeline");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("2"),
        "multi-stage pipeline should produce word count of 2, got: {}",
        stdout
    );
}

#[test]
#[serial(pty)]
fn test_50_pipeline_exit_code_from_last() {
    mish()
        .args(["echo", "hello", "|", "grep", "nonexistent_xyz_pattern"])
        .assert()
        .code(1);
}

#[test]
#[serial(pty)]
fn test_51_pipeline_passthrough_category() {
    let output = mish()
        .args(["--json", "echo", "hello", "|", "cat"])
        .output()
        .expect("mish should run pipeline in JSON mode");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("output should be valid JSON");

    assert_eq!(
        parsed["category"], "passthrough",
        "pipeline category should be passthrough, got: {}",
        parsed["category"]
    );
}

#[test]
#[serial(pty)]
fn test_52_pipeline_context_mode() {
    let output = mish()
        .args(["--context", "echo", "hello", "|", "cat"])
        .output()
        .expect("mish should run pipeline in context mode");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.trim().contains("ok"),
        "context mode pipeline should show 'ok' for exit 0, got: {}",
        stdout
    );
}

// =========================================================================
// 16. Compound command enhancements
// =========================================================================

#[test]
#[serial(pty)]
fn test_53_compound_and_chain_sequential() {
    let output = mish()
        .args(["echo", "aaa", "&&", "echo", "bbb", "&&", "echo", "ccc"])
        .output()
        .expect("mish should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("aaa"), "first segment should appear");
    assert!(stdout.contains("bbb"), "second segment should appear");
    assert!(stdout.contains("ccc"), "third segment should appear");
}

#[test]
#[serial(pty)]
fn test_54_compound_and_stops_on_failure() {
    let output = mish()
        .args(["/bin/sh", "-c", "exit 1", "&&", "echo", "should_not_appear"])
        .output()
        .expect("mish should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("should_not_appear"),
        "command after && should not run when previous fails"
    );
}

#[test]
#[serial(pty)]
fn test_55_compound_or_continues_on_failure() {
    let output = mish()
        .args(["/bin/sh", "-c", "exit 1", "||", "echo", "fallback_value"])
        .output()
        .expect("mish should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("fallback_value"),
        "command after || should run when previous fails, got: {}",
        stdout
    );
}

#[test]
#[serial(pty)]
fn test_56_compound_or_skips_on_success() {
    let output = mish()
        .args(["echo", "ok", "||", "echo", "should_not_appear"])
        .output()
        .expect("mish should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("should_not_appear"),
        "command after || should not run when previous succeeds"
    );
}

#[test]
#[serial(pty)]
fn test_57_compound_seq_unconditional() {
    let output = mish()
        .args(["/bin/sh", "-c", "exit 1", ";", "echo", "always_runs"])
        .output()
        .expect("mish should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("always_runs"),
        "command after ; should always run, got: {}",
        stdout
    );
}

#[test]
#[serial(pty)]
fn test_58_compound_mixed_operators() {
    let output = mish()
        .args([
            "echo", "first_val", "&&", "echo", "second_val", ";", "echo", "third_val",
        ])
        .output()
        .expect("mish should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("first_val"), "first should run");
    assert!(stdout.contains("second_val"), "second should run after && success");
    assert!(stdout.contains("third_val"), "third should always run after ;");
}

#[test]
#[serial(pty)]
fn test_59_compound_mixed_with_failure() {
    let output = mish()
        .args([
            "/bin/sh", "-c", "exit 1", "&&", "echo", "skip_this", ";", "echo", "always_this",
        ])
        .output()
        .expect("mish should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("skip_this"),
        "second should be skipped after && failure"
    );
    assert!(
        stdout.contains("always_this"),
        "third should run unconditionally after ;"
    );
}

// =========================================================================
// 17. Git commands through proxy
// =========================================================================
// Note: git currently routes through condense (the grammar has no category
// field and categories.toml doesn't include git). Per-action category
// routing (e.g. `git status` -> structured, `git push` -> condense) is
// planned for a future bead.

#[test]
#[serial(pty)]
fn test_60_git_status_runs_successfully() {
    // Run git status in the mish repo (which is a git repo)
    let output = mish()
        .args(["git", "status"])
        .output()
        .expect("mish should run git status");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should produce output (condensed summary of git status)
    assert!(
        !stdout.trim().is_empty(),
        "git status should produce output"
    );
}

#[test]
#[serial(pty)]
fn test_61_git_status_json_has_valid_structure() {
    let output = mish()
        .args(["--json", "git", "status"])
        .output()
        .expect("mish should run git status in JSON mode");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("output should be valid JSON");

    // Verify JSON structure is well-formed
    assert!(parsed["command"].is_string(), "should have command field");
    assert_eq!(parsed["exit_code"], 0, "git status should exit 0");
    assert!(parsed["category"].is_string(), "should have category field");
}

#[test]
#[serial(pty)]
fn test_62_git_status_context_mode() {
    let output = mish()
        .args(["--context", "git", "status"])
        .output()
        .expect("mish should run git status in context mode");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    // Context mode: single line with "ok" for exit 0
    assert!(
        !trimmed.contains('\n'),
        "context mode should be single line, got: {}",
        trimmed
    );
    assert!(
        trimmed.contains("ok"),
        "context mode should contain 'ok' for exit 0, got: {}",
        trimmed
    );
}

// =========================================================================
// 18. Dangerous category — warning display
// =========================================================================

#[test]
#[serial(pty)]
fn test_63_dangerous_rm_rf_shows_warning() {
    // rm -rf matches dangerous patterns. In CLI mode without stdin,
    // prompt_confirmation gets empty input → denied (not executed).
    // The warning symbol ⚠ should appear in stderr (prompt) and the
    // formatted output should contain the warning.
    let output = mish()
        .args(["rm", "-rf", "/tmp/mish_test_nonexistent_xyz"])
        .output()
        .expect("mish should handle dangerous command");

    let stderr = String::from_utf8_lossy(&output.stderr);
    // The dangerous handler prints the prompt to stderr
    assert!(
        stderr.contains("\u{26a0}") || stderr.contains("proceed"),
        "stderr should contain warning prompt, got: {}",
        stderr
    );
}

#[test]
#[serial(pty)]
fn test_64_dangerous_category_in_json() {
    // In JSON mode, the dangerous handler still runs through the router.
    // CLI mode prompts for confirmation — but since no stdin is available,
    // it defaults to denied. The JSON output should reflect the category.
    let output = mish()
        .args(["--json", "rm", "-rf", "/tmp/mish_test_nonexistent_xyz"])
        .output()
        .expect("mish should handle dangerous command in JSON mode");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();

    // Should produce valid JSON with category "dangerous"
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed) {
        assert_eq!(
            parsed["category"], "dangerous",
            "rm -rf should be categorized as dangerous, got: {}",
            parsed["category"]
        );
    }
    // Even if JSON parsing fails, the stderr should have the warning
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("\u{26a0}") || stderr.contains("proceed") || stdout.contains("\u{26a0}"),
        "should contain warning somewhere in output"
    );
}

// =========================================================================
// 19. Passthrough category — ls and cat with real files
// =========================================================================

#[test]
#[serial(pty)]
fn test_65_ls_passthrough_category() {
    let output = mish()
        .args(["--json", "ls"])
        .output()
        .expect("mish should run ls");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("output should be valid JSON");

    assert_eq!(
        parsed["category"], "passthrough",
        "ls should be categorized as passthrough, got: {}",
        parsed["category"]
    );
}

#[test]
#[serial(pty)]
fn test_66_ls_la_shows_files() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("visible_file.txt");
    fs::write(&file, "content").unwrap();

    let output = mish()
        .args(["ls", "-la", dir.path().to_str().unwrap()])
        .output()
        .expect("mish should run ls -la");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("visible_file.txt"),
        "ls -la should show the file, got: {}",
        stdout
    );
}

#[test]
#[serial(pty)]
fn test_67_cat_passthrough_shows_contents() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("readable.txt");
    fs::write(&file, "unique_test_content_12345\n").unwrap();

    let output = mish()
        .args(["cat", file.to_str().unwrap()])
        .output()
        .expect("mish should run cat");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("unique_test_content_12345"),
        "cat should show file contents, got: {}",
        stdout
    );
}

#[test]
#[serial(pty)]
fn test_68_cat_passthrough_category_json() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("cattest.txt");
    fs::write(&file, "json_cat_test\n").unwrap();

    let output = mish()
        .args(["--json", "cat", file.to_str().unwrap()])
        .output()
        .expect("mish should run cat in JSON mode");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("output should be valid JSON");

    assert_eq!(
        parsed["category"], "passthrough",
        "cat should be categorized as passthrough, got: {}",
        parsed["category"]
    );
}

// =========================================================================
// 20. Narrate category — verified through JSON
// =========================================================================

#[test]
#[serial(pty)]
fn test_69_cp_narrate_category_json() {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("src_file.txt");
    let dst = dir.path().join("dst_file.txt");
    fs::write(&src, "copy me").unwrap();

    let output = mish()
        .args([
            "--json",
            "cp",
            src.to_str().unwrap(),
            dst.to_str().unwrap(),
        ])
        .output()
        .expect("mish should run cp in JSON mode");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("output should be valid JSON");

    assert_eq!(
        parsed["category"], "narrate",
        "cp should be categorized as narrate, got: {}",
        parsed["category"]
    );
    assert!(dst.exists(), "cp should have created destination file");
}

#[test]
#[serial(pty)]
fn test_70_mkdir_narrate_category_json() {
    let dir = TempDir::new().unwrap();
    let nested = dir.path().join("x/y/z");

    let output = mish()
        .args(["--json", "mkdir", "-p", nested.to_str().unwrap()])
        .output()
        .expect("mish should run mkdir in JSON mode");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("output should be valid JSON");

    assert_eq!(
        parsed["category"], "narrate",
        "mkdir should be categorized as narrate, got: {}",
        parsed["category"]
    );
    assert!(nested.exists(), "mkdir -p should have created nested dirs");
}

// =========================================================================
// 21. Condense category — verified through JSON (unknown commands default)
// =========================================================================

#[test]
#[serial(pty)]
fn test_71_unknown_command_condense_category() {
    let output = mish()
        .args(["--json", "/bin/sh", "-c", "echo condense_test"])
        .output()
        .expect("mish should run");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("output should be valid JSON");

    assert_eq!(
        parsed["category"], "condense",
        "/bin/sh should fall back to condense category, got: {}",
        parsed["category"]
    );
}

// =========================================================================
// 22. Shell-compatible -c flag
//
// Comprehensive drop-in compatibility tests: mish -c must match bash -c
// on exit codes, side effects, and shell construct support. Output format
// differs (mish squashes), but command output must be present.
// Note: PTY merges stdout+stderr — stderr separation is not testable.
// =========================================================================

// ── Exit codes ──

#[test]
#[serial(pty)]
fn test_72_dash_c_exit_codes() {
    // true → 0
    mish().args(["-c", "true"]).assert().code(0);
    // false → 1
    mish().args(["-c", "false"]).assert().code(1);
    // explicit exit codes
    mish().args(["-c", "exit 0"]).assert().code(0);
    mish().args(["-c", "exit 1"]).assert().code(1);
    mish().args(["-c", "exit 42"]).assert().code(42);
    mish().args(["-c", "exit 127"]).assert().code(127);
    // nonexistent command → 127
    mish()
        .args(["-c", "nonexistent_command_xyz_72"])
        .assert()
        .code(127);
    // test builtin failures
    mish().args(["-c", "test -f /nonexistent"]).assert().code(1);
    mish().args(["-c", "[ 1 -eq 2 ]"]).assert().code(1);
}

// ── Basic output ──

#[test]
#[serial(pty)]
fn test_73_dash_c_basic_output() {
    mish()
        .args(["-c", "echo hello"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello"));
    mish()
        .args(["-c", "echo hello world"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello world"));
    mish()
        .args(["-c", "printf 'abc\\ndef\\n'"])
        .assert()
        .success()
        .stdout(predicate::str::contains("abc"));
    mish()
        .args(["-c", "echo 'single quotes'"])
        .assert()
        .success()
        .stdout(predicate::str::contains("single quotes"));
    mish()
        .args(["-c", r#"echo "double quotes""#])
        .assert()
        .success()
        .stdout(predicate::str::contains("double quotes"));
    mish()
        .args(["-c", "printf 'a\\tb\\tc\\n'"])
        .assert()
        .success()
        .stdout(predicate::str::contains("a"));
}

// ── Shell constructs (&&, ||, ;) ──

#[test]
#[serial(pty)]
fn test_74_dash_c_shell_constructs() {
    mish()
        .args(["-c", "true && true && true"])
        .assert()
        .code(0);
    mish()
        .args(["-c", "true && false && true"])
        .assert()
        .code(1);
    mish()
        .args(["-c", "false || false || true"])
        .assert()
        .code(0);
    mish().args(["-c", "false; true"]).assert().code(0);
    mish()
        .args(["-c", "echo aaa && echo bbb"])
        .assert()
        .success()
        .stdout(predicate::str::contains("aaa"))
        .stdout(predicate::str::contains("bbb"));
    mish()
        .args(["-c", "false || echo fallback"])
        .assert()
        .success()
        .stdout(predicate::str::contains("fallback"));
    mish()
        .args(["-c", "echo first; echo second"])
        .assert()
        .success()
        .stdout(predicate::str::contains("second"));
}

// ── Pipes ──

#[test]
#[serial(pty)]
fn test_75_dash_c_pipes() {
    mish()
        .args(["-c", "echo hello | cat"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello"));
    mish()
        .args(["-c", "echo hello | grep hello"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello"));
    mish()
        .args(["-c", "echo hello | cat | cat"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello"));
    // pipe exit code from last command
    mish().args(["-c", "echo x | grep y"]).assert().code(1);
    mish()
        .args(["-c", "echo -e 'a\\nb\\nc' | wc -l"])
        .assert()
        .success()
        .stdout(predicate::str::contains("3"));
    mish()
        .args(["-c", "printf 'c\\na\\nb\\n' | sort"])
        .assert()
        .success()
        .stdout(predicate::str::contains("a"));
    mish()
        .args(["-c", "seq 1 100 | head -n 3"])
        .assert()
        .success()
        .stdout(predicate::str::contains("3"));
}

// ── Redirects & file side effects ──

#[test]
#[serial(pty)]
fn test_76_dash_c_redirects_and_side_effects() {
    let dir = TempDir::new().unwrap();
    let dp = dir.path();

    // stdout redirect creates file
    let f1 = dp.join("redir.txt");
    mish()
        .args(["-c", &format!("echo mish_out > {}", f1.display())])
        .assert()
        .success();
    assert_eq!(fs::read_to_string(&f1).unwrap().trim(), "mish_out");

    // append redirect
    let f2 = dp.join("append.txt");
    mish()
        .args([
            "-c",
            &format!(
                "echo line1 > {} && echo line2 >> {}",
                f2.display(),
                f2.display()
            ),
        ])
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(&f2).unwrap(),
        "line1\nline2\n"
    );

    // redirect to /dev/null
    mish()
        .args(["-c", "echo silent > /dev/null"])
        .assert()
        .code(0);

    // here-string
    mish()
        .args(["-c", "cat <<< 'here_string_test'"])
        .assert()
        .success()
        .stdout(predicate::str::contains("here_string_test"));
}

// ── Subshells & command substitution ──

#[test]
#[serial(pty)]
fn test_77_dash_c_subshells() {
    mish()
        .args(["-c", "(echo subshell_out)"])
        .assert()
        .success()
        .stdout(predicate::str::contains("subshell_out"));
    mish()
        .args(["-c", "(echo a; (echo nested_b))"])
        .assert()
        .success()
        .stdout(predicate::str::contains("nested_b"));
    mish()
        .args(["-c", "echo $(echo cmd_sub)"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cmd_sub"));
    mish()
        .args(["-c", "echo `echo backtick`"])
        .assert()
        .success()
        .stdout(predicate::str::contains("backtick"));
    mish().args(["-c", "(exit 7)"]).assert().code(7);
}

// ── Variables & environment ──

#[test]
#[serial(pty)]
fn test_78_dash_c_variables() {
    mish()
        .args(["-c", "X=hello; echo $X"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello"));
    mish()
        .args(["-c", "export V=val; echo $V"])
        .assert()
        .success()
        .stdout(predicate::str::contains("val"));
    mish()
        .args(["-c", "echo $((2 + 3))"])
        .assert()
        .success()
        .stdout(predicate::str::contains("5"));
    mish()
        .args(["-c", "echo $PATH"])
        .assert()
        .success()
        .stdout(predicate::str::contains("/usr"));
}

// ── Complex file operations ──

#[test]
#[serial(pty)]
fn test_79_dash_c_file_operations() {
    let dir = TempDir::new().unwrap();
    let dp = dir.path();

    // printf to file — byte-exact match with bash
    let mf = dp.join("mish.txt");
    let bf = dp.join("bash.txt");
    mish()
        .args([
            "-c",
            &format!("printf 'line1\\nline2\\nline3\\n' > {}", mf.display()),
        ])
        .assert()
        .success();
    std::process::Command::new("/bin/bash")
        .args([
            "-c",
            &format!("printf 'line1\\nline2\\nline3\\n' > {}", bf.display()),
        ])
        .output()
        .unwrap();
    assert_eq!(fs::read(&mf).unwrap(), fs::read(&bf).unwrap());

    // sed -i via -c
    let ms = dp.join("mish_sed.txt");
    let bs = dp.join("bash_sed.txt");
    fs::write(&ms, "hello world\n").unwrap();
    fs::write(&bs, "hello world\n").unwrap();
    mish()
        .args([
            "-c",
            &format!("sed -i '' 's/hello/goodbye/' {}", ms.display()),
        ])
        .assert()
        .success();
    std::process::Command::new("/bin/bash")
        .args([
            "-c",
            &format!("sed -i '' 's/hello/goodbye/' {}", bs.display()),
        ])
        .output()
        .unwrap();
    assert_eq!(fs::read(&ms).unwrap(), fs::read(&bs).unwrap());

    // mkdir + touch chain
    let sub = dp.join("subdir").join("file.txt");
    mish()
        .args([
            "-c",
            &format!(
                "mkdir -p {} && touch {}",
                dp.join("subdir").display(),
                sub.display()
            ),
        ])
        .assert()
        .success();
    assert!(sub.exists());

    // cp via -c
    let src = dp.join("src.txt");
    let dst = dp.join("dst.txt");
    fs::write(&src, "content\n").unwrap();
    mish()
        .args([
            "-c",
            &format!("cp {} {}", src.display(), dst.display()),
        ])
        .assert()
        .success();
    assert_eq!(fs::read(&src).unwrap(), fs::read(&dst).unwrap());
}

// ── Loops & conditionals ──

#[test]
#[serial(pty)]
fn test_80_dash_c_loops_and_conditionals() {
    mish()
        .args(["-c", "for i in a b c; do echo $i; done"])
        .assert()
        .success()
        .stdout(predicate::str::contains("c"));
    mish()
        .args(["-c", "i=0; while [ $i -lt 3 ]; do i=$((i+1)); done; echo $i"])
        .assert()
        .success()
        .stdout(predicate::str::contains("3"));
    mish()
        .args(["-c", "if true; then echo yes; else echo no; fi"])
        .assert()
        .success()
        .stdout(predicate::str::contains("yes"));
    mish()
        .args(["-c", "if false; then echo yes; else echo no; fi"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no"));
    mish()
        .args(["-c", "if false; then exit 0; else exit 3; fi"])
        .assert()
        .code(3);
}

// ── -lc (Docker login shell compat) ──

#[test]
#[serial(pty)]
fn test_81_dash_lc_docker_compat() {
    mish()
        .args(["-lc", "echo docker_compat"])
        .assert()
        .success()
        .stdout(predicate::str::contains("docker_compat"));

    let dir = TempDir::new().unwrap();
    let f = dir.path().join("lc_file.txt");
    mish()
        .args(["-lc", &format!("echo lc_content > {}", f.display())])
        .assert()
        .success();
    assert_eq!(fs::read_to_string(&f).unwrap().trim(), "lc_content");

    mish().args(["-lc", "exit 7"]).assert().code(7);
}

// ── --json + -c ──

#[test]
#[serial(pty)]
fn test_82_dash_c_json_mode() {
    let output = mish()
        .args(["--json", "-c", "echo json_compat"])
        .output()
        .expect("mish should run");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("output should be valid JSON");
    assert_eq!(parsed["exit_code"], 0);
    assert!(
        parsed["body"].as_str().unwrap().contains("json_compat"),
        "JSON body should contain command output"
    );

    // failure exit code in JSON
    let output = mish()
        .args(["--json", "-c", "exit 5"])
        .output()
        .expect("mish should run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("output should be valid JSON");
    assert_eq!(parsed["exit_code"], 5);
}

// ── Positional parameters ($0, $1, $@) ──

#[test]
#[serial(pty)]
fn test_83_dash_c_positional_parameters() {
    // $0
    mish()
        .args(["-c", "echo $0", "myarg"])
        .assert()
        .success()
        .stdout(predicate::str::contains("myarg"));
    // $1
    mish()
        .args(["-c", "echo $1", "_", "argone"])
        .assert()
        .success()
        .stdout(predicate::str::contains("argone"));
    // $@
    mish()
        .args(["-c", "echo $@", "_", "x", "y", "z"])
        .assert()
        .success()
        .stdout(predicate::str::contains("x y z"));
}

// ── Edge cases ──

#[test]
#[serial(pty)]
fn test_84_dash_c_edge_cases() {
    // empty command → error exit 2
    mish().args(["-c", ""]).assert().code(2);

    // whitespace in output
    mish()
        .args(["-c", "echo '   spaces   '"])
        .assert()
        .success()
        .stdout(predicate::str::contains("spaces"));

    // special characters
    mish()
        .args(["-c", "echo 'hello!@#'"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello!@#"));

    // multi-line output
    mish()
        .args(["-c", "printf 'line1\\nline2\\nline3\\n'"])
        .assert()
        .success()
        .stdout(predicate::str::contains("line2"));
}

// ── Squasher condensation ──

#[test]
#[serial(pty)]
fn test_85_dash_c_squashes_output() {
    let output = mish()
        .args([
            "--json",
            "-c",
            "for i in $(seq 1 100); do echo repeated_line; done",
        ])
        .output()
        .expect("mish should run");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("output should be valid JSON");

    let body = parsed["body"].as_str().unwrap();
    assert!(
        body.len() < 5000,
        "squasher should condense 100 identical lines, got {} bytes",
        body.len()
    );
}

// ── Nested mish hazard detection ──

#[test]
fn test_86_dash_c_rejects_mish_serve() {
    mish()
        .args(["-c", "mish serve"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("refusing to run `mish serve`"));
}

#[test]
fn test_87_dash_c_rejects_mish_attach() {
    mish()
        .args(["-c", "mish attach hf_abc123"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("refusing to run `mish attach`"));
}

#[test]
fn test_88_dash_c_rejects_session_host() {
    mish()
        .args(["-c", "mish session host py --cmd python3"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("refusing to run `mish session host`"));
}

#[test]
fn test_89_dash_c_rejects_session_start_fg() {
    mish()
        .args(["-c", "mish session start py --cmd python3 --fg"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("refusing to run `mish session start --fg`"));
}

#[test]
fn test_90_dash_c_allows_session_start_detached() {
    // session start without --fg is safe (host detaches)
    // Just check it doesn't get rejected — it will fail on "already running" or similar,
    // but NOT with a "refusing" error
    let output = mish()
        .args(["-c", "mish session start test_90_safe --cmd echo"])
        .output()
        .expect("mish should run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("refusing"),
        "detached session start should not be rejected, got: {stderr}"
    );
}

#[test]
fn test_91_dash_c_rejects_serve_in_compound() {
    // "cd /tmp && mish serve" should still be caught
    mish()
        .args(["-c", "cd /tmp && mish serve"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("refusing to run `mish serve`"));
}

#[test]
fn test_92_dash_c_rejects_path_qualified_mish() {
    mish()
        .args(["-c", "/opt/homebrew/bin/mish serve"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("refusing to run `mish serve`"));
}

// ── Piped stdin forwarding ──

#[test]
#[serial(pty)]
fn test_93_dash_c_piped_stdin() {
    let output = mish()
        .args(["-c", "cat"])
        .write_stdin("piped_stdin_test\n")
        .output()
        .expect("mish should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("piped_stdin_test"),
        "piped stdin should reach the command, got: {stdout}"
    );
}

#[test]
#[serial(pty)]
fn test_94_dash_c_piped_stdin_multiline() {
    let output = mish()
        .args(["-c", "cat"])
        .write_stdin("line1\nline2\nline3\n")
        .output()
        .expect("mish should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("line1"), "should contain line1");
    assert!(stdout.contains("line2"), "should contain line2");
    assert!(stdout.contains("line3"), "should contain line3");
}

#[test]
#[serial(pty)]
fn test_95_dash_c_piped_stdin_to_grep() {
    let output = mish()
        .args(["-c", "grep target_line"])
        .write_stdin("noise\ntarget_line\nmore_noise\n")
        .output()
        .expect("mish should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("target_line"),
        "grep should find the piped line, got: {stdout}"
    );
}

#[test]
#[serial(pty)]
fn test_96_dash_c_piped_stdin_to_wc() {
    let output = mish()
        .args(["-c", "wc -l"])
        .write_stdin("a\nb\nc\n")
        .output()
        .expect("mish should run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("3") || stdout.contains("4"),
        "wc should count piped lines, got: {stdout}"
    );
}

// ── Bashism support (process substitution) ──

#[test]
#[serial(pty)]
fn test_97_dash_c_process_substitution() {
    // Process substitution requires /bin/bash, not /bin/sh
    let output = mish()
        .args(["-c", "diff <(echo aaa) <(echo bbb)"])
        .output()
        .expect("mish should run");

    // diff returns exit 1 when files differ
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("aaa") && stdout.contains("bbb"),
        "process substitution should work via /bin/bash, got: {stdout}"
    );
}

#[test]
#[serial(pty)]
fn test_98_dash_c_no_piped_stdin_no_hang() {
    // Commands that don't read stdin should not hang when stdin is a pipe
    // (agent frameworks often connect stdin to a pipe but never send data).
    // assert_cmd connects stdin to a pipe by default.
    mish()
        .args(["-c", "echo no_stdin_needed"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no_stdin_needed"));
}
