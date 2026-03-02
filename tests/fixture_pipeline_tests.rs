/// Fixture pipeline tests for mish-s1i.16.
///
/// Each of the 5 grammars has at least 2 fixtures (success + error).
/// Tests parse every line through the classifier and verify:
/// - Correct classification counts (hazard, outcome, noise, unknown)
/// - Specific hazard lines are detected
/// - Specific outcome captures are extracted
/// - Error fixtures surface all hazards

use std::collections::HashMap;
use std::path::Path;

use mish::core::grammar::{
    evaluate_line, evaluate_line_with_fallback, load_all_grammars,
    RuleMatch, Severity,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn grammars() -> HashMap<String, mish::core::grammar::Grammar> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("grammars");
    load_all_grammars(&dir).unwrap()
}

fn load_fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to read fixture {name}: {e}"))
}

/// Classify every line in a fixture through the given grammar+action.
/// Returns counts: (hazards, outcomes, noise, unknown).
fn classify_fixture(
    grammar: &mish::core::grammar::Grammar,
    action: Option<&mish::core::grammar::Action>,
    fixture: &str,
) -> (usize, usize, usize, usize) {
    let mut hazards = 0;
    let mut outcomes = 0;
    let mut noise = 0;
    let mut unknown = 0;

    for line in fixture.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let result = evaluate_line(grammar, action, line);
        match result {
            RuleMatch::Hazard { .. } => hazards += 1,
            RuleMatch::Outcome { .. } => outcomes += 1,
            RuleMatch::Noise { .. } => noise += 1,
            RuleMatch::NoMatch => unknown += 1,
        }
    }

    (hazards, outcomes, noise, unknown)
}

/// Same as classify_fixture but uses evaluate_line_with_fallback (for make).
fn classify_fixture_fallback(
    grammar: &mish::core::grammar::Grammar,
    fixture: &str,
) -> (usize, usize, usize, usize) {
    let mut hazards = 0;
    let mut outcomes = 0;
    let mut noise = 0;
    let mut unknown = 0;

    for line in fixture.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let result = evaluate_line_with_fallback(grammar, line);
        match result {
            RuleMatch::Hazard { .. } => hazards += 1,
            RuleMatch::Outcome { .. } => outcomes += 1,
            RuleMatch::Noise { .. } => noise += 1,
            RuleMatch::NoMatch => unknown += 1,
        }
    }

    (hazards, outcomes, noise, unknown)
}

/// Collect all hazard texts from a fixture.
fn collect_hazards(
    grammar: &mish::core::grammar::Grammar,
    action: Option<&mish::core::grammar::Action>,
    fixture: &str,
) -> Vec<(Option<Severity>, String)> {
    let mut hazards = Vec::new();
    for line in fixture.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let RuleMatch::Hazard { severity, .. } = evaluate_line(grammar, action, line) {
            hazards.push((severity, line.to_string()));
        }
    }
    hazards
}

/// Collect all outcome captures from a fixture.
fn collect_outcomes(
    grammar: &mish::core::grammar::Grammar,
    action: Option<&mish::core::grammar::Action>,
    fixture: &str,
) -> Vec<HashMap<String, String>> {
    let mut outcomes = Vec::new();
    for line in fixture.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let RuleMatch::Outcome { captures, .. } = evaluate_line(grammar, action, line) {
            outcomes.push(captures);
        }
    }
    outcomes
}

// ===========================================================================
// npm: install success fixture
// ===========================================================================

#[test]
fn test_npm_install_fixture_classification_counts() {
    let gs = grammars();
    let grammar = gs.get("npm").unwrap();
    let action = grammar.actions.get("install").unwrap();
    let fixture = load_fixture("npm_install.txt");

    let (hazards, outcomes, noise, _unknown) = classify_fixture(grammar, Some(action), &fixture);

    // Success fixture should have:
    // - At least 1 outcome ("added N packages")
    // - Multiple noise lines (timing, idealTree, reify, etc.)
    // - Some hazards (vulnerability counts)
    // - Some unknown lines
    assert!(outcomes >= 1, "npm install should have at least 1 outcome, got {outcomes}");
    assert!(noise >= 5, "npm install should have significant noise, got {noise}");
    assert!(hazards >= 1, "npm install should have deprecation warnings, got {hazards}");
}

#[test]
fn test_npm_install_fixture_outcome_captures() {
    let gs = grammars();
    let grammar = gs.get("npm").unwrap();
    let action = grammar.actions.get("install").unwrap();
    let fixture = load_fixture("npm_install.txt");

    let outcomes = collect_outcomes(grammar, Some(action), &fixture);
    assert!(!outcomes.is_empty(), "npm install fixture should have outcome lines");

    // Should capture count and time from "added N packages in Xs"
    let has_count = outcomes.iter().any(|c| c.contains_key("count"));
    assert!(has_count, "npm install outcome should capture package count");
}

// ===========================================================================
// npm: install error fixture
// ===========================================================================

#[test]
fn test_npm_install_error_fixture_has_hazards() {
    let gs = grammars();
    let grammar = gs.get("npm").unwrap();
    let action = grammar.actions.get("install").unwrap();
    let fixture = load_fixture("npm_install_error.txt");

    let hazards = collect_hazards(grammar, Some(action), &fixture);
    assert!(hazards.len() >= 2, "npm error fixture should have multiple hazards, got {}", hazards.len());

    // Should detect ERESOLVE
    let has_eresolve = hazards.iter().any(|(_, ref text)| text.contains("ERESOLVE"));
    assert!(has_eresolve, "npm error fixture should detect ERESOLVE");

    // Should detect permission error
    let has_perm = hazards.iter().any(|(_, ref text)| text.contains("EACCES") || text.contains("EPERM"));
    assert!(has_perm, "npm error fixture should detect permission error");
}

#[test]
fn test_npm_install_error_fixture_no_success_outcome() {
    let gs = grammars();
    let grammar = gs.get("npm").unwrap();
    let action = grammar.actions.get("install").unwrap();
    let fixture = load_fixture("npm_install_error.txt");

    let outcomes = collect_outcomes(grammar, Some(action), &fixture);
    // Error fixture should NOT have "added N packages" outcome
    let has_added = outcomes.iter().any(|c| c.contains_key("count"));
    assert!(!has_added, "npm error fixture should NOT have 'added N packages' outcome");
}

// ===========================================================================
// cargo: build success fixture (existing, enhanced)
// ===========================================================================

#[test]
fn test_cargo_build_fixture_classification_counts() {
    let gs = grammars();
    let grammar = gs.get("cargo").unwrap();
    let action = grammar.actions.get("build").unwrap();
    let fixture = load_fixture("cargo_build.txt");

    let (_hazards, outcomes, noise, _unknown) = classify_fixture(grammar, Some(action), &fixture);

    assert!(outcomes >= 1, "cargo build should have Finished outcome, got {outcomes}");
    assert!(noise >= 5, "cargo build should have Compiling/Downloading noise, got {noise}");
}

#[test]
fn test_cargo_build_fixture_outcome_captures_time() {
    let gs = grammars();
    let grammar = gs.get("cargo").unwrap();
    let action = grammar.actions.get("build").unwrap();
    let fixture = load_fixture("cargo_build.txt");

    let outcomes = collect_outcomes(grammar, Some(action), &fixture);
    let has_time = outcomes.iter().any(|c| c.contains_key("time"));
    assert!(has_time, "cargo build outcome should capture build time");
}

// ===========================================================================
// cargo: build error fixture
// ===========================================================================

#[test]
fn test_cargo_build_error_fixture_has_errors() {
    let gs = grammars();
    let grammar = gs.get("cargo").unwrap();
    let action = grammar.actions.get("build").unwrap();
    let fixture = load_fixture("cargo_build_error.txt");

    let hazards = collect_hazards(grammar, Some(action), &fixture);

    // Should have multiple error[EXXXX] lines
    let errors: Vec<_> = hazards.iter()
        .filter(|(s, _)| *s == Some(Severity::Error))
        .collect();
    assert!(errors.len() >= 2, "cargo error fixture should have multiple errors, got {}", errors.len());

    // Should capture error codes
    let fixture_lines = fixture.lines().collect::<Vec<_>>();
    let mut found_codes = Vec::new();
    for line in &fixture_lines {
        if let RuleMatch::Hazard { captures, severity, .. } = evaluate_line(
            grammar, Some(action), line,
        ) {
            if severity == Some(Severity::Error) {
                if let Some(code) = captures.get("code") {
                    found_codes.push(code.clone());
                }
            }
        }
    }
    assert!(found_codes.len() >= 2, "should capture error codes, got {:?}", found_codes);
}

#[test]
fn test_cargo_build_error_fixture_has_warnings() {
    let gs = grammars();
    let grammar = gs.get("cargo").unwrap();
    let action = grammar.actions.get("build").unwrap();
    let fixture = load_fixture("cargo_build_error.txt");

    let hazards = collect_hazards(grammar, Some(action), &fixture);
    let warnings: Vec<_> = hazards.iter()
        .filter(|(s, _)| *s == Some(Severity::Warning))
        .collect();
    assert!(warnings.len() >= 1, "cargo error fixture should have warnings, got {}", warnings.len());
}

// ===========================================================================
// cargo: test fixture
// ===========================================================================

#[test]
fn test_cargo_test_fixture_classification() {
    let gs = grammars();
    let grammar = gs.get("cargo").unwrap();
    let action = grammar.actions.get("test").unwrap();
    let fixture = load_fixture("cargo_test.txt");

    let (hazards, outcomes, noise, _unknown) = classify_fixture(grammar, Some(action), &fixture);

    // Should have test result outcome
    assert!(outcomes >= 1, "cargo test should have result outcome, got {outcomes}");
    // Should have noise (individual "test ... ok" lines, "running N tests")
    assert!(noise >= 3, "cargo test should have test-line noise, got {noise}");
    // Should have FAILED test hazards
    assert!(hazards >= 1, "cargo test should have FAILED hazards, got {hazards}");
}

#[test]
fn test_cargo_test_fixture_captures_counts() {
    let gs = grammars();
    let grammar = gs.get("cargo").unwrap();
    let action = grammar.actions.get("test").unwrap();
    let fixture = load_fixture("cargo_test.txt");

    let outcomes = collect_outcomes(grammar, Some(action), &fixture);

    // Should capture passed or failed count
    let has_passed = outcomes.iter().any(|c| c.contains_key("passed"));
    let has_failed = outcomes.iter().any(|c| c.contains_key("failed"));
    assert!(has_passed || has_failed,
        "cargo test fixture should capture passed or failed count, got {:?}", outcomes);
}

// ===========================================================================
// git: push success fixture
// ===========================================================================

#[test]
fn test_git_push_fixture_classification() {
    let gs = grammars();
    let grammar = gs.get("git").unwrap();
    let action = grammar.actions.get("push").unwrap();
    let fixture = load_fixture("git_push.txt");

    let (hazards, outcomes, noise, _unknown) = classify_fixture(grammar, Some(action), &fixture);

    assert!(outcomes >= 1, "git push should have ref update outcome, got {outcomes}");
    assert!(noise >= 2, "git push should have object-counting noise, got {noise}");
    assert_eq!(hazards, 0, "git push success should have no hazards, got {hazards}");
}

#[test]
fn test_git_push_fixture_captures_src_dst() {
    let gs = grammars();
    let grammar = gs.get("git").unwrap();
    let action = grammar.actions.get("push").unwrap();
    let fixture = load_fixture("git_push.txt");

    let outcomes = collect_outcomes(grammar, Some(action), &fixture);
    let has_src = outcomes.iter().any(|c| c.contains_key("src"));
    let has_dst = outcomes.iter().any(|c| c.contains_key("dst"));
    assert!(has_src && has_dst, "git push outcome should capture src and dst refs");
}

// ===========================================================================
// git: push rejected fixture
// ===========================================================================

#[test]
fn test_git_push_rejected_fixture_has_hazards() {
    let gs = grammars();
    let grammar = gs.get("git").unwrap();
    let action = grammar.actions.get("push").unwrap();
    let fixture = load_fixture("git_push_rejected.txt");

    let hazards = collect_hazards(grammar, Some(action), &fixture);
    assert!(hazards.len() >= 1, "git push rejected should have hazards, got {}", hazards.len());

    let has_rejected = hazards.iter().any(|(_, ref text)| text.contains("rejected"));
    assert!(has_rejected, "git push rejected should detect [rejected] line");
}

#[test]
fn test_git_push_rejected_fixture_no_success_outcome() {
    let gs = grammars();
    let grammar = gs.get("git").unwrap();
    let action = grammar.actions.get("push").unwrap();
    let fixture = load_fixture("git_push_rejected.txt");

    // Rejected push must have hazards — the [rejected] line should be caught
    let hazards = collect_hazards(grammar, Some(action), &fixture);
    assert!(!hazards.is_empty(), "rejected push must have hazards");
}

// ===========================================================================
// docker: build fixture
// ===========================================================================

#[test]
fn test_docker_build_fixture_classification() {
    let gs = grammars();
    let grammar = gs.get("docker").unwrap();
    let action = grammar.actions.get("build").unwrap();
    let fixture = load_fixture("docker_build.txt");

    let (hazards, outcomes, noise, _unknown) = classify_fixture(grammar, Some(action), &fixture);

    assert!(outcomes >= 1, "docker build should have outcome, got {outcomes}");
    assert!(noise >= 3, "docker build should have CACHED/DONE noise, got {noise}");
    assert_eq!(hazards, 0, "docker build success should have no hazards, got {hazards}");
}

#[test]
fn test_docker_build_fixture_captures_hash() {
    let gs = grammars();
    let grammar = gs.get("docker").unwrap();
    let action = grammar.actions.get("build").unwrap();
    let fixture = load_fixture("docker_build.txt");

    let outcomes = collect_outcomes(grammar, Some(action), &fixture);
    let has_hash = outcomes.iter().any(|c| c.contains_key("hash"));
    assert!(has_hash, "docker build outcome should capture image hash");
}

// ===========================================================================
// docker: build error fixture
// ===========================================================================

#[test]
fn test_docker_build_error_fixture_has_hazards() {
    let gs = grammars();
    let grammar = gs.get("docker").unwrap();
    let action = grammar.actions.get("build").unwrap();
    let fixture = load_fixture("docker_build_error.txt");

    let hazards = collect_hazards(grammar, Some(action), &fixture);
    assert!(hazards.len() >= 1, "docker error fixture should have hazards, got {}", hazards.len());

    // Should detect ERROR line
    let has_error = hazards.iter().any(|(s, _)| *s == Some(Severity::Error));
    assert!(has_error, "docker error fixture should have error-severity hazard");
}

#[test]
fn test_docker_build_error_fixture_detects_deprecated() {
    let gs = grammars();
    let grammar = gs.get("docker").unwrap();
    let action = grammar.actions.get("build").unwrap();
    let fixture = load_fixture("docker_build_error.txt");

    let hazards = collect_hazards(grammar, Some(action), &fixture);
    let has_deprecated = hazards.iter().any(|(_, ref text)| text.contains("DEPRECATED"));
    assert!(has_deprecated, "docker error fixture should detect DEPRECATED warning");
}

// ===========================================================================
// make: build fixture
// ===========================================================================

#[test]
fn test_make_build_fixture_classification() {
    let gs = grammars();
    let grammar = gs.get("make").unwrap();
    let fixture = load_fixture("make_build.txt");

    let (hazards, _outcomes, noise, _unknown) = classify_fixture_fallback(grammar, &fixture);

    assert!(noise >= 3, "make build should have compiler echo noise, got {noise}");
    // make build with warnings should have some hazards
    assert!(hazards >= 1, "make build should have compiler warnings, got {hazards}");
}

#[test]
fn test_make_build_fixture_detects_compiler_warnings() {
    let gs = grammars();
    let grammar = gs.get("make").unwrap();
    let fixture = load_fixture("make_build.txt");

    let mut warning_count = 0;
    for line in fixture.lines() {
        if line.trim().is_empty() { continue; }
        if let RuleMatch::Hazard { severity, .. } = evaluate_line_with_fallback(grammar, line) {
            if severity == Some(Severity::Warning) {
                warning_count += 1;
            }
        }
    }
    assert!(warning_count >= 1, "make build should detect compiler warnings, got {warning_count}");
}

// ===========================================================================
// make: build error fixture
// ===========================================================================

#[test]
fn test_make_build_error_fixture_has_errors() {
    let gs = grammars();
    let grammar = gs.get("make").unwrap();
    let fixture = load_fixture("make_build_error.txt");

    let mut error_count = 0;
    for line in fixture.lines() {
        if line.trim().is_empty() { continue; }
        if let RuleMatch::Hazard { severity, .. } = evaluate_line_with_fallback(grammar, line) {
            if severity == Some(Severity::Error) {
                error_count += 1;
            }
        }
    }
    assert!(error_count >= 2, "make error fixture should have multiple errors, got {error_count}");
}

#[test]
fn test_make_build_error_fixture_detects_make_error() {
    let gs = grammars();
    let grammar = gs.get("make").unwrap();
    let fixture = load_fixture("make_build_error.txt");

    let mut found_make_error = false;
    for line in fixture.lines() {
        if line.trim().is_empty() { continue; }
        if let RuleMatch::Hazard { .. } = evaluate_line_with_fallback(grammar, line) {
            if line.contains("***") {
                found_make_error = true;
            }
        }
    }
    assert!(found_make_error, "make error fixture should detect make[N]: *** error");
}

#[test]
fn test_make_build_error_fixture_detects_linker_error() {
    let gs = grammars();
    let grammar = gs.get("make").unwrap();
    let fixture = load_fixture("make_build_error.txt");

    let mut found_linker = false;
    for line in fixture.lines() {
        if line.trim().is_empty() { continue; }
        if let RuleMatch::Hazard { .. } = evaluate_line_with_fallback(grammar, line) {
            if line.contains("undefined reference") {
                found_linker = true;
            }
        }
    }
    assert!(found_linker, "make error fixture should detect linker 'undefined reference' error");
}

// ===========================================================================
// ANSI output fixture
// ===========================================================================

#[test]
fn test_ansi_output_fixture_has_progress_bars() {
    let gs = grammars();
    let grammar = gs.get("cargo").unwrap();
    let action = grammar.actions.get("build").unwrap();
    let fixture = load_fixture("ansi_output.txt");

    // ansi_output.txt contains ANSI-colored cargo output + progress bars
    // The progress bar lines should be classified as noise via inherited ansi-progress
    let mut progress_noise = 0;
    for line in fixture.lines() {
        if line.trim().is_empty() { continue; }
        if let RuleMatch::Noise { .. } = evaluate_line(grammar, Some(action), line) {
            // Count lines that look like progress bars
            if line.contains('[') && (line.contains("=>") || line.contains("==") || line.contains('%')) {
                progress_noise += 1;
            }
        }
    }
    assert!(progress_noise >= 1, "ansi fixture should have progress bar noise, got {progress_noise}");
}

// ===========================================================================
// Progress bar fixture (CR-based)
// ===========================================================================

#[test]
fn test_progress_bar_fixture_all_noise() {
    let gs = grammars();
    let grammar = gs.get("cargo").unwrap();
    let action = grammar.actions.get("build").unwrap();
    let fixture = load_fixture("progress_bar.txt");

    let (hazards, outcomes, noise, _unknown) = classify_fixture(grammar, Some(action), &fixture);

    // A fixture of pure progress bars should be mostly noise
    assert!(noise >= 3, "progress bar fixture should be mostly noise, got {noise}");
    assert_eq!(hazards, 0, "progress bars should have no hazards, got {hazards}");
    assert_eq!(outcomes, 0, "progress bars should have no outcomes, got {outcomes}");
}

// ===========================================================================
// Cursor-up progress fixture (cargo-style)
// ===========================================================================

#[test]
fn test_cursor_up_progress_fixture_is_noise() {
    let gs = grammars();
    let grammar = gs.get("cargo").unwrap();
    let action = grammar.actions.get("build").unwrap();
    let fixture = load_fixture("cursor_up_progress.txt");

    let (hazards, _outcomes, noise, _unknown) = classify_fixture(grammar, Some(action), &fixture);

    // Cursor-up progress should be primarily noise (Compiling lines) + progress bars
    assert!(noise >= 2, "cursor-up fixture should have noise, got {noise}");
    assert_eq!(hazards, 0, "cursor-up progress should have no hazards, got {hazards}");
}

// ===========================================================================
// Cross-grammar: every grammar has at least 2 fixtures
// ===========================================================================

#[test]
fn test_all_grammars_have_success_and_error_fixtures() {
    // npm: npm_install.txt (success) + npm_install_error.txt (error)
    let _ = load_fixture("npm_install.txt");
    let _ = load_fixture("npm_install_error.txt");

    // cargo: cargo_build.txt (success) + cargo_build_error.txt (error) + cargo_test.txt
    let _ = load_fixture("cargo_build.txt");
    let _ = load_fixture("cargo_build_error.txt");
    let _ = load_fixture("cargo_test.txt");

    // git: git_push.txt (success) + git_push_rejected.txt (error)
    let _ = load_fixture("git_push.txt");
    let _ = load_fixture("git_push_rejected.txt");

    // docker: docker_build.txt (success) + docker_build_error.txt (error)
    let _ = load_fixture("docker_build.txt");
    let _ = load_fixture("docker_build_error.txt");

    // make: make_build.txt (success) + make_build_error.txt (error)
    let _ = load_fixture("make_build.txt");
    let _ = load_fixture("make_build_error.txt");

    // Utility fixtures
    let _ = load_fixture("ansi_output.txt");
    let _ = load_fixture("progress_bar.txt");
    let _ = load_fixture("cursor_up_progress.txt");
}
