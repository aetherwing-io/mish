//! Policy matcher — evaluates commands and prompts against compiled policy rules.
//!
//! Two entry points:
//! - `check_forbidden()` — called BEFORE execution, checks command against forbidden rules
//! - `check_yield()` — called WHEN yield detected, checks prompt against yield_to_operator
//!   and auto_confirm rules (with scope matching)
//!
//! Precedence:
//! - Forbidden is checked before execution (separate function call)
//! - yield_to_operator is checked before auto_confirm (within check_yield)
//! - auto_confirm requires scope match
//! - If nothing matches, falls through to LLM

use super::config::CompiledPolicy;
use super::scope::scope_matches;

// ---------------------------------------------------------------------------
// PolicyDecision
// ---------------------------------------------------------------------------

/// The result of evaluating policy rules against a command/prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    /// Command is forbidden -- block before execution.
    Forbidden { message: String },
    /// Yield to operator -- route to human handoff.
    YieldToOperator { notify: bool },
    /// Auto-confirm -- send the configured response.
    AutoConfirm { response: String },
    /// No policy match -- fall through to LLM.
    FallThroughToLlm,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Check if a command is forbidden by policy rules.
///
/// Called BEFORE execution. Returns `Some(Forbidden{...})` if matched,
/// `None` otherwise.
pub fn check_forbidden(command: &str, policy: &CompiledPolicy) -> Option<PolicyDecision> {
    for rule in &policy.forbidden {
        if rule.pattern.is_match(command) {
            return Some(PolicyDecision::Forbidden {
                message: rule.message.clone(),
            });
        }
    }
    None
}

/// Check yield policy when a process is waiting for input.
///
/// Called WHEN yield detected (process is waiting for input).
/// Checks in order:
/// 1. yield_to_operator patterns (matched against prompt_text)
/// 2. auto_confirm patterns (matched against prompt_text, with scope check against command)
///
/// Returns the first match, or `FallThroughToLlm` if nothing matches.
pub fn check_yield(command: &str, prompt_text: &str, policy: &CompiledPolicy) -> PolicyDecision {
    // 1. Check yield_to_operator rules first
    for rule in &policy.yield_to_operator {
        if rule.pattern.is_match(prompt_text) {
            return PolicyDecision::YieldToOperator { notify: rule.notify };
        }
    }

    // 2. Check auto_confirm rules (with scope check)
    for rule in &policy.auto_confirm {
        if rule.pattern.is_match(prompt_text) {
            if rule.scope.is_none() {
                eprintln!(
                    "mish: warning: auto_confirm rule '{}' has no scope -- matches all commands",
                    rule.pattern
                );
            }
            if scope_matches(command, &rule.scope) {
                return PolicyDecision::AutoConfirm {
                    response: rule.respond.clone(),
                };
            }
        }
    }

    // 3. No match -- fall through to LLM
    PolicyDecision::FallThroughToLlm
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_config_from_str;
    use crate::policy::config::CompiledPolicy;

    /// Helper: build a CompiledPolicy from a TOML string.
    fn policy_from_toml(toml: &str) -> CompiledPolicy {
        let config = load_config_from_str(toml).expect("TOML should parse");
        CompiledPolicy::compile(&config).expect("policy should compile")
    }

    // -----------------------------------------------------------------------
    // Test 1: Forbidden "rm -rf /" is blocked with correct message
    // -----------------------------------------------------------------------
    #[test]
    fn forbidden_rm_rf_root_blocked() {
        let policy = policy_from_toml(
            r#"
[[policy.forbidden]]
pattern = "rm -rf /"
action = "block"
message = "Recursive delete of root filesystem"
"#,
        );

        let result = check_forbidden("rm -rf /", &policy);
        assert_eq!(
            result,
            Some(PolicyDecision::Forbidden {
                message: "Recursive delete of root filesystem".into(),
            })
        );
    }

    // -----------------------------------------------------------------------
    // Test 2: Forbidden "rm file" does not match
    // -----------------------------------------------------------------------
    #[test]
    fn forbidden_rm_file_not_matched() {
        let policy = policy_from_toml(
            r#"
[[policy.forbidden]]
pattern = "rm -rf /"
action = "block"
message = "Recursive delete of root filesystem"
"#,
        );

        let result = check_forbidden("rm file", &policy);
        assert_eq!(result, None);
    }

    // -----------------------------------------------------------------------
    // Test 3: yield_to_operator "Password:" -> YieldToOperator with notify
    // -----------------------------------------------------------------------
    #[test]
    fn yield_to_operator_password_prompt() {
        let policy = policy_from_toml(
            r#"
[[policy.yield_to_operator]]
match = "Password:"
notify = true
"#,
        );

        let result = check_yield("ssh user@host", "Password:", &policy);
        assert_eq!(result, PolicyDecision::YieldToOperator { notify: true });
    }

    // -----------------------------------------------------------------------
    // Test 4: auto_confirm "Proceed?" with scope=npm, cmd=npm -> AutoConfirm
    // -----------------------------------------------------------------------
    #[test]
    fn auto_confirm_scope_match() {
        let policy = policy_from_toml(
            r#"
[[policy.auto_confirm]]
match = "Proceed\\?"
respond = "Y\n"
scope = ["npm"]
"#,
        );

        let result = check_yield("npm install", "Proceed?", &policy);
        assert_eq!(
            result,
            PolicyDecision::AutoConfirm {
                response: "Y\n".into(),
            }
        );
    }

    // -----------------------------------------------------------------------
    // Test 5: auto_confirm scope mismatch (scope=npm, cmd=cargo) -> FallThroughToLlm
    // -----------------------------------------------------------------------
    #[test]
    fn auto_confirm_scope_mismatch() {
        let policy = policy_from_toml(
            r#"
[[policy.auto_confirm]]
match = "Proceed\\?"
respond = "Y\n"
scope = ["npm"]
"#,
        );

        let result = check_yield("cargo build", "Proceed?", &policy);
        assert_eq!(result, PolicyDecision::FallThroughToLlm);
    }

    // -----------------------------------------------------------------------
    // Test 6: Precedence -- yield_to_operator checked before auto_confirm
    // -----------------------------------------------------------------------
    #[test]
    fn precedence_yield_before_auto_confirm() {
        let policy = policy_from_toml(
            r#"
[[policy.yield_to_operator]]
match = "Proceed"
notify = true

[[policy.auto_confirm]]
match = "Proceed"
respond = "Y\n"
scope = ["npm"]
"#,
        );

        // Even though auto_confirm also matches, yield_to_operator wins
        let result = check_yield("npm install", "Proceed?", &policy);
        assert_eq!(result, PolicyDecision::YieldToOperator { notify: true });
    }

    // -----------------------------------------------------------------------
    // Test 7: Unscoped auto_confirm matches (and warns)
    // -----------------------------------------------------------------------
    #[test]
    fn unscoped_auto_confirm_matches_all() {
        let policy = policy_from_toml(
            r#"
[[policy.auto_confirm]]
match = "Continue\\?"
respond = "yes\n"
"#,
        );

        // Should match any command since scope is None
        let result = check_yield("cargo build", "Continue?", &policy);
        assert_eq!(
            result,
            PolicyDecision::AutoConfirm {
                response: "yes\n".into(),
            }
        );

        // Also matches a different command
        let result = check_yield("npm install", "Continue?", &policy);
        assert_eq!(
            result,
            PolicyDecision::AutoConfirm {
                response: "yes\n".into(),
            }
        );
    }

    // -----------------------------------------------------------------------
    // Test 8: Scope extraction (covered in scope.rs, but verify integration)
    // -----------------------------------------------------------------------
    #[test]
    fn scope_extraction_integration() {
        use crate::policy::scope::extract_scope;

        assert_eq!(extract_scope("npm install"), "npm");
        assert_eq!(extract_scope("/usr/bin/npm run"), "npm");
        assert_eq!(extract_scope(""), "");
    }

    // -----------------------------------------------------------------------
    // Test 9: Empty policy -> FallThroughToLlm for everything
    // -----------------------------------------------------------------------
    #[test]
    fn empty_policy_falls_through() {
        let policy = policy_from_toml("");

        // check_forbidden returns None
        assert_eq!(check_forbidden("rm -rf /", &policy), None);
        assert_eq!(check_forbidden("ls -la", &policy), None);

        // check_yield returns FallThroughToLlm
        assert_eq!(
            check_yield("npm install", "Proceed?", &policy),
            PolicyDecision::FallThroughToLlm
        );
        assert_eq!(
            check_yield("ssh host", "Password:", &policy),
            PolicyDecision::FallThroughToLlm
        );
    }

    // -----------------------------------------------------------------------
    // Bonus: Multiple forbidden rules, first match wins
    // -----------------------------------------------------------------------
    #[test]
    fn multiple_forbidden_first_match_wins() {
        let policy = policy_from_toml(
            r#"
[[policy.forbidden]]
pattern = "rm -rf"
action = "block"
message = "First rule"

[[policy.forbidden]]
pattern = "rm -rf /"
action = "block"
message = "Second rule"
"#,
        );

        let result = check_forbidden("rm -rf /", &policy);
        assert_eq!(
            result,
            Some(PolicyDecision::Forbidden {
                message: "First rule".into(),
            })
        );
    }

    // -----------------------------------------------------------------------
    // Bonus: Regex patterns work (not just literal matching)
    // -----------------------------------------------------------------------
    #[test]
    fn regex_patterns_work() {
        let policy = policy_from_toml(
            r#"
[[policy.forbidden]]
pattern = "rm\\s+-rf\\s+/"
action = "block"
message = "Blocked rm -rf /"

[[policy.yield_to_operator]]
match = "[Pp]assword|MFA|OTP"
notify = true

[[policy.auto_confirm]]
match = "\\[Y/n\\]"
respond = "Y\n"
scope = ["apt"]
"#,
        );

        // Forbidden: matches with varying whitespace
        assert!(check_forbidden("rm  -rf  /", &policy).is_some());
        assert!(check_forbidden("rm -rf /", &policy).is_some());
        assert!(check_forbidden("rm -rf /tmp", &policy).is_some()); // contains "rm -rf /"

        // yield_to_operator: regex alternation
        assert_eq!(
            check_yield("ssh host", "Password:", &policy),
            PolicyDecision::YieldToOperator { notify: true }
        );
        assert_eq!(
            check_yield("ssh host", "password:", &policy),
            PolicyDecision::YieldToOperator { notify: true }
        );
        assert_eq!(
            check_yield("ssh host", "Enter MFA code:", &policy),
            PolicyDecision::YieldToOperator { notify: true }
        );
        assert_eq!(
            check_yield("ssh host", "OTP token:", &policy),
            PolicyDecision::YieldToOperator { notify: true }
        );

        // auto_confirm: regex bracket expression
        assert_eq!(
            check_yield("apt install vim", "[Y/n]", &policy),
            PolicyDecision::AutoConfirm {
                response: "Y\n".into(),
            }
        );
    }

    // -----------------------------------------------------------------------
    // Bonus: check_yield with forbidden rules (forbidden is NOT checked here)
    // -----------------------------------------------------------------------
    #[test]
    fn check_yield_ignores_forbidden() {
        let policy = policy_from_toml(
            r#"
[[policy.forbidden]]
pattern = "rm -rf /"
action = "block"
message = "Blocked"
"#,
        );

        // check_yield doesn't evaluate forbidden rules
        let result = check_yield("rm -rf /", "Continue?", &policy);
        assert_eq!(result, PolicyDecision::FallThroughToLlm);
    }

    // -----------------------------------------------------------------------
    // Bonus: auto_confirm with absolute path command and scope
    // -----------------------------------------------------------------------
    #[test]
    fn auto_confirm_with_absolute_path_command() {
        let policy = policy_from_toml(
            r#"
[[policy.auto_confirm]]
match = "Proceed\\?"
respond = "Y\n"
scope = ["npm"]
"#,
        );

        // /usr/bin/npm should match scope "npm" via basename extraction
        let result = check_yield("/usr/bin/npm install", "Proceed?", &policy);
        assert_eq!(
            result,
            PolicyDecision::AutoConfirm {
                response: "Y\n".into(),
            }
        );
    }
}
