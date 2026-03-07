//! Compiled policy configuration with regex patterns.
//!
//! Takes the string-based `PolicyConfig` from `config.rs` and compiles
//! all match patterns into `regex::Regex` for use by the policy matcher.

use regex::Regex;

use crate::config::{HandoffConfig, MishConfig};

/// Policy configuration with compiled regex patterns, ready for matching.
#[derive(Debug, Clone)]
pub struct CompiledPolicy {
    pub auto_confirm: Vec<CompiledAutoConfirmRule>,
    pub yield_to_operator: Vec<CompiledYieldToOperatorRule>,
    pub forbidden: Vec<CompiledForbiddenRule>,
    pub handoff: HandoffConfig,
}

/// Auto-confirm rule with compiled regex.
#[derive(Debug, Clone)]
pub struct CompiledAutoConfirmRule {
    pub pattern: Regex,
    pub respond: String,
    pub scope: Option<Vec<String>>,
}

/// Yield-to-operator rule with compiled regex.
#[derive(Debug, Clone)]
pub struct CompiledYieldToOperatorRule {
    pub pattern: Regex,
    pub notify: bool,
}

/// Forbidden command rule with compiled regex.
#[derive(Debug, Clone)]
pub struct CompiledForbiddenRule {
    pub pattern: Regex,
    pub action: String,
    pub message: String,
}

/// Error compiling a policy pattern.
#[derive(Debug)]
pub struct PolicyCompileError {
    pub rule_type: String,
    pub index: usize,
    pub pattern: String,
    pub source: regex::Error,
}

impl std::fmt::Display for PolicyCompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid regex in {rule_type}[{idx}] pattern {pat:?}: {src}",
            rule_type = self.rule_type,
            idx = self.index,
            pat = self.pattern,
            src = self.source,
        )
    }
}

impl std::error::Error for PolicyCompileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl CompiledPolicy {
    /// Compile all regex patterns from a `MishConfig`.
    /// Returns an error on the first invalid regex.
    pub fn compile(config: &MishConfig) -> Result<Self, PolicyCompileError> {
        let auto_confirm = config
            .policy
            .auto_confirm
            .iter()
            .enumerate()
            .map(|(i, rule)| {
                let pattern = Regex::new(&rule.match_pattern).map_err(|e| PolicyCompileError {
                    rule_type: "auto_confirm".into(),
                    index: i,
                    pattern: rule.match_pattern.clone(),
                    source: e,
                })?;
                Ok(CompiledAutoConfirmRule {
                    pattern,
                    respond: rule.respond.clone(),
                    scope: rule.scope.clone(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let yield_to_operator = config
            .policy
            .yield_to_operator
            .iter()
            .enumerate()
            .map(|(i, rule)| {
                let pattern = Regex::new(&rule.match_pattern).map_err(|e| PolicyCompileError {
                    rule_type: "yield_to_operator".into(),
                    index: i,
                    pattern: rule.match_pattern.clone(),
                    source: e,
                })?;
                Ok(CompiledYieldToOperatorRule {
                    pattern,
                    notify: rule.notify,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let forbidden = config
            .policy
            .forbidden
            .iter()
            .enumerate()
            .map(|(i, rule)| {
                let pattern = Regex::new(&rule.pattern).map_err(|e| PolicyCompileError {
                    rule_type: "forbidden".into(),
                    index: i,
                    pattern: rule.pattern.clone(),
                    source: e,
                })?;
                Ok(CompiledForbiddenRule {
                    pattern,
                    action: rule.action.clone(),
                    message: rule.message.clone(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            auto_confirm,
            yield_to_operator,
            forbidden,
            handoff: config.handoff.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    /// Helper: build a MishConfig from a TOML string.
    fn config_from_toml(toml_str: &str) -> MishConfig {
        config::load_config_from_str(toml_str).expect("test TOML should parse")
    }

    // -- Test 1: Valid TOML → all rules populated --

    #[test]
    fn test_valid_config_all_rules_populated() {
        let cfg = config_from_toml(
            r#"
[[policy.auto_confirm]]
match = 'Do you want to continue'
respond = "Y\n"
scope = ["apt", "apt-get"]

[[policy.auto_confirm]]
match = 'Proceed\?'
respond = "y\n"
scope = ["npm"]

[[policy.yield_to_operator]]
match = '[Pp]assword|MFA|OTP'
notify = true

[[policy.yield_to_operator]]
match = 'passphrase'
notify = false

[[policy.forbidden]]
pattern = 'rm -rf /'
action = "block"
message = "Blocked by policy"

[handoff]
timeout_sec = 120
fallback = "kill"
"#,
        );

        let compiled = CompiledPolicy::compile(&cfg).expect("should compile");

        assert_eq!(compiled.auto_confirm.len(), 2);
        assert_eq!(compiled.yield_to_operator.len(), 2);
        assert_eq!(compiled.forbidden.len(), 1);

        // Verify patterns actually match
        assert!(compiled.auto_confirm[0].pattern.is_match("Do you want to continue? [Y/n]"));
        assert!(compiled.auto_confirm[1].pattern.is_match("Proceed?"));
        assert_eq!(compiled.auto_confirm[0].respond, "Y\n");
        assert_eq!(compiled.auto_confirm[0].scope.as_deref(), Some(&["apt".to_string(), "apt-get".to_string()][..]));

        assert!(compiled.yield_to_operator[0].pattern.is_match("Enter Password:"));
        assert!(compiled.yield_to_operator[0].notify);
        assert!(!compiled.yield_to_operator[1].notify);

        assert!(compiled.forbidden[0].pattern.is_match("rm -rf /"));
        assert_eq!(compiled.forbidden[0].action, "block");
        assert_eq!(compiled.forbidden[0].message, "Blocked by policy");

        // Handoff config preserved
        assert_eq!(compiled.handoff.timeout_sec, 120);
        assert_eq!(compiled.handoff.fallback, "kill");
    }

    // -- Test 2: Missing scope → None (applies globally) --

    #[test]
    fn test_missing_scope_applies_globally() {
        let cfg = config_from_toml(
            r#"
[[policy.auto_confirm]]
match = 'continue'
respond = "y\n"
"#,
        );

        let compiled = CompiledPolicy::compile(&cfg).expect("should compile");
        assert_eq!(compiled.auto_confirm.len(), 1);
        assert!(compiled.auto_confirm[0].scope.is_none());
    }

    // -- Test 3: Invalid regex → compile error --

    #[test]
    fn test_invalid_regex_returns_error() {
        let cfg = config_from_toml(
            r#"
[[policy.auto_confirm]]
match = '[invalid regex('
respond = "y\n"
"#,
        );

        let err = CompiledPolicy::compile(&cfg).unwrap_err();
        assert_eq!(err.rule_type, "auto_confirm");
        assert_eq!(err.index, 0);
        assert_eq!(err.pattern, "[invalid regex(");
    }

    #[test]
    fn test_invalid_regex_in_forbidden() {
        let cfg = config_from_toml(
            r#"
[[policy.forbidden]]
pattern = '(unclosed'
action = "block"
message = "bad"
"#,
        );

        let err = CompiledPolicy::compile(&cfg).unwrap_err();
        assert_eq!(err.rule_type, "forbidden");
        assert_eq!(err.index, 0);
    }

    #[test]
    fn test_invalid_regex_in_yield_to_operator() {
        let cfg = config_from_toml(
            r#"
[[policy.yield_to_operator]]
match = '**bad**'
notify = true
"#,
        );

        let err = CompiledPolicy::compile(&cfg).unwrap_err();
        assert_eq!(err.rule_type, "yield_to_operator");
    }

    // -- Test 4: Empty policy → empty rules (not error) --

    #[test]
    fn test_empty_policy_is_ok() {
        let cfg = config_from_toml("");

        let compiled = CompiledPolicy::compile(&cfg).expect("empty should compile");
        assert!(compiled.auto_confirm.is_empty());
        assert!(compiled.yield_to_operator.is_empty());
        assert!(compiled.forbidden.is_empty());
    }

    // -- Test 5: Error reports correct index for second rule --

    #[test]
    fn test_error_index_for_second_rule() {
        let cfg = config_from_toml(
            r#"
[[policy.auto_confirm]]
match = 'valid'
respond = "y\n"

[[policy.auto_confirm]]
match = '[bad('
respond = "n\n"
"#,
        );

        let err = CompiledPolicy::compile(&cfg).unwrap_err();
        assert_eq!(err.rule_type, "auto_confirm");
        assert_eq!(err.index, 1);
        assert_eq!(err.pattern, "[bad(");
    }

    // -- Test 6: Handoff defaults when omitted --

    #[test]
    fn test_handoff_defaults() {
        let cfg = config_from_toml("");

        let compiled = CompiledPolicy::compile(&cfg).expect("should compile");
        assert_eq!(compiled.handoff.timeout_sec, 900);
        assert_eq!(compiled.handoff.fallback, "yield_to_llm");
    }
}
