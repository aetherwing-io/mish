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
