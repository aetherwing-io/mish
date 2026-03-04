//! Bidirectional argument injection.
//!
//! Quiet: inject --quiet flags to reduce noise at source.
//! Verbose: inject -v flags to enrich terse commands.
//! Grammar-declared, never injects behavior-changing flags.

use crate::core::grammar::{Action, Grammar};

// Re-export OutputMode from format.rs — single canonical definition.
pub use crate::core::format::OutputMode;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A recommendation from preflight analysis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recommendation {
    pub flag: String,
    pub reason: String,
}

/// Result of preflight analysis.
pub struct PreflightResult {
    /// Flags that were automatically injected (safe_inject)
    pub injected: Vec<String>,
    /// Flags that are recommended but not injected
    pub recommendations: Vec<Recommendation>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Run preflight analysis on a command, potentially injecting flags.
/// Modifies `command` in-place by appending safe_inject flags.
///
/// In Passthrough mode, no injection occurs (safety constraint).
/// safe_inject flags are added to the command if not already present.
/// recommend flags are returned as recommendations but never injected.
///
/// When an action is provided and has action-specific quiet overrides,
/// those override the global quiet config entirely.
pub fn preflight(
    command: &mut Vec<String>,
    grammar: &Grammar,
    action: Option<&Action>,
    mode: OutputMode,
) -> PreflightResult {
    let mut result = PreflightResult {
        injected: Vec::new(),
        recommendations: Vec::new(),
    };

    // Safety: never inject in Passthrough mode
    if mode == OutputMode::Passthrough {
        return result;
    }

    // Get quiet config from grammar
    let quiet = match &grammar.quiet {
        Some(q) => q,
        None => return result,
    };

    // Determine which safe_inject and recommend lists to use.
    // If an action is matched and has action-specific overrides, use those
    // instead of the global quiet config.
    let (safe_inject, recommend) = if let Some(act) = action {
        // Find the action name by matching detect lists
        let action_override = grammar.actions.iter().find_map(|(name, a)| {
            // Compare by detect list identity — the action reference must
            // point to the same Action data as what's in the grammar's actions map.
            if std::ptr::eq(a, act) {
                quiet.actions.get(name)
            } else {
                None
            }
        });

        match action_override {
            Some(override_cfg) => (
                override_cfg.safe_inject.as_slice(),
                override_cfg.recommend.as_slice(),
            ),
            None => (quiet.safe_inject.as_slice(), quiet.recommend.as_slice()),
        }
    } else {
        (quiet.safe_inject.as_slice(), quiet.recommend.as_slice())
    };

    // Inject safe_inject flags (if not already present)
    for flag in safe_inject {
        if !command.contains(flag) {
            command.push(flag.clone());
            result.injected.push(flag.clone());
        }
    }

    // Generate recommendations (never injected, skip if already present)
    for flag in recommend {
        if !command.contains(flag) {
            result.recommendations.push(Recommendation {
                flag: flag.clone(),
                reason: format!(
                    "Consider adding {} for quieter output",
                    flag
                ),
            });
        }
    }

    result
}

/// Inject verbose flags from the grammar's verbosity config.
///
/// Returns the list of flags that were actually injected.
/// Skips flags already present in the command.
pub fn inject_verbosity(command: &mut Vec<String>, grammar: &Grammar) -> Vec<String> {
    let mut injected = Vec::new();

    let verbosity = match &grammar.verbosity {
        Some(v) => v,
        None => return injected,
    };

    for flag in &verbosity.inject {
        if !command.contains(flag) {
            command.push(flag.clone());
            injected.push(flag.clone());
        }
    }

    injected
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::grammar::{load_grammar_from_str, resolve_action};

    // Helper to create a command vec from string slices
    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    // Test 1: quiet injection — safe_inject flags added to command
    #[test]
    fn test_quiet_injection_safe_inject_flags_added() {
        let toml_str = r#"
[tool]
name = "npm"

[quiet]
safe_inject = ["--no-progress"]
recommend = []
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut command = cmd(&["npm", "install", "express"]);
        let result = preflight(&mut command, &grammar, None, OutputMode::Human);

        assert_eq!(result.injected, vec!["--no-progress"]);
        assert!(command.contains(&"--no-progress".to_string()));
        assert_eq!(command.len(), 4); // npm install express --no-progress
    }

    // Test 2: verbose injection — verbosity.inject flags added
    #[test]
    fn test_verbose_injection_flags_added() {
        let toml_str = r#"
[tool]
name = "cp"

[verbosity]
inject = ["-v"]
provides = ["copied-files"]
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut command = cmd(&["cp", "src.txt", "dst.txt"]);
        let injected = inject_verbosity(&mut command, &grammar);

        assert_eq!(injected, vec!["-v"]);
        assert!(command.contains(&"-v".to_string()));
        assert_eq!(command.len(), 4); // cp src.txt dst.txt -v
    }

    // Test 3: no double injection — if flag already present, skip
    #[test]
    fn test_no_double_injection() {
        let toml_str = r#"
[tool]
name = "npm"

[quiet]
safe_inject = ["--no-progress"]
recommend = []
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut command = cmd(&["npm", "install", "--no-progress"]);
        let result = preflight(&mut command, &grammar, None, OutputMode::Human);

        assert!(result.injected.is_empty());
        assert_eq!(command.len(), 3); // unchanged
    }

    // Test 4: passthrough mode skip — no injection in Passthrough mode
    #[test]
    fn test_passthrough_mode_skips_injection() {
        let toml_str = r#"
[tool]
name = "npm"

[quiet]
safe_inject = ["--no-progress"]
recommend = ["--loglevel=warn"]
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut command = cmd(&["npm", "install"]);
        let result = preflight(&mut command, &grammar, None, OutputMode::Passthrough);

        assert!(result.injected.is_empty());
        assert!(result.recommendations.is_empty());
        assert_eq!(command.len(), 2); // unchanged
    }

    // Test 5: per-action override — action-specific quiet flags used when action matches
    #[test]
    fn test_per_action_override() {
        let toml_str = r#"
[tool]
name = "docker"

[actions.build]
detect = ["build"]

[actions.build.summary]
success = "built"

[quiet]
safe_inject = ["--quiet"]
recommend = []

[quiet.actions.build]
safe_inject = ["--progress=plain"]
recommend = ["-q"]
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let args = cmd(&["docker", "build", "."]);
        let action = resolve_action(&grammar, &args);
        assert!(action.is_some());

        let mut command = cmd(&["docker", "build", "."]);
        let result = preflight(&mut command, &grammar, action, OutputMode::Human);

        // Should use action-specific overrides, not global
        assert_eq!(result.injected, vec!["--progress=plain"]);
        assert!(!command.contains(&"--quiet".to_string()));
        assert_eq!(result.recommendations.len(), 1);
        assert_eq!(result.recommendations[0].flag, "-q");
    }

    // Test 6: recommendation generation — recommend flags returned but not injected
    #[test]
    fn test_recommendation_generation() {
        let toml_str = r#"
[tool]
name = "cargo"

[quiet]
safe_inject = []
recommend = ["--message-format=json"]
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut command = cmd(&["cargo", "build"]);
        let result = preflight(&mut command, &grammar, None, OutputMode::Human);

        assert!(result.injected.is_empty());
        assert_eq!(result.recommendations.len(), 1);
        assert_eq!(result.recommendations[0].flag, "--message-format=json");
        assert_eq!(command.len(), 2); // no flags injected
    }

    // Test 7: safety — no injection without grammar declaration
    #[test]
    fn test_no_injection_without_grammar_declaration() {
        let toml_str = r#"
[tool]
name = "echo"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut command = cmd(&["echo", "hello"]);
        let result = preflight(&mut command, &grammar, None, OutputMode::Human);

        assert!(result.injected.is_empty());
        assert!(result.recommendations.is_empty());
        assert_eq!(command.len(), 2); // unchanged
    }

    // Test 8: flag position — injected flags appended at end
    #[test]
    fn test_flag_position_appended_at_end() {
        let toml_str = r#"
[tool]
name = "pip"

[quiet]
safe_inject = ["--no-color", "--progress-bar=off"]
recommend = []
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut command = cmd(&["pip", "install", "requests"]);
        let _result = preflight(&mut command, &grammar, None, OutputMode::Human);

        assert_eq!(command.len(), 5);
        assert_eq!(command[0], "pip");
        assert_eq!(command[1], "install");
        assert_eq!(command[2], "requests");
        assert_eq!(command[3], "--no-color");
        assert_eq!(command[4], "--progress-bar=off");
    }

    // Test 9: empty grammar — no quiet/verbosity config, no changes
    #[test]
    fn test_empty_grammar_no_changes() {
        let toml_str = r#"
[tool]
name = "echo"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut command = cmd(&["echo", "hello"]);
        let original = command.clone();

        let quiet_result = preflight(&mut command, &grammar, None, OutputMode::Human);
        let verbose_result = inject_verbosity(&mut command, &grammar);

        assert!(quiet_result.injected.is_empty());
        assert!(quiet_result.recommendations.is_empty());
        assert!(verbose_result.is_empty());
        assert_eq!(command, original);
    }

    // Test 10: both quiet and verbose — quiet injected, verbose injected separately
    #[test]
    fn test_both_quiet_and_verbose() {
        let toml_str = r#"
[tool]
name = "rsync"

[quiet]
safe_inject = ["--no-progress"]
recommend = []

[verbosity]
inject = ["-v", "--stats"]
provides = ["transfer-summary"]
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();

        // Quiet injection (for condense mode)
        let mut command = cmd(&["rsync", "-a", "src/", "dst/"]);
        let quiet_result = preflight(&mut command, &grammar, None, OutputMode::Human);
        assert_eq!(quiet_result.injected, vec!["--no-progress"]);

        // Verbose injection (for narrate mode — separate call)
        let mut command2 = cmd(&["rsync", "-a", "src/", "dst/"]);
        let verbose_result = inject_verbosity(&mut command2, &grammar);
        assert_eq!(verbose_result, vec!["-v", "--stats"]);
    }

    // Test 11: action-specific quiet overrides global
    #[test]
    fn test_action_specific_overrides_global() {
        let toml_str = r#"
[tool]
name = "npm"

[actions.install]
detect = ["install", "i"]

[actions.install.summary]
success = "installed"

[actions.test]
detect = ["test", "t"]

[actions.test.summary]
success = "tested"

[quiet]
safe_inject = ["--no-progress"]
recommend = ["--loglevel=warn"]

[quiet.actions.install]
safe_inject = ["--no-fund", "--no-audit"]
recommend = ["--prefer-offline"]
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();

        // "npm install" — should use action-specific overrides
        let args_install = cmd(&["npm", "install"]);
        let action_install = resolve_action(&grammar, &args_install);
        let mut cmd_install = cmd(&["npm", "install"]);
        let result_install =
            preflight(&mut cmd_install, &grammar, action_install, OutputMode::Human);

        assert_eq!(result_install.injected, vec!["--no-fund", "--no-audit"]);
        assert!(!cmd_install.contains(&"--no-progress".to_string())); // global NOT used
        assert_eq!(result_install.recommendations.len(), 1);
        assert_eq!(result_install.recommendations[0].flag, "--prefer-offline");

        // "npm test" — no action-specific override, should use global
        let args_test = cmd(&["npm", "test"]);
        let action_test = resolve_action(&grammar, &args_test);
        let mut cmd_test = cmd(&["npm", "test"]);
        let result_test =
            preflight(&mut cmd_test, &grammar, action_test, OutputMode::Human);

        assert_eq!(result_test.injected, vec!["--no-progress"]);
        assert_eq!(result_test.recommendations.len(), 1);
        assert_eq!(result_test.recommendations[0].flag, "--loglevel=warn");
    }

    // Test 12: multiple flags injected correctly
    #[test]
    fn test_multiple_flags_injected_correctly() {
        let toml_str = r#"
[tool]
name = "pip"

[quiet]
safe_inject = ["--no-color", "--progress-bar=off"]
recommend = ["-q"]
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut command = cmd(&["pip", "install", "flask"]);
        let result = preflight(&mut command, &grammar, None, OutputMode::Human);

        assert_eq!(result.injected.len(), 2);
        assert_eq!(result.injected[0], "--no-color");
        assert_eq!(result.injected[1], "--progress-bar=off");
        assert!(command.contains(&"--no-color".to_string()));
        assert!(command.contains(&"--progress-bar=off".to_string()));
        assert_eq!(command.len(), 5);
    }

    // Test 13: no double injection for verbose flags
    #[test]
    fn test_no_double_injection_verbose() {
        let toml_str = r#"
[tool]
name = "cp"

[verbosity]
inject = ["-v"]
provides = ["copied-files"]
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut command = cmd(&["cp", "-v", "src.txt", "dst.txt"]);
        let injected = inject_verbosity(&mut command, &grammar);

        assert!(injected.is_empty());
        assert_eq!(command.len(), 4); // unchanged
    }

    // Test 14: Json mode still injects (only Passthrough skips)
    #[test]
    fn test_json_mode_still_injects() {
        let toml_str = r#"
[tool]
name = "npm"

[quiet]
safe_inject = ["--no-progress"]
recommend = []
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut command = cmd(&["npm", "install"]);
        let result = preflight(&mut command, &grammar, None, OutputMode::Json);

        assert_eq!(result.injected, vec!["--no-progress"]);
    }

    // Test 15: Context mode still injects
    #[test]
    fn test_context_mode_still_injects() {
        let toml_str = r#"
[tool]
name = "npm"

[quiet]
safe_inject = ["--no-progress"]
recommend = []
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut command = cmd(&["npm", "install"]);
        let result = preflight(&mut command, &grammar, None, OutputMode::Context);

        assert_eq!(result.injected, vec!["--no-progress"]);
    }

    // Test 16: recommendation skipped if flag already present in command
    #[test]
    fn test_recommendation_skipped_if_flag_already_present() {
        let toml_str = r#"
[tool]
name = "cargo"

[quiet]
safe_inject = []
recommend = ["--message-format=json"]
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();

        // User already added the recommended flag
        let mut command = cmd(&["cargo", "build", "--message-format=json"]);
        let result = preflight(&mut command, &grammar, None, OutputMode::Human);

        assert!(
            result.recommendations.is_empty(),
            "should not recommend a flag the user already provided"
        );
        assert_eq!(command.len(), 3); // unchanged
    }
}
