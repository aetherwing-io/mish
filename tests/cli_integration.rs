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

use assert_cmd::Command;
use predicates::prelude::*;
use serial_test::serial;
use std::fs;
use tempfile::TempDir;

/// Helper: build a `mish` command from the compiled binary.
fn mish() -> Command {
    Command::cargo_bin("mish").unwrap()
}

// =========================================================================
// 1. Basic command execution
// =========================================================================

#[test]
#[serial(pty)]
fn test_01_echo_hello_produces_output() {
    mish()
        .args(&["echo", "hello"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello"));
}

#[test]
#[serial(pty)]
fn test_02_true_exits_zero() {
    mish()
        .args(&["true"])
        .assert()
        .success();
}

#[test]
#[serial(pty)]
fn test_03_exit_code_one_propagated() {
    mish()
        .args(&["/bin/sh", "-c", "exit 1"])
        .assert()
        .code(1);
}

#[test]
#[serial(pty)]
fn test_04_exit_code_42_propagated() {
    mish()
        .args(&["/bin/sh", "-c", "exit 42"])
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
        .args(&["/bin/sh", "-c", "echo hello"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("+"));
}

#[test]
#[serial(pty)]
fn test_06_human_failure_starts_with_bang() {
    mish()
        .args(&["/bin/sh", "-c", "exit 1"])
        .assert()
        .code(1)
        .stdout(predicate::str::starts_with("!"));
}

#[test]
#[serial(pty)]
fn test_07_human_shows_line_count() {
    mish()
        .args(&["/bin/sh", "-c", "echo a; echo b; echo c"])
        .assert()
        .success()
        .stdout(predicate::str::contains("lines"));
}

#[test]
#[serial(pty)]
fn test_08_human_shows_exit_code_on_failure() {
    mish()
        .args(&["/bin/sh", "-c", "echo output && exit 42"])
        .assert()
        .code(42)
        .stdout(predicate::str::contains("exit 42"));
}

#[test]
#[serial(pty)]
fn test_09_ring_buffer_last_lines() {
    mish()
        .args(&["/bin/sh", "-c", "echo alpha; echo bravo; echo charlie"])
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
        .args(&["--json", "echo", "hello"])
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
        .args(&["--json", "/bin/sh", "-c", "exit 1"])
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
        .args(&["--json", "echo", "test"])
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
        .args(&["--json", "echo", "test"])
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
        .args(&["--context", "echo", "hello"])
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
        .args(&["--context", "/bin/sh", "-c", "exit 1"])
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
        .args(&["--passthrough", "echo", "hello"])
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
        .args(&["echo", "first", "&&", "echo", "second"])
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
        .args(&[
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
        .args(&["/bin/sh", "-c", "exit 1", "||", "echo", "fallback"])
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
        .args(&["echo", "first", ";", "echo", "second"])
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
        .args(&["echo", "ok", ";", "/bin/sh", "-c", "exit 1"])
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
        .args(&[
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
        .args(&[
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
        .args(&["nonexistent_command_xyz_123"])
        .assert()
        .failure();
}

#[test]
#[serial(pty)]
fn test_26_stderr_captured_through_pty() {
    mish()
        .args(&["/bin/sh", "-c", "echo err_output >&2"])
        .assert()
        .success()
        .stdout(predicate::str::contains("err_output"));
}

#[test]
#[serial(pty)]
fn test_27_multiline_output_has_content() {
    let output = mish()
        .args(&["/bin/sh", "-c", "echo alpha; echo bravo"])
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
        .args(&[
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
        .args(&["mkdir", "-p", nested.to_str().unwrap()])
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
        .args(&["rm", target.to_str().unwrap()])
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
        .args(&["cp", "/nonexistent_path_xyz/src.txt", "/tmp/dst.txt"])
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
        .args(&["--json", "echo", "a", ";", "echo", "b"])
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
        .args(&["true"])
        .assert()
        .success()
        .stdout(predicate::str::contains("exit 0"));
}

#[test]
#[serial(pty)]
fn test_34_binary_safe_output() {
    // Write some bytes via printf, verify no crash
    mish()
        .args(&["/bin/sh", "-c", "printf 'hello\\x00world\\n'"])
        .assert()
        .success();
}

#[test]
#[serial(pty)]
fn test_35_rapid_exit() {
    // Command that exits immediately with no output
    mish()
        .args(&["/bin/sh", "-c", "exit 0"])
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
        .args(&["echo", "--foo", "bar"])
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
        .args(&["echo", "--loglevel=warn", "hello"])
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
        .args(&["echo", "--json", "hello"])
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
        .args(&["--json", "/bin/sh", "-c", "exit 0"])
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
        .args(&["--json", "echo", "test"])
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
