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
// Test 20: load_all_grammars loads all 5 grammars from the directory
// ===========================================================================

#[test]
fn test_20_load_all_grammars_loads_all_five() {
    let grammars = build_grammars_map();

    assert!(
        grammars.contains_key("npm"),
        "load_all_grammars should load npm"
    );
    assert!(
        grammars.contains_key("cargo"),
        "load_all_grammars should load cargo"
    );
    assert!(
        grammars.contains_key("git"),
        "load_all_grammars should load git"
    );
    assert!(
        grammars.contains_key("docker"),
        "load_all_grammars should load docker"
    );
    assert!(
        grammars.contains_key("make"),
        "load_all_grammars should load make"
    );

    // Verify all grammars have had inheritance resolved
    let npm = grammars.get("npm").unwrap();
    // npm inherits ansi-progress (2 rules) + node-stacktrace (2 rules) + own 2 global_noise = 6
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
