/// Integration tests for the 5 tool grammars (npm, cargo, git, docker, make),
/// shared grammar inheritance, summary formatting, and load_all_grammars.

use std::collections::HashMap;
use std::path::Path;

use mish::core::grammar::{
    load_all_grammars, load_grammar, detect_tool, resolve_action,
    format_summary, evaluate_line, evaluate_line_with_fallback,
    resolve_category, CapturedOutcome, RuleMatch, Severity,
};
use mish::router::categories::Category;

// ---------------------------------------------------------------------------
// Helper: load grammar from the grammars/ directory by filename
// ---------------------------------------------------------------------------

fn load_tool_grammar(name: &str) -> mish::core::grammar::Grammar {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("grammars")
        .join(format!("{name}.toml"));
    load_grammar(&path).unwrap_or_else(|e| panic!("Failed to load {name}.toml: {e}"))
}

fn load_fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("Failed to read fixture {name}: {e}"))
}

// ===========================================================================
// Tests 1-5: Each grammar parses successfully via load_grammar_from_str
// ===========================================================================

#[test]
fn test_01_npm_grammar_parses() {
    let grammar = load_tool_grammar("npm");
    assert_eq!(grammar.tool.name, "npm");
    assert_eq!(grammar.detect, vec!["npm", "npx"]);
    assert_eq!(grammar.inherit, vec!["ansi-progress", "node-stacktrace"]);
    assert!(grammar.actions.contains_key("install"));
    assert!(grammar.actions.contains_key("test"));
    assert!(grammar.actions.contains_key("run"));
    assert!(grammar.quiet.is_some());
}

#[test]
fn test_02_cargo_grammar_parses() {
    let grammar = load_tool_grammar("cargo");
    assert_eq!(grammar.tool.name, "cargo");
    assert_eq!(grammar.detect, vec!["cargo"]);
    assert_eq!(grammar.inherit, vec!["ansi-progress"]);
    assert!(grammar.actions.contains_key("build"));
    assert!(grammar.actions.contains_key("test"));
    assert!(grammar.quiet.is_some());
}

#[test]
fn test_03_git_grammar_parses() {
    let grammar = load_tool_grammar("git");
    assert_eq!(grammar.tool.name, "git");
    assert_eq!(grammar.detect, vec!["git"]);
    assert_eq!(grammar.inherit, vec!["ansi-progress"]);
    assert!(grammar.actions.contains_key("push"));
    assert!(grammar.actions.contains_key("pull"));
    assert!(grammar.actions.contains_key("clone"));
}

#[test]
fn test_04_docker_grammar_parses() {
    let grammar = load_tool_grammar("docker");
    assert_eq!(grammar.tool.name, "docker");
    assert_eq!(grammar.detect, vec!["docker"]);
    assert_eq!(grammar.inherit, vec!["ansi-progress"]);
    assert!(grammar.actions.contains_key("build"));
    assert!(grammar.actions.contains_key("up"));
    assert!(grammar.quiet.is_some());
}

#[test]
fn test_05_make_grammar_parses() {
    let grammar = load_tool_grammar("make");
    assert_eq!(grammar.tool.name, "make");
    assert_eq!(grammar.detect, vec!["make", "gmake"]);
    assert_eq!(grammar.inherit, vec!["ansi-progress", "c-compiler-output"]);
    assert!(grammar.fallback.is_some());
    assert!(grammar.actions.is_empty());
}

// ===========================================================================
// Tests 6-10: Each grammar categorizes correctly (detect_tool returns correct grammar)
// ===========================================================================

fn build_grammars_map() -> HashMap<String, mish::core::grammar::Grammar> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("grammars");
    load_all_grammars(&dir).unwrap()
}

#[test]
fn test_06_detect_tool_npm() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["npm", "install", "express"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "npm should be detected");
    let (g, a) = result.unwrap();
    assert_eq!(g.tool.name, "npm");
    assert!(a.is_some(), "install action should be resolved");
}

#[test]
fn test_07_detect_tool_cargo() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["cargo", "build"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "cargo should be detected");
    let (g, a) = result.unwrap();
    assert_eq!(g.tool.name, "cargo");
    assert!(a.is_some(), "build action should be resolved");
}

#[test]
fn test_08_detect_tool_git() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["git", "push", "origin", "main"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "git should be detected");
    let (g, a) = result.unwrap();
    assert_eq!(g.tool.name, "git");
    assert!(a.is_some(), "push action should be resolved");
}

#[test]
fn test_09_detect_tool_docker() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["docker", "build", "-t", "myimage", "."]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "docker should be detected");
    let (g, a) = result.unwrap();
    assert_eq!(g.tool.name, "docker");
    assert!(a.is_some(), "build action should be resolved");
}

#[test]
fn test_10_detect_tool_make() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["make", "all"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "make should be detected");
    let (g, a) = result.unwrap();
    assert_eq!(g.tool.name, "make");
    // make has no named actions, so fallback is returned
    assert!(a.is_some(), "fallback action should be returned for make");
}

// ===========================================================================
// Tests 11-15: Each grammar's hazard/outcome/noise rules match test fixture content
// ===========================================================================

#[test]
fn test_11_npm_rules_match_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("npm").unwrap();
    let _fixture = load_fixture("npm_install.txt");
    let install = grammar.actions.get("install").unwrap();

    // Outcome rule should match "added 147 packages in 3.2s"
    let outcome_line = "added 147 packages in 3.2s";
    let matched = install.outcome.iter().any(|r| r.pattern.is_match(outcome_line));
    assert!(matched, "npm install outcome should match: {outcome_line}");

    // Noise rule should match idealTree/reify/resolv lines
    let noise_line = "idealTree: calculating ideal tree";
    let noise_matched = install.noise.iter().any(|r| r.pattern.is_match(noise_line));
    assert!(noise_matched, "npm install noise should match: {noise_line}");

    // Hazard rule should match ERESOLVE
    let hazard_line = "npm ERR! ERESOLVE unable to resolve dependency tree";
    let hazard_matched = install.hazard.iter().any(|r| r.pattern.is_match(hazard_line));
    assert!(hazard_matched, "npm install hazard should match ERESOLVE error");

    // Hazard rule should match permission errors
    let perm_line = "npm ERR! Error: EACCES: permission denied";
    let perm_matched = install.hazard.iter().any(|r| r.pattern.is_match(perm_line));
    assert!(perm_matched, "npm install hazard should match EACCES error");

    // Hazard rule should match vulnerability warnings
    let vuln_line = "6 vulnerabilities (2 moderate, 4 high)";
    let vuln_matched = install.hazard.iter().any(|r| r.pattern.is_match(vuln_line));
    assert!(vuln_matched, "npm install hazard should match vulnerability count");

    // Global noise should match npm timing lines
    let timing_line = "npm timing idealTree:init Completed in 12ms";
    let global_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(timing_line));
    assert!(global_matched, "npm global_noise should match npm timing line");
}

#[test]
fn test_12_cargo_rules_match_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("cargo").unwrap();
    let _fixture = load_fixture("cargo_build.txt");
    let build = grammar.actions.get("build").unwrap();

    // Outcome rule should match "Finished" line
    let outcome_line = "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 12.3s";
    let matched = build.outcome.iter().any(|r| r.pattern.is_match(outcome_line));
    assert!(matched, "cargo build outcome should match Finished line");

    // Hazard: error[E should match
    let error_line = "error[E0433]: failed to resolve: could not find `nonexistent` in `core`";
    let error_matched = build.hazard.iter().any(|r| {
        r.pattern.is_match(error_line) && r.severity == Some(Severity::Error)
    });
    assert!(error_matched, "cargo build hazard should match error[E line");

    // Hazard: warning: should match
    let warning_line = "warning: unused import: `std::collections::HashMap`";
    let warn_matched = build.hazard.iter().any(|r| {
        r.pattern.is_match(warning_line) && r.severity == Some(Severity::Warning)
    });
    assert!(warn_matched, "cargo build hazard should match warning line");

    // Global noise should match Compiling lines
    let compiling_line = "   Compiling serde v1.0.197";
    let global_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(compiling_line));
    assert!(global_matched, "cargo global_noise should match Compiling line");

    // Global noise should match Downloaded lines
    let downloaded_line = "   Downloaded serde v1.0.197";
    let dl_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(downloaded_line));
    assert!(dl_matched, "cargo global_noise should match Downloaded line");

    // Global noise should match Updating lines
    let updating_line = "   Updating crates.io index";
    let up_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(updating_line));
    assert!(up_matched, "cargo global_noise should match Updating line");
}

#[test]
fn test_13_git_rules_match_fixture() {
    let grammar = load_tool_grammar("git");

    // Push outcome should match ref update line (src -> dst pattern)
    let push = grammar.actions.get("push").unwrap();
    let push_line = "   abc1234..def5678 main -> main";
    let push_matched = push.outcome.iter().any(|r| r.pattern.is_match(push_line));
    assert!(push_matched, "git push outcome should match ref update line");

    // Push hazard should match rejected
    let rejected = " ! [rejected]        main -> main (non-fast-forward)";
    let reject_matched = push.hazard.iter().any(|r| r.pattern.is_match(rejected));
    assert!(reject_matched, "git push hazard should match rejected line");

    // Pull outcome should match "Already up to date"
    let pull = grammar.actions.get("pull").unwrap();
    let up_to_date = "Already up to date";
    let pull_matched = pull.outcome.iter().any(|r| r.pattern.is_match(up_to_date));
    assert!(pull_matched, "git pull outcome should match 'Already up to date'");

    // Clone noise should match Cloning into/Receiving/Resolving
    let clone = grammar.actions.get("clone").unwrap();
    let cloning_line = "Cloning into 'my-repo'...";
    let clone_noise = clone.noise.iter().any(|r| r.pattern.is_match(cloning_line));
    assert!(clone_noise, "git clone noise should match 'Cloning into' line");

    // Clone outcome should capture directory name
    let clone_outcome = clone.outcome.iter().any(|r| {
        r.pattern.is_match("Cloning into 'my-repo'") && r.captures.contains(&"dir".to_string())
    });
    assert!(clone_outcome, "git clone outcome should capture dir");
}

#[test]
fn test_14_docker_rules_match_fixture() {
    let grammar = load_tool_grammar("docker");

    // Build noise should match #N CACHED/DONE lines (BuildKit format)
    let build = grammar.actions.get("build").unwrap();
    let cached_line = " #5 CACHED";
    let cached_matched = build.noise.iter().any(|r| r.pattern.is_match(cached_line));
    assert!(cached_matched, "docker build noise should match #N CACHED line");

    let done_line = " #3 DONE 0.1s";
    let done_matched = build.noise.iter().any(|r| r.pattern.is_match(done_line));
    assert!(done_matched, "docker build noise should match #N DONE line");

    // Build noise should match timed step lines
    let timed_line = " #4 0.234 some output";
    let timed_matched = build.noise.iter().any(|r| r.pattern.is_match(timed_line));
    assert!(timed_matched, "docker build noise should match timed step line");

    // Build hazard should match ERROR
    let error_line = "ERROR failed to solve";
    let error_matched = build.hazard.iter().any(|r| r.pattern.is_match(error_line));
    assert!(error_matched, "docker build hazard should match ERROR line");

    // Build outcome should match "exporting to image"
    let export_line = "exporting to image";
    let export_matched = build.outcome.iter().any(|r| r.pattern.is_match(export_line));
    assert!(export_matched, "docker build outcome should match exporting to image");

    // Up outcome should match Container Started
    let up = grammar.actions.get("up").unwrap();
    let started = "Container my-app-db-1  Started";
    let started_matched = up.outcome.iter().any(|r| r.pattern.is_match(started));
    assert!(started_matched, "docker up outcome should match Container Started");
}

#[test]
fn test_15_make_rules_match_fixture() {
    let grammar = load_tool_grammar("make");
    let fallback = grammar.fallback.as_ref().unwrap();

    // Fallback noise should match make[N]: lines
    let make_line = "make[1]: Entering directory '/home/user/project/src'";
    let make_matched = fallback.noise.iter().any(|r| r.pattern.is_match(make_line));
    assert!(make_matched, "make fallback noise should match make[N]: line");

    // Fallback noise should match compiler command echoes
    let gcc_line = "gcc -Wall -O2 -c main.c -o main.o";
    let gcc_matched = fallback.noise.iter().any(|r| r.pattern.is_match(gcc_line));
    assert!(gcc_matched, "make fallback noise should match gcc command echo");

    let clang_line = "clang -std=c11 -o output main.c";
    let clang_matched = fallback.noise.iter().any(|r| r.pattern.is_match(clang_line));
    assert!(clang_matched, "make fallback noise should match clang command echo");
}

// ===========================================================================
// Tests 16-17: Shared grammar inheritance works
// ===========================================================================

#[test]
fn test_16_npm_inherits_ansi_progress_rules() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("npm").unwrap();

    // After inheritance resolution, global_noise should contain ansi-progress rules
    // The ansi-progress shared grammar has a counter-style progress pattern (e.g., "14/57 ")
    let progress_line = "  32/57 packages resolved";
    let matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(progress_line));
    assert!(
        matched,
        "npm should inherit ansi-progress rules matching counter-style progress"
    );

    // Should also inherit node-stacktrace rules
    let stack_frame = "    at Object.<anonymous> (/home/user/app.js:10:15)";
    let stack_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(stack_frame));
    assert!(
        stack_matched,
        "npm should inherit node-stacktrace rules matching stack frames"
    );
}

#[test]
fn test_17_make_inherits_c_compiler_output_rules() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("make").unwrap();

    // After inheritance resolution, global_noise should contain c-compiler-output rules
    // c-compiler-output strips compiler command echoes
    let gcc_cmd = "gcc -Wall -O2 -c file.c -o file.o";
    let matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(gcc_cmd));
    assert!(
        matched,
        "make should inherit c-compiler-output rules matching gcc command echo"
    );

    let clang_cmd = "clang -std=c11 -o output main.c";
    let clang_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(clang_cmd));
    assert!(
        clang_matched,
        "make should inherit c-compiler-output rules matching clang command echo"
    );

    // Also check ansi-progress inheritance
    let progress_line = "  32/57 files compiled";
    let progress_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(progress_line));
    assert!(
        progress_matched,
        "make should inherit ansi-progress rules matching counter-style progress"
    );
}

// ===========================================================================
// Tests 18-19: Summary templates produce expected output with format_summary
// ===========================================================================

#[test]
fn test_18_npm_install_summary_success() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("npm").unwrap();
    let action = grammar.actions.get("install").unwrap();

    let outcomes = vec![CapturedOutcome {
        captures: HashMap::from([
            ("count".to_string(), "147".to_string()),
            ("time".to_string(), "3.2s".to_string()),
        ]),
    }];

    let result = format_summary(grammar, Some(action), &outcomes, 0);
    assert_eq!(result, vec!["+ 147 packages installed (3.2s)"]);
}

#[test]
fn test_19_cargo_build_summary_failure() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("cargo").unwrap();
    let action = grammar.actions.get("build").unwrap();

    let result = format_summary(grammar, Some(action), &[], 1);
    assert_eq!(result, vec!["! build failed (exit 1)"]);
}

// ===========================================================================
// Test 20: load_all_grammars loads all grammars from the directory
// ===========================================================================

#[test]
fn test_20_load_all_grammars_loads_all() {
    let grammars = build_grammars_map();

    for name in &["npm", "cargo", "git", "docker", "make", "pip", "pytest", "jest", "webpack", "kubectl", "terraform"] {
        assert!(
            grammars.contains_key(*name),
            "load_all_grammars should load {name}"
        );
    }

    // Verify all grammars have had inheritance resolved
    let npm = grammars.get("npm").unwrap();
    // npm inherits ansi-progress (4 rules) + node-stacktrace (1 rule) + own 2 global_noise
    assert!(
        npm.global_noise.len() >= 4,
        "npm global_noise should include inherited + own rules, got {}",
        npm.global_noise.len()
    );
}

// ===========================================================================
// Test 21: Fallback action works for make
// ===========================================================================

#[test]
fn test_21_make_fallback_action() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("make").unwrap();

    // When no subcommand matches, fallback should be used
    let args: Vec<String> = vec!["make", "clean"]
        .into_iter()
        .map(String::from)
        .collect();
    let action = resolve_action(grammar, &args);
    assert!(action.is_some(), "make should have a fallback action");
    let fb = action.unwrap();
    assert_eq!(fb.summary.success, "+ make complete");
    assert_eq!(fb.summary.failure, "! make failed (exit {exit_code})");
    assert_eq!(fb.summary.partial, "... building ({lines} lines)");

    // format_summary should work with fallback
    let result = format_summary(grammar, action, &[], 0);
    assert_eq!(result, vec!["+ make complete"]);

    // format_summary failure path
    let result_fail = format_summary(grammar, action, &[], 2);
    assert_eq!(result_fail, vec!["! make failed (exit 2)"]);
}

// ===========================================================================
// Bonus tests: additional coverage
// ===========================================================================

#[test]
fn test_npx_detected_as_npm() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["npx", "create-react-app", "my-app"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "npx should be detected as npm grammar");
    assert_eq!(result.unwrap().0.tool.name, "npm");
}

#[test]
fn test_gmake_detected_as_make() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["gmake", "all"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "gmake should be detected as make grammar");
    assert_eq!(result.unwrap().0.tool.name, "make");
}

#[test]
fn test_cargo_test_action_resolves() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("cargo").unwrap();
    let args: Vec<String> = vec!["cargo", "t"]
        .into_iter()
        .map(String::from)
        .collect();
    let action = resolve_action(grammar, &args);
    assert!(action.is_some(), "cargo t should resolve to test action");
    let test_action = action.unwrap();
    assert!(
        test_action.detect.contains(&"t".to_string()),
        "test action detect should include 't'"
    );
}

#[test]
fn test_npm_quiet_config() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("npm").unwrap();
    let quiet = grammar.quiet.as_ref().unwrap();
    assert!(
        quiet.safe_inject.contains(&"--loglevel=error".to_string()),
        "npm quiet should include --loglevel=error"
    );
    assert!(
        quiet.recommend.contains(&"--silent".to_string()),
        "npm quiet should recommend --silent"
    );
    let install_quiet = quiet.actions.get("install").unwrap();
    assert!(
        install_quiet.safe_inject.contains(&"--no-fund".to_string()),
        "npm quiet install should include --no-fund"
    );
    assert!(
        install_quiet.safe_inject.contains(&"--no-audit".to_string()),
        "npm quiet install should include --no-audit"
    );
}

#[test]
fn test_git_push_summary_success() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("git").unwrap();
    let action = grammar.actions.get("push").unwrap();

    let outcomes = vec![CapturedOutcome {
        captures: HashMap::from([
            ("src".to_string(), "main".to_string()),
            ("dst".to_string(), "main".to_string()),
        ]),
    }];

    let result = format_summary(grammar, Some(action), &outcomes, 0);
    assert_eq!(result, vec!["+ pushed main -> main"]);
}

#[test]
fn test_docker_quiet_config() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("docker").unwrap();
    let quiet = grammar.quiet.as_ref().unwrap();
    assert!(
        quiet.safe_inject.contains(&"--progress=plain".to_string()),
        "docker quiet should include --progress=plain"
    );
    assert!(
        quiet.recommend.contains(&"-q".to_string()),
        "docker quiet should recommend -q"
    );
}

#[test]
fn test_npm_outcome_captures_from_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("npm").unwrap();
    let install = grammar.actions.get("install").unwrap();

    let fixture = load_fixture("npm_install.txt");
    let mut captured = HashMap::new();

    for line in fixture.lines() {
        for rule in &install.outcome {
            if let Some(caps) = rule.pattern.captures(line) {
                for name in &rule.captures {
                    if let Some(m) = caps.name(name) {
                        captured.insert(name.clone(), m.as_str().to_string());
                    }
                }
            }
        }
    }

    assert_eq!(captured.get("count").map(|s| s.as_str()), Some("147"));
    assert_eq!(captured.get("time").map(|s| s.as_str()), Some("3.2s"));
}

#[test]
fn test_cargo_outcome_captures_from_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("cargo").unwrap();
    let build = grammar.actions.get("build").unwrap();

    let fixture = load_fixture("cargo_build.txt");
    let mut captured = HashMap::new();

    for line in fixture.lines() {
        for rule in &build.outcome {
            if let Some(caps) = rule.pattern.captures(line) {
                for name in &rule.captures {
                    if let Some(m) = caps.name(name) {
                        captured.insert(name.clone(), m.as_str().to_string());
                    }
                }
            }
        }
    }

    assert_eq!(captured.get("time").map(|s| s.as_str()), Some("12.3s"));
}

// ===========================================================================
// Tests: evaluate_line with real grammars
// ===========================================================================

#[test]
fn test_evaluate_line_npm_install_hazard_first() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("npm").unwrap();
    let install = grammar.actions.get("install").unwrap();

    // ERESOLVE should be classified as hazard
    let result = evaluate_line(grammar, Some(install), "npm ERR! ERESOLVE unable to resolve");
    assert!(matches!(result, RuleMatch::Hazard { .. }),
        "ERESOLVE should be Hazard, got {:?}", result);

    // idealTree should be classified as noise
    let result = evaluate_line(grammar, Some(install), "idealTree: calculating");
    assert!(matches!(result, RuleMatch::Noise { .. }),
        "idealTree should be Noise, got {:?}", result);

    // "added N packages" should be outcome
    let result = evaluate_line(grammar, Some(install), "added 147 packages in 3.2s");
    match &result {
        RuleMatch::Outcome { captures, .. } => {
            assert_eq!(captures.get("count").map(|s| s.as_str()), Some("147"));
            assert_eq!(captures.get("time").map(|s| s.as_str()), Some("3.2s"));
        }
        other => panic!("expected Outcome, got {:?}", other),
    }
}

#[test]
fn test_evaluate_line_cargo_build_evaluation_order() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("cargo").unwrap();
    let build = grammar.actions.get("build").unwrap();

    // Hazard: error[E0433] should match hazard before anything else
    let result = evaluate_line(grammar, Some(build), "error[E0433]: failed to resolve");
    match &result {
        RuleMatch::Hazard { severity, captures, .. } => {
            assert_eq!(*severity, Some(Severity::Error));
            assert_eq!(captures.get("code").map(|s| s.as_str()), Some("E0433"));
        }
        other => panic!("expected Hazard for error line, got {:?}", other),
    }

    // Outcome: Finished line
    let result = evaluate_line(grammar, Some(build), "   Finished `dev` profile in 5.2s");
    match &result {
        RuleMatch::Outcome { captures, .. } => {
            assert_eq!(captures.get("time").map(|s| s.as_str()), Some("5.2s"));
        }
        other => panic!("expected Outcome for Finished line, got {:?}", other),
    }

    // Global noise: Compiling (dedup from global_noise)
    let result = evaluate_line(grammar, Some(build), "   Compiling serde v1.0.0");
    assert!(matches!(result, RuleMatch::Noise { .. }),
        "Compiling should be Noise, got {:?}", result);
}

#[test]
fn test_evaluate_line_make_fallback() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("make").unwrap();

    // make has no named actions, uses fallback
    let result = evaluate_line_with_fallback(grammar, "make[1]: *** [Makefile:10: all] Error 2");
    assert!(matches!(result, RuleMatch::Hazard { .. }),
        "make *** should be Hazard, got {:?}", result);

    // Noise via fallback: make[N]: Entering directory
    let result = evaluate_line_with_fallback(grammar, "make[1]: Entering directory '/tmp/foo'");
    assert!(matches!(result, RuleMatch::Noise { .. }),
        "make entering dir should be Noise, got {:?}", result);

    // "Nothing to be done" should be outcome
    let result = evaluate_line_with_fallback(grammar, "make[1]: Nothing to be done for 'all'");
    assert!(matches!(result, RuleMatch::Outcome { .. }),
        "Nothing to be done should be Outcome, got {:?}", result);
}

// ===========================================================================
// Tests: inheritance order (own rules evaluated before inherited)
// ===========================================================================

#[test]
fn test_inheritance_order_npm_own_before_inherited() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("npm").unwrap();

    // npm has 2 own global_noise rules, then inherited ansi-progress + node-stacktrace rules
    // Verify own rules come first by checking the first 2 rules
    assert!(grammar.global_noise[0].pattern.is_match("npm timing foo"),
        "first global_noise should be npm's own (npm timing)");
    assert!(grammar.global_noise[1].pattern.is_match("npm warn something"),
        "second global_noise should be npm's own (npm warn)");

    // Verify inherited rules come after by checking that ansi-progress patterns
    // are found at indices >= 2
    let counter_progress = "  32/57 done";
    let found_at = grammar.global_noise.iter().position(|r| r.pattern.is_match(counter_progress));
    assert!(found_at.is_some(), "inherited ansi-progress counter rule should exist");
    assert!(found_at.unwrap() >= 2, "inherited rules should come after own rules");
}

#[test]
fn test_inheritance_order_make_own_before_inherited() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("make").unwrap();

    // make's own global_noise is empty, so all global_noise should be inherited
    // (from ansi-progress and c-compiler-output)
    assert!(!grammar.global_noise.is_empty(),
        "make should have inherited global_noise rules");

    // Check that inherited c-compiler-output rule matches gcc commands
    let gcc_match = grammar.global_noise.iter()
        .any(|r| r.pattern.is_match("gcc -Wall main.c -o main"));
    assert!(gcc_match, "make should inherit gcc command echo from c-compiler-output");
}

// ===========================================================================
// Tests: category resolution
// ===========================================================================

#[test]
fn test_resolve_category_npm_defaults_to_condense() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("npm").unwrap();

    // npm grammar has no category field, should default to Condense
    let result = resolve_category(grammar, None);
    assert_eq!(result, Category::Condense,
        "npm with no category field should default to Condense");
}

#[test]
fn test_resolve_category_with_categories_map() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("npm").unwrap();

    // Provide a categories map that maps npm -> Narrate
    let mut map = HashMap::new();
    map.insert("npm".to_string(), Category::Narrate);

    let result = resolve_category(grammar, Some(&map));
    assert_eq!(result, Category::Narrate,
        "npm with categories map entry should resolve to Narrate");
}

// ===========================================================================
// Tests: git push/pull/clone outcome pattern matching
// ===========================================================================

#[test]
fn test_git_push_outcome_captures_src_dst() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("git").unwrap();
    let push = grammar.actions.get("push").unwrap();

    let line = "   abc1234..def5678  main -> origin/main";
    let result = evaluate_line(grammar, Some(push), line);
    match &result {
        RuleMatch::Outcome { captures, .. } => {
            assert!(captures.contains_key("src"), "should capture src");
            assert!(captures.contains_key("dst"), "should capture dst");
        }
        other => panic!("expected Outcome, got {:?}", other),
    }
}

#[test]
fn test_git_pull_conflict_is_hazard() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("git").unwrap();
    let pull = grammar.actions.get("pull").unwrap();

    let line = "CONFLICT (content): Merge conflict in src/main.rs";
    let result = evaluate_line(grammar, Some(pull), line);
    assert!(matches!(result, RuleMatch::Hazard { .. }),
        "CONFLICT should be Hazard, got {:?}", result);
}

#[test]
fn test_docker_up_hazard_error() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("docker").unwrap();
    let up = grammar.actions.get("up").unwrap();

    let line = "my-app exited with code 1";
    let result = evaluate_line(grammar, Some(up), line);
    assert!(matches!(result, RuleMatch::Hazard { .. }),
        "exited with code 1 should be Hazard, got {:?}", result);
}

// ===========================================================================
// pip + pytest grammar tests (e1)
// ===========================================================================

#[test]
fn test_pip_grammar_parses() {
    let grammar = load_tool_grammar("pip");
    assert_eq!(grammar.tool.name, "pip");
    assert_eq!(grammar.detect, vec!["pip", "pip3"]);
    assert_eq!(grammar.inherit, vec!["ansi-progress", "python-traceback"]);
    assert!(grammar.actions.contains_key("install"));
    assert!(grammar.actions.contains_key("uninstall"));
    assert!(grammar.quiet.is_some());
}

#[test]
fn test_pytest_grammar_parses() {
    let grammar = load_tool_grammar("pytest");
    assert_eq!(grammar.tool.name, "pytest");
    assert_eq!(grammar.detect, vec!["pytest", "py.test"]);
    assert_eq!(grammar.inherit, vec!["python-traceback"]);
    assert!(grammar.fallback.is_some());
}

#[test]
fn test_detect_tool_pip() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["pip", "install", "flask"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "pip should be detected");
    let (g, a) = result.unwrap();
    assert_eq!(g.tool.name, "pip");
    assert!(a.is_some(), "install action should be resolved");
}

#[test]
fn test_detect_tool_pip3() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["pip3", "install", "requests"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "pip3 should be detected as pip");
    assert_eq!(result.unwrap().0.tool.name, "pip");
}

#[test]
fn test_detect_tool_pytest() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["pytest", "-v"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "pytest should be detected");
    let (g, a) = result.unwrap();
    assert_eq!(g.tool.name, "pytest");
    assert!(a.is_some(), "fallback action should be resolved for pytest");
}

#[test]
fn test_pip_rules_match_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("pip").unwrap();
    let install = grammar.actions.get("install").unwrap();

    // Outcome: Successfully installed line
    let outcome_line = "Successfully installed Jinja2-3.1.3 MarkupSafe-2.1.5 flask-3.0.2";
    let matched = install.outcome.iter().any(|r| r.pattern.is_match(outcome_line));
    assert!(matched, "pip install outcome should match 'Successfully installed'");

    // Noise: Collecting lines
    let collect_line = "Collecting flask>=2.0";
    let noise_matched = install.noise.iter().any(|r| r.pattern.is_match(collect_line));
    assert!(noise_matched, "pip install noise should match 'Collecting'");

    // Noise: Requirement already satisfied
    let req_line = "Requirement already satisfied: setuptools in /usr/lib/python3/dist-packages";
    let req_matched = install.noise.iter().any(|r| r.pattern.is_match(req_line));
    assert!(req_matched, "pip install noise should match 'Requirement already satisfied'");

    // Hazard: ERROR
    let error_line = "ERROR: Could not install packages due to an OSError";
    let error_matched = install.hazard.iter().any(|r| r.pattern.is_match(error_line));
    assert!(error_matched, "pip install hazard should match 'ERROR:'");

    // Hazard: Could not find
    let find_line = "Could not find a version that satisfies the requirement";
    let find_matched = install.hazard.iter().any(|r| r.pattern.is_match(find_line));
    assert!(find_matched, "pip install hazard should match 'Could not find'");

    // Global noise: Downloading lines
    let dl_line = "  Downloading flask-3.0.2-py3-none-any.whl (101 kB)";
    let dl_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(dl_line));
    assert!(dl_matched, "pip global_noise should match 'Downloading' line");
}

#[test]
fn test_pytest_rules_match_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("pytest").unwrap();
    let fallback = grammar.fallback.as_ref().unwrap();

    // Noise: passing test lines
    let pass_line = "tests/test_auth.py::test_login_success PASSED";
    let pass_matched = fallback.noise.iter().any(|r| r.pattern.is_match(pass_line));
    assert!(pass_matched, "pytest noise should match PASSED test line");

    // Hazard: FAILED test line
    let fail_line = "tests/test_api.py::test_delete_user FAILED";
    let fail_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(fail_line));
    assert!(fail_matched, "pytest hazard should match FAILED test line");

    // Hazard: FAILED in short summary
    let summary_fail = "FAILED tests/test_api.py::test_delete_user - AssertionError";
    let sf_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(summary_fail));
    assert!(sf_matched, "pytest hazard should match FAILED summary line");

    // Outcome: combined result with failed/passed/time
    let result_line = "2 failed, 5 passed in 1.23s";
    let outcome_matched = fallback.outcome.iter().any(|r| r.pattern.is_match(result_line));
    assert!(outcome_matched, "pytest outcome should match combined result line");

    // Global noise: separator lines
    let sep_line = "============================== test session starts ==============================";
    let sep_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(sep_line));
    assert!(sep_matched, "pytest global_noise should match separator line with text");

    // Global noise: platform info
    let platform_line = "platform linux -- Python 3.12.1, pytest-8.0.2, pluggy-1.4.0";
    let plat_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(platform_line));
    assert!(plat_matched, "pytest global_noise should match platform info line");

    // Global noise: collected items
    let collected_line = "collected 12 items";
    let coll_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(collected_line));
    assert!(coll_matched, "pytest global_noise should match 'collected N items'");
}

#[test]
fn test_pip_outcome_captures_from_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("pip").unwrap();
    let install = grammar.actions.get("install").unwrap();

    let fixture = load_fixture("pip_install.txt");
    let mut captured = HashMap::new();

    for line in fixture.lines() {
        for rule in &install.outcome {
            if let Some(caps) = rule.pattern.captures(line) {
                for name in &rule.captures {
                    if let Some(m) = caps.name(name) {
                        captured.insert(name.clone(), m.as_str().to_string());
                    }
                }
            }
        }
    }

    assert!(captured.contains_key("packages"),
        "pip install fixture should capture packages");
    let packages = captured.get("packages").unwrap();
    assert!(packages.contains("flask"),
        "captured packages should contain flask, got: {}", packages);
}

#[test]
fn test_pip_inherits_python_traceback() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("pip").unwrap();

    // Inherited python-traceback: File "..." line N
    let frame_line = "  File \"/usr/lib/python3/site.py\", line 42";
    let matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(frame_line));
    assert!(matched,
        "pip should inherit python-traceback matching File frame");
}

#[test]
fn test_pytest_summary_format() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("pytest").unwrap();
    let action = grammar.fallback.as_ref().unwrap();

    let outcomes = vec![CapturedOutcome {
        captures: HashMap::from([
            ("failed".to_string(), "2".to_string()),
            ("passed".to_string(), "5".to_string()),
            ("time".to_string(), "1.23s".to_string()),
        ]),
    }];

    let result = format_summary(grammar, Some(action), &outcomes, 1);
    assert_eq!(result, vec!["! 2 failed, 5 passed (1.23s)"]);
}

// ===========================================================================
// jest + webpack grammar tests (e2)
// ===========================================================================

#[test]
fn test_jest_grammar_parses() {
    let grammar = load_tool_grammar("jest");
    assert_eq!(grammar.tool.name, "jest");
    assert!(grammar.detect.contains(&"jest".to_string()));
    assert_eq!(grammar.inherit, vec!["ansi-progress", "node-stacktrace"]);
    assert!(grammar.fallback.is_some());
}

#[test]
fn test_webpack_grammar_parses() {
    let grammar = load_tool_grammar("webpack");
    assert_eq!(grammar.tool.name, "webpack");
    assert!(grammar.detect.contains(&"webpack".to_string()));
    assert_eq!(grammar.inherit, vec!["ansi-progress"]);
    assert!(grammar.fallback.is_some());
    assert!(grammar.quiet.is_some());
}

#[test]
fn test_detect_tool_jest() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["jest", "--verbose"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "jest should be detected");
    let (g, a) = result.unwrap();
    assert_eq!(g.tool.name, "jest");
    assert!(a.is_some(), "fallback action should be resolved");
}

#[test]
fn test_detect_tool_webpack() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["webpack", "--mode", "production"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "webpack should be detected");
    let (g, a) = result.unwrap();
    assert_eq!(g.tool.name, "webpack");
    assert!(a.is_some(), "fallback action should be resolved");
}

#[test]
fn test_jest_rules_match_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("jest").unwrap();
    let fallback = grammar.fallback.as_ref().unwrap();

    // Noise: passing test checkmark
    let pass_line = "  ✓ formats date correctly (3 ms)";
    let pass_matched = fallback.noise.iter().any(|r| r.pattern.is_match(pass_line));
    assert!(pass_matched, "jest noise should match checkmark test line");

    // Noise: PASS suite
    let pass_suite = " PASS  src/__tests__/utils.test.ts";
    let ps_matched = fallback.noise.iter().any(|r| r.pattern.is_match(pass_suite));
    assert!(ps_matched, "jest noise should match PASS suite line");

    // Hazard: FAIL suite
    let fail_suite = " FAIL  src/__tests__/api.test.ts";
    let fs_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(fail_suite));
    assert!(fs_matched, "jest hazard should match FAIL suite line");

    // Hazard: bullet marker
    let bullet = "  ● UserService › should create a user";
    let bullet_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(bullet));
    assert!(bullet_matched, "jest hazard should match ● test name line");

    // Outcome: Tests line with failures
    let tests_line = "Tests:       1 failed, 4 passed, 5 total";
    let tests_matched = fallback.outcome.iter().any(|r| r.pattern.is_match(tests_line));
    assert!(tests_matched, "jest outcome should match Tests summary line");

    // Global noise: Snapshots
    let snap = "Snapshots:   0 total";
    let snap_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(snap));
    assert!(snap_matched, "jest global_noise should match Snapshots line");

    // Global noise: Ran all test suites
    let ran = "Ran all test suites.";
    let ran_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(ran));
    assert!(ran_matched, "jest global_noise should match 'Ran all test suites'");
}

#[test]
fn test_webpack_rules_match_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("webpack").unwrap();
    let fallback = grammar.fallback.as_ref().unwrap();

    // Noise: asset line
    let asset_line = "asset main.js 1.24 MiB [emitted] (name: main)";
    let asset_matched = fallback.noise.iter().any(|r| r.pattern.is_match(asset_line));
    assert!(asset_matched, "webpack noise should match asset line");

    // Hazard: ERROR in
    let error_line = "ERROR in ./src/index.js";
    let error_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(error_line));
    assert!(error_matched, "webpack hazard should match ERROR in");

    // Hazard: Module not found
    let module_line = "Module not found: Error: Can't resolve './missing'";
    let module_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(module_line));
    assert!(module_matched, "webpack hazard should match Module not found");

    // Hazard: WARNING in
    let warn_line = "WARNING in ./src/legacy.js";
    let warn_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(warn_line));
    assert!(warn_matched, "webpack hazard should match WARNING in");

    // Outcome: compiled successfully
    let success_line = "webpack 5.90.1 compiled successfully in 4523 ms";
    let success_matched = fallback.outcome.iter().any(|r| r.pattern.is_match(success_line));
    assert!(success_matched, "webpack outcome should match compiled successfully");

    // Global noise: module count lines
    let modules_line = "  runtime modules 1.02 KiB 5 modules";
    let mod_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(modules_line));
    assert!(mod_matched, "webpack global_noise should match module count line");
}

#[test]
fn test_jest_outcome_captures_from_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("jest").unwrap();
    let fallback = grammar.fallback.as_ref().unwrap();

    let fixture = load_fixture("jest_test.txt");
    let mut captured = HashMap::new();

    for line in fixture.lines() {
        for rule in &fallback.outcome {
            if let Some(caps) = rule.pattern.captures(line) {
                for name in &rule.captures {
                    if let Some(m) = caps.name(name) {
                        captured.insert(name.clone(), m.as_str().to_string());
                    }
                }
            }
        }
    }

    assert_eq!(captured.get("failed").map(|s| s.as_str()), Some("1"));
    assert_eq!(captured.get("passed").map(|s| s.as_str()), Some("4"));
}

#[test]
fn test_webpack_outcome_captures_from_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("webpack").unwrap();
    let fallback = grammar.fallback.as_ref().unwrap();

    let fixture = load_fixture("webpack_build.txt");
    let mut captured = HashMap::new();

    for line in fixture.lines() {
        for rule in &fallback.outcome {
            if let Some(caps) = rule.pattern.captures(line) {
                for name in &rule.captures {
                    if let Some(m) = caps.name(name) {
                        captured.insert(name.clone(), m.as_str().to_string());
                    }
                }
            }
        }
    }

    assert_eq!(captured.get("status").map(|s| s.as_str()), Some("successfully"));
    assert_eq!(captured.get("time").map(|s| s.as_str()), Some("4523 ms"));
}

// ===========================================================================
// kubectl + terraform grammar tests (e3)
// ===========================================================================

#[test]
fn test_kubectl_grammar_parses() {
    let grammar = load_tool_grammar("kubectl");
    assert_eq!(grammar.tool.name, "kubectl");
    assert!(grammar.detect.contains(&"kubectl".to_string()));
    assert!(grammar.actions.contains_key("apply"));
    assert!(grammar.actions.contains_key("get"));
    assert!(grammar.actions.contains_key("delete"));
}

#[test]
fn test_terraform_grammar_parses() {
    let grammar = load_tool_grammar("terraform");
    assert_eq!(grammar.tool.name, "terraform");
    assert!(grammar.detect.contains(&"terraform".to_string()));
    assert!(grammar.actions.contains_key("plan"));
    assert!(grammar.actions.contains_key("apply"));
    assert!(grammar.actions.contains_key("init"));
}

#[test]
fn test_detect_tool_kubectl() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["kubectl", "apply", "-f", "deploy.yaml"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "kubectl should be detected");
    let (g, a) = result.unwrap();
    assert_eq!(g.tool.name, "kubectl");
    assert!(a.is_some(), "apply action should be resolved");
}

#[test]
fn test_detect_tool_terraform() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["terraform", "apply"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "terraform should be detected");
    let (g, a) = result.unwrap();
    assert_eq!(g.tool.name, "terraform");
    assert!(a.is_some(), "apply action should be resolved");
}

#[test]
fn test_kubectl_rules_match_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("kubectl").unwrap();
    let apply = grammar.actions.get("apply").unwrap();

    // Noise: unchanged
    let unchanged = "service/web-server unchanged";
    let un_matched = apply.noise.iter().any(|r| r.pattern.is_match(unchanged));
    assert!(un_matched, "kubectl apply noise should match unchanged");

    // Noise: configured (dedup)
    let configured = "deployment.apps/web-server configured";
    let conf_matched = apply.noise.iter().any(|r| r.pattern.is_match(configured));
    assert!(conf_matched, "kubectl apply noise should match configured");

    // Outcome: created
    let created = "configmap/app-config created";
    let cr_matched = apply.outcome.iter().any(|r| r.pattern.is_match(created));
    assert!(cr_matched, "kubectl apply outcome should match created");

    // Hazard: error
    let error_line = "error: unable to recognize \"deploy.yaml\"";
    let err_matched = apply.hazard.iter().any(|r| r.pattern.is_match(error_line));
    assert!(err_matched, "kubectl apply hazard should match error:");
}

#[test]
fn test_terraform_rules_match_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("terraform").unwrap();
    let apply = grammar.actions.get("apply").unwrap();

    // Noise: Still creating
    let still = "aws_instance.web: Still creating... [10s elapsed]";
    let still_matched = apply.noise.iter().any(|r| r.pattern.is_match(still));
    assert!(still_matched, "terraform apply noise should match 'Still creating'");

    // Hazard: Error:
    let error_line = "Error: creating EC2 Instance: operation error EC2";
    let err_matched = apply.hazard.iter().any(|r| r.pattern.is_match(error_line));
    assert!(err_matched, "terraform apply hazard should match 'Error:'");

    // Outcome: Apply complete
    let complete = "Apply complete! Resources: 1 added, 1 changed, 0 destroyed.";
    let comp_matched = apply.outcome.iter().any(|r| r.pattern.is_match(complete));
    assert!(comp_matched, "terraform apply outcome should match 'Apply complete!'");

    // Outcome captures
    for rule in &apply.outcome {
        if let Some(caps) = rule.pattern.captures(complete) {
            if let Some(m) = caps.name("added") {
                assert_eq!(m.as_str(), "1");
            }
        }
    }
}

#[test]
fn test_terraform_plan_outcome_captures() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("terraform").unwrap();
    let plan = grammar.actions.get("plan").unwrap();

    let plan_line = "Plan: 1 to add, 1 to change, 0 to destroy.";
    let mut captured = HashMap::new();
    for rule in &plan.outcome {
        if let Some(caps) = rule.pattern.captures(plan_line) {
            for name in &rule.captures {
                if let Some(m) = caps.name(name) {
                    captured.insert(name.clone(), m.as_str().to_string());
                }
            }
        }
    }
    assert_eq!(captured.get("add").map(|s| s.as_str()), Some("1"));
    assert_eq!(captured.get("change").map(|s| s.as_str()), Some("1"));
    assert_eq!(captured.get("destroy").map(|s| s.as_str()), Some("0"));
}

#[test]
fn test_terraform_apply_summary_format() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("terraform").unwrap();
    let action = grammar.actions.get("apply").unwrap();

    let outcomes = vec![CapturedOutcome {
        captures: HashMap::from([
            ("added".to_string(), "1".to_string()),
            ("changed".to_string(), "1".to_string()),
            ("destroyed".to_string(), "0".to_string()),
        ]),
    }];

    let result = format_summary(grammar, Some(action), &outcomes, 0);
    assert_eq!(result, vec!["+ applied: 1 added, 1 changed, 0 destroyed"]);
}

// ===========================================================================
// gcc + go + rustc grammar tests (e4)
// ===========================================================================

#[test]
fn test_gcc_grammar_parses() {
    let grammar = load_tool_grammar("gcc");
    assert_eq!(grammar.tool.name, "gcc");
    assert!(grammar.detect.contains(&"gcc".to_string()));
    assert!(grammar.detect.contains(&"g++".to_string()));
    assert!(grammar.detect.contains(&"cc".to_string()));
    assert!(grammar.detect.contains(&"c++".to_string()));
    assert_eq!(grammar.inherit, vec!["ansi-progress"]);
    assert!(grammar.actions.contains_key("compile"));
    assert!(grammar.quiet.is_some());
}

#[test]
fn test_detect_tool_gcc() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["gcc", "-o", "main", "main.c"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "gcc should be detected");
    let (g, a) = result.unwrap();
    assert_eq!(g.tool.name, "gcc");
    assert!(a.is_some(), "compile action should be resolved");
}

#[test]
fn test_detect_tool_gpp() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["g++", "-c", "main.cpp"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "g++ should be detected as gcc");
    assert_eq!(result.unwrap().0.tool.name, "gcc");
}

#[test]
fn test_gcc_rules_match_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("gcc").unwrap();
    let compile = grammar.actions.get("compile").unwrap();

    // Hazard: warning line
    let warn_line = "main.c:15:5: warning: implicit declaration of function 'gets' [-Wimplicit-function-declaration]";
    let warn_matched = compile.hazard.iter().any(|r| r.pattern.is_match(warn_line));
    assert!(warn_matched, "gcc compile hazard should match 'warning:'");

    // Global noise: "In file included from"
    let incl_line = "In file included from /usr/include/stdio.h:27,";
    let incl_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(incl_line));
    assert!(incl_matched, "gcc global_noise should match 'In file included from'");

    // Global noise: note line
    let note_line = "/usr/include/features.h:461:12: note: expanded from macro '__GLIBC_USE'";
    let note_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(note_line));
    assert!(note_matched, "gcc global_noise should match note lines");

    // Hazard: error line
    let error_line = "main.c:10:5: error: use of undeclared identifier 'foo'";
    let err_matched = compile.hazard.iter().any(|r| r.pattern.is_match(error_line));
    assert!(err_matched, "gcc compile hazard should match 'error:'");

    // Hazard: undefined reference
    let undef_line = "main.c:(.text+0x1a): undefined reference to `bar'";
    let undef_matched = compile.hazard.iter().any(|r| r.pattern.is_match(undef_line));
    assert!(undef_matched, "gcc compile hazard should match 'undefined reference'");

    // Hazard: fatal error
    let fatal_line = "main.c:3:10: fatal error: 'missing.h' file not found";
    let fatal_matched = compile.hazard.iter().any(|r| r.pattern.is_match(fatal_line));
    assert!(fatal_matched, "gcc compile hazard should match 'fatal error:'");
}

#[test]
fn test_gcc_error_fixture_hazards() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("gcc").unwrap();
    let compile = grammar.actions.get("compile").unwrap();

    let fixture = load_fixture("gcc_build_error.txt");
    let mut error_count = 0;
    for line in fixture.lines() {
        for rule in &compile.hazard {
            if rule.pattern.is_match(line) {
                if rule.severity == Some(Severity::Error) {
                    error_count += 1;
                }
                break;
            }
        }
    }
    assert!(error_count >= 3, "gcc_build_error.txt should have at least 3 error-severity hazards, found {error_count}");
}

#[test]
fn test_go_grammar_parses() {
    let grammar = load_tool_grammar("go");
    assert_eq!(grammar.tool.name, "go");
    assert!(grammar.detect.contains(&"go".to_string()));
    assert_eq!(grammar.inherit, vec!["ansi-progress"]);
    assert!(grammar.actions.contains_key("build"));
    assert!(grammar.actions.contains_key("test"));
    assert!(grammar.actions.contains_key("run"));
    assert!(grammar.actions.contains_key("mod"));
}

#[test]
fn test_detect_tool_go() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["go", "test", "./..."]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "go should be detected");
    let (g, a) = result.unwrap();
    assert_eq!(g.tool.name, "go");
    assert!(a.is_some(), "test action should be resolved");
}

#[test]
fn test_go_rules_match_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("go").unwrap();
    let test_action = grammar.actions.get("test").unwrap();

    // Noise: === RUN
    let run_line = "=== RUN   TestAdd";
    let run_matched = test_action.noise.iter().any(|r| r.pattern.is_match(run_line));
    assert!(run_matched, "go test noise should match '=== RUN'");

    // Noise: --- PASS
    let pass_line = "--- PASS: TestAdd (0.00s)";
    let pass_matched = test_action.noise.iter().any(|r| r.pattern.is_match(pass_line));
    assert!(pass_matched, "go test noise should match '--- PASS:'");

    // Hazard: --- FAIL
    let fail_line = "--- FAIL: TestDivide (0.00s)";
    let fail_matched = test_action.hazard.iter().any(|r| r.pattern.is_match(fail_line));
    assert!(fail_matched, "go test hazard should match '--- FAIL:'");

    // Outcome: ok line
    let ok_line = "ok\tgithub.com/user/mathlib\t0.012s";
    let ok_matched = test_action.outcome.iter().any(|r| r.pattern.is_match(ok_line));
    assert!(ok_matched, "go test outcome should match 'ok\\t...'");

    // Outcome: FAIL line
    let fail_pkg = "FAIL\tgithub.com/user/mathlib\t0.012s";
    let fail_pkg_matched = test_action.outcome.iter().any(|r| r.pattern.is_match(fail_pkg));
    assert!(fail_pkg_matched, "go test outcome should match 'FAIL\\t...'");

    // Global noise: go: downloading
    let dl_line = "go: downloading github.com/gin-gonic/gin v1.9.1";
    let dl_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(dl_line));
    assert!(dl_matched, "go global_noise should match 'go: downloading'");
}

#[test]
fn test_go_test_outcome_captures() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("go").unwrap();
    let test_action = grammar.actions.get("test").unwrap();

    let ok_line = "ok\tgithub.com/user/mathlib\t0.012s";
    let mut captured = HashMap::new();
    for rule in &test_action.outcome {
        if let Some(caps) = rule.pattern.captures(ok_line) {
            for name in &rule.captures {
                if let Some(m) = caps.name(name) {
                    captured.insert(name.clone(), m.as_str().to_string());
                }
            }
        }
    }
    assert_eq!(captured.get("pkg").map(|s| s.as_str()), Some("github.com/user/mathlib"));
    assert_eq!(captured.get("time").map(|s| s.as_str()), Some("0.012s"));
}

#[test]
fn test_rustc_grammar_parses() {
    let grammar = load_tool_grammar("rustc");
    assert_eq!(grammar.tool.name, "rustc");
    assert!(grammar.detect.contains(&"rustc".to_string()));
    assert_eq!(grammar.inherit, vec!["ansi-progress"]);
    assert!(grammar.fallback.is_some());
}

#[test]
fn test_detect_tool_rustc() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["rustc", "main.rs"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "rustc should be detected");
    let (g, a) = result.unwrap();
    assert_eq!(g.tool.name, "rustc");
    assert!(a.is_some(), "fallback action should be resolved for rustc");
}

#[test]
fn test_rustc_rules_match_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("rustc").unwrap();
    let fallback = grammar.fallback.as_ref().unwrap();

    // Hazard: error[E...]
    let error_line = "error[E0425]: cannot find value `foo` in this scope";
    let err_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(error_line));
    assert!(err_matched, "rustc hazard should match 'error[E...]'");

    // Hazard: warning[...]
    let warn_line = "warning[E0unused]: unused variable: `z`";
    let warn_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(warn_line));
    assert!(warn_matched, "rustc hazard should match 'warning[...]'");

    // Hazard: cannot find
    let cannot_line = "error[E0425]: cannot find value `foo` in this scope";
    let cannot_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(cannot_line));
    assert!(cannot_matched, "rustc hazard should match 'cannot find'");

    // Hazard: aborting due to
    let abort_line = "error: aborting due to 2 previous errors; 1 warning emitted";
    let abort_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(abort_line));
    assert!(abort_matched, "rustc hazard should match 'error: aborting due to'");
}

#[test]
fn test_rustc_error_fixture_hazards() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("rustc").unwrap();
    let fallback = grammar.fallback.as_ref().unwrap();

    let fixture = load_fixture("rustc_error.txt");
    let mut hazard_count = 0;
    for line in fixture.lines() {
        for rule in &fallback.hazard {
            if rule.pattern.is_match(line) {
                hazard_count += 1;
                break;
            }
        }
    }
    assert!(hazard_count >= 3, "rustc_error.txt should have at least 3 hazard matches, found {hazard_count}");
}

#[test]
fn test_rustc_error_code_captures() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("rustc").unwrap();
    let fallback = grammar.fallback.as_ref().unwrap();

    let error_line = "error[E0425]: cannot find value `foo` in this scope";
    let mut captured = HashMap::new();
    for rule in &fallback.hazard {
        if let Some(caps) = rule.pattern.captures(error_line) {
            for name in &rule.captures {
                if let Some(m) = caps.name(name) {
                    captured.insert(name.clone(), m.as_str().to_string());
                }
            }
        }
    }
    assert_eq!(captured.get("code").map(|s| s.as_str()), Some("E0425"));
}

// ===========================================================================
// rsync + ssh + systemctl + ansible grammar tests (e6)
// ===========================================================================

#[test]
fn test_rsync_grammar_parses() {
    let grammar = load_tool_grammar("rsync");
    assert_eq!(grammar.tool.name, "rsync");
    assert!(grammar.detect.contains(&"rsync".to_string()));
    assert_eq!(grammar.inherit, vec!["ansi-progress"]);
    assert!(grammar.fallback.is_some());
    assert!(grammar.quiet.is_some());
}

#[test]
fn test_detect_tool_rsync() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["rsync", "-avz", "src/", "dest/"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "rsync should be detected");
    let (g, a) = result.unwrap();
    assert_eq!(g.tool.name, "rsync");
    assert!(a.is_some(), "fallback action should be resolved for rsync");
}

#[test]
fn test_rsync_rules_match_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("rsync").unwrap();
    let fallback = grammar.fallback.as_ref().unwrap();

    // Noise: individual file transfer lines (dedup)
    let file_line = "src/main.rs";
    let file_matched = fallback.noise.iter().any(|r| r.pattern.is_match(file_line));
    assert!(file_matched, "rsync fallback noise should match individual file lines");

    // Outcome: "sent X bytes received Y bytes" summary
    let sent_line = "sent 12,345 bytes  received 234 bytes  25,158.00 bytes/sec";
    let sent_matched = fallback.outcome.iter().any(|r| r.pattern.is_match(sent_line));
    assert!(sent_matched, "rsync fallback outcome should match 'sent ... bytes'");

    // Hazard: "rsync error:"
    let error_line = "rsync error: some files/attrs were not transferred (code 23) at main.c(1207) [sender=3.2.7]";
    let err_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(error_line));
    assert!(err_matched, "rsync fallback hazard should match 'rsync error:'");

    // Global noise: "sending incremental file list"
    let send_line = "sending incremental file list";
    let send_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(send_line));
    assert!(send_matched, "rsync global_noise should match 'sending incremental file list'");

    // Global noise: progress percentage lines
    let progress_line = "   45%  12.34MB/s   0:01:23";
    let progress_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(progress_line));
    assert!(progress_matched, "rsync global_noise should match progress percentage lines");
}

#[test]
fn test_rsync_outcome_captures() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("rsync").unwrap();
    let fallback = grammar.fallback.as_ref().unwrap();

    let fixture = load_fixture("rsync_transfer.txt");
    let mut captured = HashMap::new();

    for line in fixture.lines() {
        for rule in &fallback.outcome {
            if let Some(caps) = rule.pattern.captures(line) {
                for name in &rule.captures {
                    if let Some(m) = caps.name(name) {
                        captured.insert(name.clone(), m.as_str().to_string());
                    }
                }
            }
        }
    }

    assert_eq!(captured.get("sent").map(|s| s.as_str()), Some("12,345"));
    assert_eq!(captured.get("received").map(|s| s.as_str()), Some("234"));
}

#[test]
fn test_rsync_error_fixture_hazards() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("rsync").unwrap();
    let fallback = grammar.fallback.as_ref().unwrap();

    let fixture = load_fixture("rsync_error.txt");
    let mut hazard_count = 0;
    for line in fixture.lines() {
        for rule in &fallback.hazard {
            if rule.pattern.is_match(line) {
                hazard_count += 1;
                break;
            }
        }
    }
    assert!(hazard_count >= 2, "rsync_error.txt should have at least 2 hazard matches, found {hazard_count}");
}

#[test]
fn test_ssh_grammar_parses() {
    let grammar = load_tool_grammar("ssh");
    assert_eq!(grammar.tool.name, "ssh");
    assert!(grammar.detect.contains(&"ssh".to_string()));
    assert!(grammar.detect.contains(&"scp".to_string()));
    assert!(grammar.detect.contains(&"sftp".to_string()));
    assert_eq!(grammar.inherit, vec!["ansi-progress"]);
    assert!(grammar.fallback.is_some());
}

#[test]
fn test_detect_tool_ssh() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["ssh", "user@host", "ls"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "ssh should be detected");
    let (g, a) = result.unwrap();
    assert_eq!(g.tool.name, "ssh");
    assert!(a.is_some(), "fallback action should be resolved for ssh");
}

#[test]
fn test_ssh_rules_match_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("ssh").unwrap();
    let fallback = grammar.fallback.as_ref().unwrap();

    // Hazard: Connection refused
    let refused_line = "ssh: connect to host example.com port 22: Connection refused";
    let refused_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(refused_line));
    assert!(refused_matched, "ssh fallback hazard should match 'Connection refused'");

    // Hazard: Permission denied
    let perm_line = "Permission denied (publickey,password).";
    let perm_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(perm_line));
    assert!(perm_matched, "ssh fallback hazard should match 'Permission denied'");

    // Hazard: Could not resolve hostname
    let resolve_line = "ssh: Could not resolve hostname nonexistent.host: nodename nor servname provided, or not known";
    let resolve_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(resolve_line));
    assert!(resolve_matched, "ssh fallback hazard should match 'Could not resolve hostname'");

    // Hazard: Host key verification failed
    let hostkey_line = "Host key verification failed.";
    let hostkey_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(hostkey_line));
    assert!(hostkey_matched, "ssh fallback hazard should match 'Host key verification failed'");

    // Global noise: debug1:
    let debug_line = "debug1: Connection established.";
    let debug_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(debug_line));
    assert!(debug_matched, "ssh global_noise should match 'debug1:'");

    // Global noise: Authenticated to
    let auth_line = "Authenticated to host (via publickey).";
    let auth_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(auth_line));
    assert!(auth_matched, "ssh global_noise should match 'Authenticated to'");
}

#[test]
fn test_ssh_error_fixture_hazards() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("ssh").unwrap();
    let fallback = grammar.fallback.as_ref().unwrap();

    let fixture = load_fixture("ssh_error.txt");
    let mut hazard_count = 0;
    for line in fixture.lines() {
        for rule in &fallback.hazard {
            if rule.pattern.is_match(line) {
                hazard_count += 1;
                break;
            }
        }
    }
    assert!(hazard_count >= 3, "ssh_error.txt should have at least 3 hazard matches, found {hazard_count}");
}

#[test]
fn test_systemctl_grammar_parses() {
    let grammar = load_tool_grammar("systemctl");
    assert_eq!(grammar.tool.name, "systemctl");
    assert!(grammar.detect.contains(&"systemctl".to_string()));
    assert_eq!(grammar.inherit, vec!["ansi-progress"]);
    assert!(grammar.actions.contains_key("status"));
    assert!(grammar.actions.contains_key("start"));
    assert!(grammar.actions.contains_key("stop"));
    assert!(grammar.actions.contains_key("restart"));
    assert!(grammar.actions.contains_key("enable"));
    assert!(grammar.actions.contains_key("disable"));
}

#[test]
fn test_detect_tool_systemctl() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["systemctl", "status", "nginx"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "systemctl should be detected");
    let (g, a) = result.unwrap();
    assert_eq!(g.tool.name, "systemctl");
    assert!(a.is_some(), "status action should be resolved");
}

#[test]
fn test_systemctl_rules_match_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("systemctl").unwrap();
    let status = grammar.actions.get("status").unwrap();

    // Outcome: Active: active (running)
    let active_line = "     Active: active (running) since Mon 2026-03-03 10:15:30 UTC; 2h 30min ago";
    let active_matched = status.outcome.iter().any(|r| r.pattern.is_match(active_line));
    assert!(active_matched, "systemctl status outcome should match 'Active: active (running)'");

    // Noise: Loaded: line
    let loaded_line = "     Loaded: loaded (/lib/systemd/system/nginx.service; enabled; vendor preset: enabled)";
    let loaded_matched = status.noise.iter().any(|r| r.pattern.is_match(loaded_line));
    assert!(loaded_matched, "systemctl status noise should match 'Loaded:'");

    // Noise: Docs: line
    let docs_line = "       Docs: man:nginx(8)";
    let docs_matched = status.noise.iter().any(|r| r.pattern.is_match(docs_line));
    assert!(docs_matched, "systemctl status noise should match 'Docs:'");

    // Noise: Process: line
    let process_line = "    Process: 1234 ExecStartPre=/usr/sbin/nginx -t (code=exited, status=0/SUCCESS)";
    let process_matched = status.noise.iter().any(|r| r.pattern.is_match(process_line));
    assert!(process_matched, "systemctl status noise should match 'Process:'");

    // Noise: CGroup: line
    let cgroup_line = "     CGroup: /system.slice/nginx.service";
    let cgroup_matched = status.noise.iter().any(|r| r.pattern.is_match(cgroup_line));
    assert!(cgroup_matched, "systemctl status noise should match 'CGroup:'");

    // Noise: journal log lines (dedup)
    let journal_line = "Mar 03 10:15:30 server systemd[1]: Starting A high performance web server...";
    let journal_matched = status.noise.iter().any(|r| r.pattern.is_match(journal_line));
    assert!(journal_matched, "systemctl status noise should match journal log lines");

    // Hazard: Active: failed
    let failed_line = "     Active: failed (Result: exit-code) since Mon 2026-03-03 10:15:30 UTC";
    let failed_matched = status.hazard.iter().any(|r| r.pattern.is_match(failed_line));
    assert!(failed_matched, "systemctl status hazard should match 'Active: failed'");

    // Hazard: Active: inactive
    let inactive_line = "     Active: inactive (dead) since Mon 2026-03-03 10:15:30 UTC";
    let inactive_matched = status.hazard.iter().any(|r| r.pattern.is_match(inactive_line));
    assert!(inactive_matched, "systemctl status hazard should match 'Active: inactive'");
}

#[test]
fn test_ansible_grammar_parses() {
    let grammar = load_tool_grammar("ansible");
    assert_eq!(grammar.tool.name, "ansible");
    assert!(grammar.detect.contains(&"ansible".to_string()));
    assert!(grammar.detect.contains(&"ansible-playbook".to_string()));
    assert_eq!(grammar.inherit, vec!["ansi-progress"]);
    assert!(grammar.fallback.is_some());
}

#[test]
fn test_detect_tool_ansible() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["ansible-playbook", "deploy.yml"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "ansible-playbook should be detected");
    let (g, a) = result.unwrap();
    assert_eq!(g.tool.name, "ansible");
    assert!(a.is_some(), "fallback action should be resolved for ansible");
}

#[test]
fn test_ansible_rules_match_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("ansible").unwrap();
    let fallback = grammar.fallback.as_ref().unwrap();

    // Noise: ok: lines (dedup)
    let ok_line = "ok: [web1.example.com]";
    let ok_matched = fallback.noise.iter().any(|r| r.pattern.is_match(ok_line));
    assert!(ok_matched, "ansible fallback noise should match 'ok:' lines");

    // Noise: skipping: lines (dedup)
    let skip_line = "skipping: [web1.example.com]";
    let skip_matched = fallback.noise.iter().any(|r| r.pattern.is_match(skip_line));
    assert!(skip_matched, "ansible fallback noise should match 'skipping:' lines");

    // Hazard: fatal: line
    let fatal_line = "fatal: [web2.example.com]: UNREACHABLE! => {\"changed\": false, \"msg\": \"Failed to connect\"}";
    let fatal_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(fatal_line));
    assert!(fatal_matched, "ansible fallback hazard should match 'fatal:' lines");

    // Hazard: ERROR!
    let error_line = "ERROR! the playbook: missing.yml could not be found";
    let error_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(error_line));
    assert!(error_matched, "ansible fallback hazard should match 'ERROR!'");

    // Hazard: WARNING
    let warn_line = "WARNING]: provided hosts list is empty";
    let warn_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(warn_line));
    assert!(warn_matched, "ansible fallback hazard should match 'WARNING'");

    // Outcome: PLAY RECAP line with captures
    let recap_line = "web1.example.com           : ok=3    changed=1    unreachable=0    failed=0    skipped=0";
    let recap_matched = fallback.outcome.iter().any(|r| r.pattern.is_match(recap_line));
    assert!(recap_matched, "ansible fallback outcome should match PLAY RECAP host line");

    // Global noise: Gathering Facts
    let gather_line = "TASK [Gathering Facts] ********************************************************";
    let gather_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(gather_line));
    assert!(gather_matched, "ansible global_noise should match 'Gathering Facts'");
}

#[test]
fn test_ansible_outcome_captures() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("ansible").unwrap();
    let fallback = grammar.fallback.as_ref().unwrap();

    let fixture = load_fixture("ansible_playbook.txt");
    let mut all_captured: Vec<HashMap<String, String>> = Vec::new();

    for line in fixture.lines() {
        for rule in &fallback.outcome {
            if let Some(caps) = rule.pattern.captures(line) {
                let mut captured = HashMap::new();
                for name in &rule.captures {
                    if let Some(m) = caps.name(name) {
                        captured.insert(name.clone(), m.as_str().to_string());
                    }
                }
                if !captured.is_empty() {
                    all_captured.push(captured);
                }
            }
        }
    }

    assert_eq!(all_captured.len(), 2, "should capture 2 PLAY RECAP lines");
    assert_eq!(all_captured[0].get("host").map(|s| s.as_str()), Some("web1.example.com"));
    assert_eq!(all_captured[0].get("ok").map(|s| s.as_str()), Some("3"));
    assert_eq!(all_captured[0].get("changed").map(|s| s.as_str()), Some("1"));
    assert_eq!(all_captured[0].get("failed").map(|s| s.as_str()), Some("0"));
    assert_eq!(all_captured[1].get("host").map(|s| s.as_str()), Some("web2.example.com"));
    assert_eq!(all_captured[1].get("changed").map(|s| s.as_str()), Some("0"));
}

#[test]
fn test_ansible_error_fixture_hazards() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("ansible").unwrap();
    let fallback = grammar.fallback.as_ref().unwrap();

    let fixture = load_fixture("ansible_playbook_error.txt");
    let mut hazard_count = 0;
    for line in fixture.lines() {
        for rule in &fallback.hazard {
            if rule.pattern.is_match(line) {
                hazard_count += 1;
                break;
            }
        }
    }
    assert!(hazard_count >= 1, "ansible_playbook_error.txt should have at least 1 hazard match (fatal/unreachable), found {hazard_count}");
}

// ===========================================================================
// apt + brew + curl grammar tests (e5)
// ===========================================================================

#[test]
fn test_apt_grammar_parses() {
    let grammar = load_tool_grammar("apt");
    assert_eq!(grammar.tool.name, "apt");
    assert!(grammar.detect.contains(&"apt".to_string()));
    assert!(grammar.detect.contains(&"apt-get".to_string()));
    assert_eq!(grammar.inherit, vec!["ansi-progress"]);
    assert!(grammar.actions.contains_key("install"));
    assert!(grammar.actions.contains_key("update"));
    assert!(grammar.actions.contains_key("upgrade"));
    assert!(grammar.actions.contains_key("remove"));
    assert!(grammar.quiet.is_some());
}

#[test]
fn test_detect_tool_apt() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["apt", "install", "vim"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "apt should be detected");
    let (g, a) = result.unwrap();
    assert_eq!(g.tool.name, "apt");
    assert!(a.is_some(), "install action should be resolved");
}

#[test]
fn test_detect_tool_apt_get() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["apt-get", "update"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "apt-get should be detected as apt");
    assert_eq!(result.unwrap().0.tool.name, "apt");
}

#[test]
fn test_apt_rules_match_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("apt").unwrap();
    let install = grammar.actions.get("install").unwrap();

    // Global noise: Reading package lists
    let reading_line = "Reading package lists... Done";
    let reading_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(reading_line));
    assert!(reading_matched, "apt global_noise should match 'Reading package lists'");

    // Global noise: Building dependency tree
    let building_line = "Building dependency tree... Done";
    let building_matched = grammar.global_noise.iter().any(|r| r.pattern.is_match(building_line));
    assert!(building_matched, "apt global_noise should match 'Building dependency tree'");

    // Noise: Get: download lines
    let get_line = "Get:1 http://archive.ubuntu.com/ubuntu jammy/main amd64 libbar2 amd64 2.1.0-1 [234 kB]";
    let get_matched = install.noise.iter().any(|r| r.pattern.is_match(get_line));
    assert!(get_matched, "apt install noise should match 'Get:' lines");

    // Noise: Setting up
    let setup_line = "Setting up libbar2:amd64 (2.1.0-1) ...";
    let setup_matched = install.noise.iter().any(|r| r.pattern.is_match(setup_line));
    assert!(setup_matched, "apt install noise should match 'Setting up'");

    // Noise: Unpacking
    let unpack_line = "Unpacking libbar2:amd64 (2.1.0-1) ...";
    let unpack_matched = install.noise.iter().any(|r| r.pattern.is_match(unpack_line));
    assert!(unpack_matched, "apt install noise should match 'Unpacking'");

    // Hazard: E: error
    let error_line = "E: Unable to locate package nonexistent-pkg";
    let err_matched = install.hazard.iter().any(|r| r.pattern.is_match(error_line));
    assert!(err_matched, "apt install hazard should match 'E:' error");

    // Hazard: dpkg: error
    let dpkg_line = "dpkg: error processing package broken-pkg (--configure):";
    let dpkg_matched = install.hazard.iter().any(|r| r.pattern.is_match(dpkg_line));
    assert!(dpkg_matched, "apt install hazard should match 'dpkg: error'");

    // Outcome: newly installed count
    let outcome_line = "0 upgraded, 2 newly installed, 0 to remove and 15 not upgraded.";
    let outcome_matched = install.outcome.iter().any(|r| r.pattern.is_match(outcome_line));
    assert!(outcome_matched, "apt install outcome should match 'newly installed' count");
}

#[test]
fn test_apt_error_fixture_hazards() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("apt").unwrap();
    let install = grammar.actions.get("install").unwrap();

    let fixture = load_fixture("apt_install_error.txt");
    let mut hazard_count = 0;
    for line in fixture.lines() {
        for rule in &install.hazard {
            if rule.pattern.is_match(line) {
                hazard_count += 1;
                break;
            }
        }
    }
    assert!(hazard_count >= 3, "apt_install_error.txt should have at least 3 hazard matches, found {hazard_count}");

    // Verify specific hazard types are detected
    let fixture_text = load_fixture("apt_install_error.txt");
    let has_e_error = fixture_text.lines().any(|line| {
        install.hazard.iter().any(|r| r.pattern.is_match(line) && line.starts_with("E:"))
    });
    assert!(has_e_error, "should detect E: error lines");

    let has_dpkg = fixture_text.lines().any(|line| {
        install.hazard.iter().any(|r| r.pattern.is_match(line) && line.starts_with("dpkg: error"))
    });
    assert!(has_dpkg, "should detect dpkg: error lines");
}

#[test]
fn test_apt_update_rules() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("apt").unwrap();
    let update = grammar.actions.get("update").unwrap();

    // Noise: Get/Hit/Ign lines (dedup)
    let get_line = "Get:1 http://archive.ubuntu.com/ubuntu jammy InRelease [270 kB]";
    let get_matched = update.noise.iter().any(|r| r.pattern.is_match(get_line));
    assert!(get_matched, "apt update noise should match 'Get:' lines");

    let hit_line = "Hit:2 http://security.ubuntu.com/ubuntu jammy-security InRelease";
    let hit_matched = update.noise.iter().any(|r| r.pattern.is_match(hit_line));
    assert!(hit_matched, "apt update noise should match 'Hit:' lines");

    // Outcome: All packages are up to date
    let uptodate = "All packages are up to date.";
    let uptodate_matched = update.outcome.iter().any(|r| r.pattern.is_match(uptodate));
    assert!(uptodate_matched, "apt update outcome should match 'All packages are up to date'");
}

#[test]
fn test_brew_grammar_parses() {
    let grammar = load_tool_grammar("brew");
    assert_eq!(grammar.tool.name, "brew");
    assert!(grammar.detect.contains(&"brew".to_string()));
    assert_eq!(grammar.inherit, vec!["ansi-progress"]);
    assert!(grammar.actions.contains_key("install"));
    assert!(grammar.actions.contains_key("update"));
    assert!(grammar.actions.contains_key("upgrade"));
    assert!(grammar.actions.contains_key("search"));
}

#[test]
fn test_detect_tool_brew() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["brew", "install", "node"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "brew should be detected");
    let (g, a) = result.unwrap();
    assert_eq!(g.tool.name, "brew");
    assert!(a.is_some(), "install action should be resolved");
}

#[test]
fn test_brew_rules_match_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("brew").unwrap();
    let install = grammar.actions.get("install").unwrap();

    // Noise: ==> Downloading
    let dl_line = "==> Downloading https://ghcr.io/v2/homebrew/core/node/blobs/sha256:def456";
    let dl_matched = install.noise.iter().any(|r| r.pattern.is_match(dl_line));
    assert!(dl_matched, "brew install noise should match '==> Downloading'");

    // Noise: ==> Fetching
    let fetch_line = "==> Fetching node";
    let fetch_matched = install.noise.iter().any(|r| r.pattern.is_match(fetch_line));
    assert!(fetch_matched, "brew install noise should match '==> Fetching'");

    // Noise: ==> Pouring
    let pour_line = "==> Pouring node--21.6.0.arm64_sonoma.bottle.tar.gz";
    let pour_matched = install.noise.iter().any(|r| r.pattern.is_match(pour_line));
    assert!(pour_matched, "brew install noise should match '==> Pouring'");

    // Noise: Already downloaded
    let already_line = "Already downloaded: /Users/user/Library/Caches/Homebrew/downloads/abc123--icu4c-74.2.bottle.tar.gz";
    let already_matched = install.noise.iter().any(|r| r.pattern.is_match(already_line));
    assert!(already_matched, "brew install noise should match 'Already downloaded:'");

    // Outcome: beer mug line
    let beer_line = "\u{1f37a}  /opt/homebrew/Cellar/node/21.6.0: 2,123 files, 62.3MB";
    let beer_matched = install.outcome.iter().any(|r| r.pattern.is_match(beer_line));
    assert!(beer_matched, "brew install outcome should match beer mug line");

    // Hazard: Error:
    let error_line = "Error: No such file or directory @ rb_file_s_symlink";
    let err_matched = install.hazard.iter().any(|r| r.pattern.is_match(error_line));
    assert!(err_matched, "brew install hazard should match 'Error:'");

    // Hazard: No available formula
    let no_formula = "No available formula with the name \"nonexistent\"";
    let nf_matched = install.hazard.iter().any(|r| r.pattern.is_match(no_formula));
    assert!(nf_matched, "brew install hazard should match 'No available formula'");
}

#[test]
fn test_brew_update_rules() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("brew").unwrap();
    let update = grammar.actions.get("update").unwrap();

    // Outcome: Already up-to-date
    let uptodate = "Already up-to-date.";
    let uptodate_matched = update.outcome.iter().any(|r| r.pattern.is_match(uptodate));
    assert!(uptodate_matched, "brew update outcome should match 'Already up-to-date'");

    // Outcome: Updated Homebrew
    let updated = "Updated Homebrew from abc123 to def456.";
    let updated_matched = update.outcome.iter().any(|r| r.pattern.is_match(updated));
    assert!(updated_matched, "brew update outcome should match 'Updated Homebrew'");
}

#[test]
fn test_curl_grammar_parses() {
    let grammar = load_tool_grammar("curl");
    assert_eq!(grammar.tool.name, "curl");
    assert!(grammar.detect.contains(&"curl".to_string()));
    assert_eq!(grammar.inherit, vec!["ansi-progress"]);
    assert!(grammar.fallback.is_some());
    assert!(grammar.actions.is_empty());
    assert!(grammar.quiet.is_some());
}

#[test]
fn test_detect_tool_curl() {
    let grammars = build_grammars_map();
    let args: Vec<String> = vec!["curl", "-sS", "https://example.com"]
        .into_iter()
        .map(String::from)
        .collect();
    let result = detect_tool(&args, &grammars);
    assert!(result.is_some(), "curl should be detected");
    let (g, a) = result.unwrap();
    assert_eq!(g.tool.name, "curl");
    assert!(a.is_some(), "fallback action should be resolved for curl");
}

#[test]
fn test_curl_rules_match_fixture() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("curl").unwrap();
    let fallback = grammar.fallback.as_ref().unwrap();

    // Noise: progress meter header
    let header_line = "  % Total    % Received % Xferd  Average Speed   Time    Time     Time  Current";
    let header_matched = fallback.noise.iter().any(|r| r.pattern.is_match(header_line));
    assert!(header_matched, "curl noise should match progress meter header");

    // Noise: Dload/Upload column header
    let dload_line = "                                 Dload  Upload   Total   Spent    Left  Speed";
    let dload_matched = fallback.noise.iter().any(|r| r.pattern.is_match(dload_line));
    assert!(dload_matched, "curl noise should match Dload/Upload header");

    // Noise: progress data line
    let progress_line = "  0     0    0     0    0     0      0      0 --:--:-- --:--:-- --:--:--     0";
    let progress_matched = fallback.noise.iter().any(|r| r.pattern.is_match(progress_line));
    assert!(progress_matched, "curl noise should match progress data line");

    // Hazard: curl error code
    let curl_err = "curl: (6) Could not resolve host: nonexistent.example.com";
    let curl_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(curl_err));
    assert!(curl_matched, "curl hazard should match 'curl: (6)'");

    // Hazard: Connection refused
    let conn_line = "curl: (7) Failed to connect to localhost port 9999: Connection refused";
    let conn_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(conn_line));
    assert!(conn_matched, "curl hazard should match 'Connection refused'");

    // Hazard: SSL certificate problem
    let ssl_line = "curl: (60) SSL certificate problem: unable to get local issuer certificate";
    let ssl_matched = fallback.hazard.iter().any(|r| r.pattern.is_match(ssl_line));
    assert!(ssl_matched, "curl hazard should match 'SSL certificate problem'");
}

#[test]
fn test_curl_error_fixture_hazards() {
    let grammars = build_grammars_map();
    let grammar = grammars.get("curl").unwrap();
    let fallback = grammar.fallback.as_ref().unwrap();

    let fixture = load_fixture("curl_error.txt");
    let mut hazard_count = 0;
    for line in fixture.lines() {
        for rule in &fallback.hazard {
            if rule.pattern.is_match(line) {
                hazard_count += 1;
                break;
            }
        }
    }
    assert!(hazard_count >= 2, "curl_error.txt should have at least 2 hazard matches, found {hazard_count}");
}
