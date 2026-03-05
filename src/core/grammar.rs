//! Grammar loading and matching.
//!
//! Loads TOML grammars, detects tools from command args, resolves actions.
//! Uses intermediate "raw" serde structs for TOML deserialization, then
//! converts to final types with compiled Regex fields.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use regex::Regex;
use serde::Deserialize;

use crate::router::categories::Category;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum GrammarError {
    Io(std::io::Error),
    Parse(toml::de::Error),
    InvalidRegex { pattern: String, source: regex::Error },
    InvalidAction(String),
    InvalidSeverity(String),
    InvalidCategory(String),
}

impl fmt::Display for GrammarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GrammarError::Io(e) => write!(f, "IO error: {e}"),
            GrammarError::Parse(e) => write!(f, "TOML parse error: {e}"),
            GrammarError::InvalidRegex { pattern, source } => {
                write!(f, "invalid regex '{pattern}': {source}")
            }
            GrammarError::InvalidAction(a) => write!(f, "invalid rule action: {a}"),
            GrammarError::InvalidSeverity(s) => write!(f, "invalid severity: {s}"),
            GrammarError::InvalidCategory(c) => write!(f, "invalid category: {c}"),
        }
    }
}

impl std::error::Error for GrammarError {}

impl From<std::io::Error> for GrammarError {
    fn from(e: std::io::Error) -> Self {
        GrammarError::Io(e)
    }
}

impl From<toml::de::Error> for GrammarError {
    fn from(e: toml::de::Error) -> Self {
        GrammarError::Parse(e)
    }
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Grammar {
    pub tool: ToolInfo,
    pub detect: Vec<String>,
    pub inherit: Vec<String>,
    pub global_noise: Vec<Rule>,
    pub actions: HashMap<String, Action>,
    pub fallback: Option<Action>,
    pub quiet: Option<QuietConfig>,
    pub verbosity: Option<VerbosityConfig>,
    pub enrich: Option<EnrichConfig>,
    pub category: Option<Category>,
    pub block: Vec<BlockRule>,
    pub llm_hints: Vec<LlmHint>,
}

#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct Action {
    pub detect: Vec<String>,
    pub noise: Vec<Rule>,
    pub hazard: Vec<Rule>,
    pub outcome: Vec<Rule>,
    pub summary: SummaryTemplate,
    pub llm_hints: Vec<LlmHint>,
    pub category: Option<Category>,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub pattern: Regex,
    pub action: RuleAction,
    pub severity: Option<Severity>,
    pub captures: Vec<String>,
    pub multiline: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleAction {
    Strip,
    Dedup,
    Keep,
    Promote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Default)]
pub struct SummaryTemplate {
    pub success: String,
    pub failure: String,
    pub partial: String,
}

#[derive(Debug, Clone)]
pub struct QuietConfig {
    pub safe_inject: Vec<String>,
    pub recommend: Vec<String>,
    pub actions: HashMap<String, QuietActionConfig>,
}

#[derive(Debug, Clone)]
pub struct QuietActionConfig {
    pub safe_inject: Vec<String>,
    pub recommend: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct VerbosityConfig {
    pub inject: Vec<String>,
    pub provides: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct EnrichConfig {
    pub on_failure: Vec<String>,
    pub args: Option<EnrichArgMapping>,
    pub actions: HashMap<String, EnrichActionConfig>,
}

#[derive(Debug, Clone)]
pub struct EnrichArgMapping {
    pub path_args: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct EnrichActionConfig {
    pub on_failure: Vec<String>,
}

/// An LLM hint declaring a preferred invocation form for a tool or action.
#[derive(Debug, Clone)]
pub struct LlmHint {
    pub prefer: String,
    pub reason: String,
    /// Optional mode filter: "mcp", "cli", or None (emit in both modes).
    pub mode: Option<String>,
}

/// A block compression rule for collapsing multi-line diagnostic blocks
/// (e.g., rustc warnings/errors) into single dense digest lines.
#[derive(Debug, Clone)]
pub struct BlockRule {
    pub start: Regex,
    pub end: Regex,
    pub extract: Regex,
    pub digest: String,
}

/// A captured outcome from evaluating rules against output lines.
#[derive(Debug, Clone)]
pub struct CapturedOutcome {
    pub captures: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Raw (serde) types — intermediate deserialization
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawGrammar {
    tool: RawToolInfo,
    #[serde(default)]
    detect: Option<Vec<String>>,
    #[serde(default)]
    inherit: Option<Vec<String>>,
    #[serde(default)]
    global_noise: Vec<RawRule>,
    #[serde(default)]
    actions: HashMap<String, RawAction>,
    #[serde(default)]
    fallback: Option<RawAction>,
    #[serde(default)]
    quiet: Option<RawQuietConfig>,
    #[serde(default)]
    verbosity: Option<RawVerbosityConfig>,
    #[serde(default)]
    enrich: Option<RawEnrichConfig>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    block: Vec<RawBlockRule>,
    #[serde(default)]
    llm_hints: Vec<RawLlmHint>,
}

#[derive(Deserialize)]
struct RawBlockRule {
    start: String,
    end: Option<String>,
    extract: String,
    digest: String,
}

#[derive(Deserialize)]
struct RawToolInfo {
    name: String,
    #[serde(default)]
    detect: Option<Vec<String>>,
    #[serde(default)]
    inherit: Option<Vec<String>>,
    #[serde(default)]
    category: Option<String>,
}

#[derive(Deserialize)]
struct RawRule {
    pattern: String,
    action: String,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    captures: Option<Vec<String>>,
    #[serde(default)]
    multiline: Option<u32>,
    // Allow an optional description field in TOML without failing
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
}

#[derive(Deserialize)]
struct RawLlmHint {
    prefer: String,
    reason: String,
    #[serde(default)]
    mode: Option<String>,
}

#[derive(Deserialize)]
struct RawAction {
    #[serde(default)]
    detect: Option<Vec<String>>,
    #[serde(default)]
    noise: Vec<RawRule>,
    #[serde(default)]
    hazard: Vec<RawRule>,
    #[serde(default)]
    outcome: Vec<RawRule>,
    #[serde(default)]
    summary: Option<RawSummaryTemplate>,
    #[serde(default)]
    llm_hints: Vec<RawLlmHint>,
    #[serde(default)]
    category: Option<String>,
}

#[derive(Deserialize)]
struct RawSummaryTemplate {
    #[serde(default)]
    success: Option<String>,
    #[serde(default)]
    failure: Option<String>,
    #[serde(default)]
    partial: Option<String>,
}

#[derive(Deserialize)]
struct RawQuietConfig {
    #[serde(default)]
    safe_inject: Vec<String>,
    #[serde(default)]
    recommend: Vec<String>,
    #[serde(default)]
    actions: HashMap<String, RawQuietActionConfig>,
}

#[derive(Deserialize)]
struct RawQuietActionConfig {
    #[serde(default)]
    safe_inject: Vec<String>,
    #[serde(default)]
    recommend: Vec<String>,
}

#[derive(Deserialize)]
struct RawVerbosityConfig {
    #[serde(default)]
    inject: Vec<String>,
    #[serde(default)]
    provides: Vec<String>,
}

#[derive(Deserialize)]
struct RawEnrichConfig {
    #[serde(default)]
    on_failure: Vec<String>,
    #[serde(default)]
    args: Option<RawEnrichArgMapping>,
    #[serde(default)]
    actions: HashMap<String, RawEnrichActionConfig>,
}

#[derive(Deserialize)]
struct RawEnrichArgMapping {
    #[serde(default)]
    path_args: Vec<usize>,
}

#[derive(Deserialize)]
struct RawEnrichActionConfig {
    #[serde(default)]
    on_failure: Vec<String>,
}

// Shared grammar TOML uses `[[rules]]` at the top level
#[derive(Deserialize)]
struct RawSharedGrammar {
    #[serde(default)]
    rules: Vec<RawRule>,
}

// ---------------------------------------------------------------------------
// Conversions: Raw -> compiled types
// ---------------------------------------------------------------------------

fn parse_rule_action(s: &str) -> Result<RuleAction, GrammarError> {
    match s {
        "strip" => Ok(RuleAction::Strip),
        "dedup" => Ok(RuleAction::Dedup),
        "keep" => Ok(RuleAction::Keep),
        "promote" => Ok(RuleAction::Promote),
        other => Err(GrammarError::InvalidAction(other.to_string())),
    }
}

fn parse_severity(s: &str) -> Result<Severity, GrammarError> {
    match s {
        "error" => Ok(Severity::Error),
        "warning" => Ok(Severity::Warning),
        other => Err(GrammarError::InvalidSeverity(other.to_string())),
    }
}

fn parse_category(s: &str) -> Result<Category, GrammarError> {
    match s {
        "condense" => Ok(Category::Condense),
        "narrate" => Ok(Category::Narrate),
        "passthrough" => Ok(Category::Passthrough),
        "structured" => Ok(Category::Structured),
        "interactive" => Ok(Category::Interactive),
        "dangerous" => Ok(Category::Dangerous),
        other => Err(GrammarError::InvalidCategory(other.to_string())),
    }
}

impl TryFrom<RawRule> for Rule {
    type Error = GrammarError;

    fn try_from(raw: RawRule) -> Result<Self, GrammarError> {
        let pattern = Regex::new(&raw.pattern).map_err(|e| GrammarError::InvalidRegex {
            pattern: raw.pattern.clone(),
            source: e,
        })?;
        let action = parse_rule_action(&raw.action)?;
        let severity = raw.severity.as_deref().map(parse_severity).transpose()?;
        let captures = raw.captures.unwrap_or_default();
        Ok(Rule {
            pattern,
            action,
            severity,
            captures,
            multiline: raw.multiline,
        })
    }
}

impl TryFrom<RawBlockRule> for BlockRule {
    type Error = GrammarError;

    fn try_from(raw: RawBlockRule) -> Result<Self, GrammarError> {
        let start = Regex::new(&raw.start).map_err(|e| GrammarError::InvalidRegex {
            pattern: raw.start.clone(),
            source: e,
        })?;
        let end_pattern = raw.end.as_deref().unwrap_or(r"^\s*$");
        let end = Regex::new(end_pattern).map_err(|e| GrammarError::InvalidRegex {
            pattern: end_pattern.to_string(),
            source: e,
        })?;
        // Build multiline regex with (?s) flag for dot-matches-newline
        let extract_pattern = format!("(?s){}", raw.extract);
        let extract = Regex::new(&extract_pattern).map_err(|e| GrammarError::InvalidRegex {
            pattern: raw.extract.clone(),
            source: e,
        })?;
        Ok(BlockRule {
            start,
            end,
            extract,
            digest: raw.digest,
        })
    }
}

impl TryFrom<RawAction> for Action {
    type Error = GrammarError;

    fn try_from(raw: RawAction) -> Result<Self, GrammarError> {
        let detect = raw.detect.unwrap_or_default();
        let noise = raw
            .noise
            .into_iter()
            .map(Rule::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let hazard = raw
            .hazard
            .into_iter()
            .map(Rule::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let outcome = raw
            .outcome
            .into_iter()
            .map(Rule::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let summary = match raw.summary {
            Some(s) => SummaryTemplate {
                success: s.success.unwrap_or_default(),
                failure: s.failure.unwrap_or_default(),
                partial: s.partial.unwrap_or_default(),
            },
            None => SummaryTemplate::default(),
        };
        let llm_hints = raw
            .llm_hints
            .into_iter()
            .map(|h| LlmHint { prefer: h.prefer, reason: h.reason, mode: h.mode })
            .collect();
        let category = raw.category.as_deref().map(parse_category).transpose()?;
        Ok(Action {
            detect,
            noise,
            hazard,
            outcome,
            summary,
            llm_hints,
            category,
        })
    }
}

fn convert_raw_grammar(raw: RawGrammar) -> Result<Grammar, GrammarError> {
    // category: prefer top-level, fall back to [tool] section
    let category_str = raw.category.or(raw.tool.category);
    let category = category_str.as_deref().map(parse_category).transpose()?;

    // detect list: prefer top-level, fall back to [tool] section, default to [tool.name]
    let detect = raw
        .detect
        .or(raw.tool.detect)
        .unwrap_or_else(|| vec![raw.tool.name.clone()]);

    // inherit: prefer top-level, fall back to [tool] section
    let inherit_list = raw.inherit.or(raw.tool.inherit).unwrap_or_default();

    let global_noise = raw
        .global_noise
        .into_iter()
        .map(Rule::try_from)
        .collect::<Result<Vec<_>, _>>()?;

    let actions = raw
        .actions
        .into_iter()
        .map(|(k, v)| Action::try_from(v).map(|a| (k, a)))
        .collect::<Result<HashMap<_, _>, _>>()?;

    let fallback = raw.fallback.map(Action::try_from).transpose()?;

    let quiet = raw.quiet.map(|q| QuietConfig {
        safe_inject: q.safe_inject,
        recommend: q.recommend,
        actions: q
            .actions
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    QuietActionConfig {
                        safe_inject: v.safe_inject,
                        recommend: v.recommend,
                    },
                )
            })
            .collect(),
    });

    let verbosity = raw.verbosity.map(|v| VerbosityConfig {
        inject: v.inject,
        provides: v.provides,
    });

    let enrich = raw.enrich.map(|e| EnrichConfig {
        on_failure: e.on_failure,
        args: e.args.map(|a| EnrichArgMapping {
            path_args: a.path_args,
        }),
        actions: e
            .actions
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    EnrichActionConfig {
                        on_failure: v.on_failure,
                    },
                )
            })
            .collect(),
    });

    let block = raw
        .block
        .into_iter()
        .map(BlockRule::try_from)
        .collect::<Result<Vec<_>, _>>()?;

    let llm_hints = raw
        .llm_hints
        .into_iter()
        .map(|h| LlmHint { prefer: h.prefer, reason: h.reason, mode: h.mode })
        .collect();

    Ok(Grammar {
        tool: ToolInfo {
            name: raw.tool.name,
        },
        detect,
        inherit: inherit_list,
        global_noise,
        actions,
        fallback,
        quiet,
        verbosity,
        enrich,
        category,
        block,
        llm_hints,
    })
}

// ---------------------------------------------------------------------------
// Public API — loading
// ---------------------------------------------------------------------------

/// Load a single grammar from a TOML file.
pub fn load_grammar(path: &Path) -> Result<Grammar, GrammarError> {
    let contents = std::fs::read_to_string(path)?;
    load_grammar_from_str(&contents)
}

/// Parse a grammar from a TOML string.
pub fn load_grammar_from_str(toml_str: &str) -> Result<Grammar, GrammarError> {
    let raw: RawGrammar = toml::from_str(toml_str)?;
    convert_raw_grammar(raw)
}

/// Load shared grammar rules from a `_shared/*.toml` file.
/// These files have `[[rules]]` at the top level (no [tool] section).
fn load_shared_rules(path: &Path) -> Result<Vec<Rule>, GrammarError> {
    let contents = std::fs::read_to_string(path)?;
    load_shared_rules_from_str(&contents)
}

fn load_shared_rules_from_str(toml_str: &str) -> Result<Vec<Rule>, GrammarError> {
    let raw: RawSharedGrammar = toml::from_str(toml_str)?;
    raw.rules
        .into_iter()
        .map(Rule::try_from)
        .collect::<Result<Vec<_>, _>>()
}

/// Load all tool grammars from a grammars directory.
///
/// Skips `_meta/` (config files) and `_shared/` (inherited rule fragments).
/// After loading, resolves `inherit` references by prepending shared rules
/// into each grammar's `global_noise`.
pub fn load_all_grammars(
    grammars_dir: &Path,
) -> Result<HashMap<String, Grammar>, GrammarError> {
    let mut grammars = HashMap::new();

    // First pass: load shared rules
    let shared_dir = grammars_dir.join("_shared");
    let mut shared_rules: HashMap<String, Vec<Rule>> = HashMap::new();

    if shared_dir.is_dir() {
        for entry in std::fs::read_dir(&shared_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                let rules = load_shared_rules(&path)?;
                shared_rules.insert(stem, rules);
            }
        }
    }

    // Second pass: load tool grammars (everything not in _meta/ or _shared/)
    if grammars_dir.is_dir() {
        for entry in std::fs::read_dir(grammars_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                let dir_name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                // Skip meta and shared directories
                if dir_name.starts_with('_') {
                    continue;
                }
                // Recurse into subdirectories for tool grammars
                for sub_entry in std::fs::read_dir(&path)? {
                    let sub_entry = sub_entry?;
                    let sub_path = sub_entry.path();
                    if sub_path.extension().and_then(|e| e.to_str()) == Some("toml") {
                        let mut grammar = load_grammar(&sub_path)?;
                        resolve_inherit(&mut grammar, &shared_rules);
                        grammars.insert(grammar.tool.name.clone(), grammar);
                    }
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                let mut grammar = load_grammar(&path)?;
                resolve_inherit(&mut grammar, &shared_rules);
                grammars.insert(grammar.tool.name.clone(), grammar);
            }
        }
    }

    Ok(grammars)
}

/// Resolve `inherit` references by appending shared rules into `global_noise`.
///
/// Inherited rules are evaluated **after** the tool's own rules. This allows
/// a tool grammar to override shared behavior when needed.
fn resolve_inherit(grammar: &mut Grammar, shared_rules: &HashMap<String, Vec<Rule>>) {
    for name in &grammar.inherit {
        if let Some(rules) = shared_rules.get(name) {
            grammar.global_noise.extend(rules.iter().cloned());
        }
    }
}

// ---------------------------------------------------------------------------
// Public API — tool detection and action resolution
// ---------------------------------------------------------------------------

/// Detect which grammar matches the given command arguments.
///
/// Returns the matching grammar and optionally the resolved action.
/// Matches against the `detect` list in each grammar.
pub fn detect_tool<'a>(
    args: &[String],
    grammars: &'a HashMap<String, Grammar>,
) -> Option<(&'a Grammar, Option<&'a Action>)> {
    if args.is_empty() {
        return None;
    }

    let cmd = &args[0];

    // Check each grammar's detect list
    for grammar in grammars.values() {
        if grammar.detect.iter().any(|d| d == cmd) {
            let action = resolve_action(grammar, args);
            return Some((grammar, action));
        }
    }

    None
}

/// Resolve which action within a grammar matches the command arguments.
///
/// Walks the args (skipping the command name at index 0) and checks each
/// action's detect list for a match.
pub fn resolve_action<'a>(grammar: &'a Grammar, args: &[String]) -> Option<&'a Action> {
    if args.len() < 2 {
        return grammar.fallback.as_ref();
    }

    // Check each action's detect list against args[1..]
    for action in grammar.actions.values() {
        for arg in &args[1..] {
            if action.detect.iter().any(|d| d == arg) {
                return Some(action);
            }
        }
    }

    // No action matched — use fallback if available
    grammar.fallback.as_ref()
}

// ---------------------------------------------------------------------------
// Public API — summary formatting
// ---------------------------------------------------------------------------

/// Format a summary line from a grammar's summary template and captured outcomes.
///
/// Substitutes `{variable}` placeholders with values from captured outcomes.
/// Also substitutes `{exit_code}`, `{lines}`, etc.
pub fn format_summary(
    grammar: &Grammar,
    action: Option<&Action>,
    outcomes: &[CapturedOutcome],
    exit_code: i32,
) -> Vec<String> {
    let template = match action {
        Some(a) => &a.summary,
        None => match &grammar.fallback {
            Some(fb) => &fb.summary,
            None => return Vec::new(),
        },
    };

    let template_str = if exit_code == 0 {
        &template.success
    } else {
        &template.failure
    };

    if template_str.is_empty() {
        return Vec::new();
    }

    // Build a combined capture map from all outcomes
    let mut vars: HashMap<&str, &str> = HashMap::new();
    for outcome in outcomes {
        for (k, v) in &outcome.captures {
            vars.insert(k.as_str(), v.as_str());
        }
    }

    // Substitute template variables
    let mut result = template_str.clone();

    // Substitute {exit_code}
    result = result.replace("{exit_code}", &exit_code.to_string());

    // Substitute captured variables
    for (key, value) in &vars {
        let placeholder = format!("{{{key}}}");
        result = result.replace(&placeholder, value);
    }

    vec![result]
}

// ---------------------------------------------------------------------------
// Public API — rule evaluation
// ---------------------------------------------------------------------------

/// The result of evaluating a line against grammar rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleMatch {
    /// Line matched a hazard rule.
    Hazard {
        action: RuleAction,
        severity: Option<Severity>,
        captures: HashMap<String, String>,
        multiline: Option<u32>,
    },
    /// Line matched an outcome rule.
    Outcome {
        action: RuleAction,
        captures: HashMap<String, String>,
        multiline: Option<u32>,
    },
    /// Line matched a noise rule (action-specific or global).
    Noise {
        action: RuleAction,
        multiline: Option<u32>,
    },
    /// No rule matched this line.
    NoMatch,
}

/// Try to match a single line against a single rule.
/// Returns the extracted captures if the rule matches.
fn try_match_rule(rule: &Rule, line: &str) -> Option<HashMap<String, String>> {
    let caps = rule.pattern.captures(line)?;
    let mut captured = HashMap::new();
    for name in &rule.captures {
        if let Some(m) = caps.name(name) {
            captured.insert(name.clone(), m.as_str().to_string());
        }
    }
    Some(captured)
}

/// Evaluate a line against a grammar action's rules in the specified order:
///
/// 1. **Hazard rules** (action-specific) -- never suppress an error
/// 2. **Outcome rules** (action-specific) -- extract summary-worthy info
/// 3. **Noise rules** (action-specific, then global_noise) -- strip or dedup
///
/// First match wins. If no rule matches, returns `RuleMatch::NoMatch`.
pub fn evaluate_line(grammar: &Grammar, action: Option<&Action>, line: &str) -> RuleMatch {
    if let Some(act) = action {
        // Step 1: Hazard rules (highest priority)
        for rule in &act.hazard {
            if let Some(captures) = try_match_rule(rule, line) {
                return RuleMatch::Hazard {
                    action: rule.action,
                    severity: rule.severity,
                    captures,
                    multiline: rule.multiline,
                };
            }
        }

        // Step 2: Outcome rules
        for rule in &act.outcome {
            if let Some(captures) = try_match_rule(rule, line) {
                return RuleMatch::Outcome {
                    action: rule.action,
                    captures,
                    multiline: rule.multiline,
                };
            }
        }

        // Step 3: Action-specific noise rules
        for rule in &act.noise {
            if rule.pattern.is_match(line) {
                return RuleMatch::Noise {
                    action: rule.action,
                    multiline: rule.multiline,
                };
            }
        }
    }

    // Step 4: Global noise rules (includes inherited rules, which come last)
    for rule in &grammar.global_noise {
        if rule.pattern.is_match(line) {
            return RuleMatch::Noise {
                action: rule.action,
                multiline: rule.multiline,
            };
        }
    }

    RuleMatch::NoMatch
}

/// Evaluate a line against a grammar's fallback action rules.
///
/// Convenience wrapper that uses the fallback action for grammars
/// like `make` that don't have named actions.
pub fn evaluate_line_with_fallback(grammar: &Grammar, line: &str) -> RuleMatch {
    evaluate_line(grammar, grammar.fallback.as_ref(), line)
}

// ---------------------------------------------------------------------------
// Public API — category resolution (grammar-level)
// ---------------------------------------------------------------------------

/// Resolve the category for a grammar.
///
/// Resolution order (first match wins):
/// 1. Grammar front matter `category` field
/// 2. categories.toml mapping (caller provides)
/// 3. Default to Condense
pub fn resolve_category(
    grammar: &Grammar,
    categories_map: Option<&HashMap<String, Category>>,
) -> Category {
    // Step 1: Grammar front matter
    if let Some(cat) = grammar.category {
        return cat;
    }

    // Step 2: categories.toml mapping (look up the grammar's tool name)
    if let Some(map) = categories_map {
        if let Some(&cat) = map.get(&grammar.tool.name) {
            return cat;
        }
    }

    // Step 3: Default
    Category::default()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Test 1: Parse a minimal grammar TOML (just [tool] section)
    #[test]
    fn test_parse_minimal_grammar() {
        let toml_str = r#"
[tool]
name = "echo"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        assert_eq!(grammar.tool.name, "echo");
        assert_eq!(grammar.detect, vec!["echo"]);
        assert!(grammar.global_noise.is_empty());
        assert!(grammar.actions.is_empty());
        assert!(grammar.fallback.is_none());
        assert!(grammar.category.is_none());
    }

    // Test 2: Parse grammar with global_noise rules
    #[test]
    fn test_parse_grammar_with_global_noise() {
        let toml_str = r#"
[tool]
name = "npm"

[[global_noise]]
pattern = '^npm (timing|http|sill|verb)'
action = "strip"

[[global_noise]]
pattern = '^npm warn'
action = "dedup"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        assert_eq!(grammar.global_noise.len(), 2);
        assert_eq!(grammar.global_noise[0].action, RuleAction::Strip);
        assert_eq!(grammar.global_noise[1].action, RuleAction::Dedup);
    }

    // Test 3: Parse grammar with actions and all rule types
    #[test]
    fn test_parse_grammar_with_actions_all_rule_types() {
        let toml_str = r#"
[tool]
name = "npm"
detect = ["npm", "npx"]

[actions.install]
detect = ["install", "i", "add", "ci"]

[[actions.install.noise]]
pattern = '^(idealTree|reify|resolv)'
action = "strip"

[[actions.install.hazard]]
pattern = 'ERESOLVE'
severity = "error"
action = "keep"

[[actions.install.outcome]]
pattern = '^added (?P<count>\d+) packages? in (?P<time>.+)'
action = "promote"
captures = ["count", "time"]

[actions.install.summary]
success = "+ {count} packages installed ({time})"
failure = "! npm install failed (exit {exit_code})"
partial = "... installing ({lines} lines)"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        assert_eq!(grammar.detect, vec!["npm", "npx"]);

        let install = grammar.actions.get("install").unwrap();
        assert_eq!(install.detect, vec!["install", "i", "add", "ci"]);
        assert_eq!(install.noise.len(), 1);
        assert_eq!(install.hazard.len(), 1);
        assert_eq!(install.hazard[0].severity, Some(Severity::Error));
        assert_eq!(install.outcome.len(), 1);
        assert_eq!(install.outcome[0].action, RuleAction::Promote);
        assert_eq!(install.outcome[0].captures, vec!["count", "time"]);
    }

    // Test 4: Parse grammar with summary templates
    #[test]
    fn test_parse_grammar_with_summary_templates() {
        let toml_str = r#"
[tool]
name = "cargo"

[actions.build]
detect = ["build"]

[actions.build.summary]
success = "+ build succeeded"
failure = "! build failed (exit {exit_code})"
partial = "... compiling"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let build = grammar.actions.get("build").unwrap();
        assert_eq!(build.summary.success, "+ build succeeded");
        assert_eq!(
            build.summary.failure,
            "! build failed (exit {exit_code})"
        );
        assert_eq!(build.summary.partial, "... compiling");
    }

    // Test 5: Parse grammar with inherit list
    #[test]
    fn test_parse_grammar_with_inherit() {
        let toml_str = r#"
[tool]
name = "npm"
inherit = ["ansi-progress", "node-stacktrace"]
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        assert_eq!(
            grammar.inherit,
            vec!["ansi-progress", "node-stacktrace"]
        );
    }

    // Test 6: Parse grammar with quiet config
    #[test]
    fn test_parse_grammar_with_quiet_config() {
        let toml_str = r#"
[tool]
name = "npm"

[quiet]
safe_inject = ["--loglevel=error"]
recommend = ["--silent"]

[quiet.actions.install]
safe_inject = ["--no-fund", "--no-audit"]
recommend = ["--prefer-offline"]
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let quiet = grammar.quiet.unwrap();
        assert_eq!(quiet.safe_inject, vec!["--loglevel=error"]);
        assert_eq!(quiet.recommend, vec!["--silent"]);
        let install = quiet.actions.get("install").unwrap();
        assert_eq!(install.safe_inject, vec!["--no-fund", "--no-audit"]);
        assert_eq!(install.recommend, vec!["--prefer-offline"]);
    }

    // Test 7: Parse grammar with verbosity config
    #[test]
    fn test_parse_grammar_with_verbosity_config() {
        let toml_str = r#"
[tool]
name = "ls"

[verbosity]
inject = ["-la"]
provides = ["permissions", "size", "owner"]
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let verbosity = grammar.verbosity.unwrap();
        assert_eq!(verbosity.inject, vec!["-la"]);
        assert_eq!(
            verbosity.provides,
            vec!["permissions", "size", "owner"]
        );
    }

    // Test 8: Parse grammar with enrich config
    #[test]
    fn test_parse_grammar_with_enrich_config() {
        let toml_str = r#"
[tool]
name = "cp"

[enrich]
on_failure = ["stat {src}", "stat {dst}"]

[enrich.args]
path_args = [0, 1]

[enrich.actions.recursive]
on_failure = ["ls -la {src}"]
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let enrich = grammar.enrich.unwrap();
        assert_eq!(
            enrich.on_failure,
            vec!["stat {src}", "stat {dst}"]
        );
        let args = enrich.args.unwrap();
        assert_eq!(args.path_args, vec![0, 1]);
        let recursive = enrich.actions.get("recursive").unwrap();
        assert_eq!(recursive.on_failure, vec!["ls -la {src}"]);
    }

    // Test 9: Parse grammar with fallback action
    #[test]
    fn test_parse_grammar_with_fallback() {
        let toml_str = r#"
[tool]
name = "make"

[fallback]
detect = []

[[fallback.noise]]
pattern = '^make\[\d+\]:'
action = "strip"

[fallback.summary]
success = "+ make succeeded"
failure = "! make failed"
partial = "... building"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let fallback = grammar.fallback.unwrap();
        assert_eq!(fallback.noise.len(), 1);
        assert_eq!(fallback.summary.success, "+ make succeeded");
    }

    // Test 10: detect_tool matches "npm install" -> npm grammar, install action
    #[test]
    fn test_detect_tool_npm_install() {
        let toml_str = r#"
[tool]
name = "npm"
detect = ["npm", "npx"]

[actions.install]
detect = ["install", "i", "add", "ci"]

[actions.install.summary]
success = "+ installed"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut grammars = HashMap::new();
        grammars.insert("npm".to_string(), grammar);

        let args: Vec<String> = vec!["npm", "install"]
            .into_iter()
            .map(String::from)
            .collect();
        let result = detect_tool(&args, &grammars);
        assert!(result.is_some());
        let (g, a) = result.unwrap();
        assert_eq!(g.tool.name, "npm");
        assert!(a.is_some());
        let action = a.unwrap();
        assert_eq!(action.detect, vec!["install", "i", "add", "ci"]);
    }

    // Test 11: detect_tool matches "cargo build" -> cargo grammar, build action
    #[test]
    fn test_detect_tool_cargo_build() {
        let toml_str = r#"
[tool]
name = "cargo"

[actions.build]
detect = ["build", "b"]

[actions.build.summary]
success = "+ build ok"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut grammars = HashMap::new();
        grammars.insert("cargo".to_string(), grammar);

        let args: Vec<String> = vec!["cargo", "build"]
            .into_iter()
            .map(String::from)
            .collect();
        let result = detect_tool(&args, &grammars);
        assert!(result.is_some());
        let (g, a) = result.unwrap();
        assert_eq!(g.tool.name, "cargo");
        assert!(a.is_some());
    }

    // Test 12: detect_tool returns None for unknown command
    #[test]
    fn test_detect_tool_unknown_command() {
        let toml_str = r#"
[tool]
name = "npm"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut grammars = HashMap::new();
        grammars.insert("npm".to_string(), grammar);

        let args: Vec<String> = vec!["curl", "https://example.com"]
            .into_iter()
            .map(String::from)
            .collect();
        let result = detect_tool(&args, &grammars);
        assert!(result.is_none());
    }

    // Test 13: resolve_action finds correct action from args
    #[test]
    fn test_resolve_action_finds_correct_action() {
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
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let args: Vec<String> = vec!["npm", "test"]
            .into_iter()
            .map(String::from)
            .collect();
        let action = resolve_action(&grammar, &args);
        assert!(action.is_some());
        assert_eq!(action.unwrap().summary.success, "tested");
    }

    // Test 14: resolve_action returns fallback when no action matches
    #[test]
    fn test_resolve_action_returns_fallback() {
        let toml_str = r#"
[tool]
name = "make"

[actions.build]
detect = ["build"]

[actions.build.summary]
success = "built"

[fallback]

[fallback.summary]
success = "make fallback"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let args: Vec<String> = vec!["make", "clean"]
            .into_iter()
            .map(String::from)
            .collect();
        let action = resolve_action(&grammar, &args);
        assert!(action.is_some());
        assert_eq!(action.unwrap().summary.success, "make fallback");
    }

    // Test 21: format_summary substitutes template variables correctly
    #[test]
    fn test_format_summary_substitutes_variables() {
        let toml_str = r#"
[tool]
name = "npm"

[actions.install]
detect = ["install"]

[[actions.install.outcome]]
pattern = '^added (?P<count>\d+) packages? in (?P<time>.+)'
action = "promote"
captures = ["count", "time"]

[actions.install.summary]
success = "+ {count} packages installed ({time})"
failure = "! npm install failed (exit {exit_code})"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let action = grammar.actions.get("install").unwrap();

        let outcomes = vec![CapturedOutcome {
            captures: HashMap::from([
                ("count".to_string(), "42".to_string()),
                ("time".to_string(), "3.2s".to_string()),
            ]),
        }];

        let result = format_summary(&grammar, Some(action), &outcomes, 0);
        assert_eq!(result, vec!["+ 42 packages installed (3.2s)"]);
    }

    #[test]
    fn test_format_summary_failure_template() {
        let toml_str = r#"
[tool]
name = "npm"

[actions.install]
detect = ["install"]

[actions.install.summary]
success = "+ installed"
failure = "! npm install failed (exit {exit_code})"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let action = grammar.actions.get("install").unwrap();

        let result = format_summary(&grammar, Some(action), &[], 1);
        assert_eq!(result, vec!["! npm install failed (exit 1)"]);
    }

    // Test: invalid regex pattern produces error
    #[test]
    fn test_invalid_regex_produces_error() {
        let toml_str = r#"
[tool]
name = "bad"

[[global_noise]]
pattern = '[invalid'
action = "strip"
"#;
        let result = load_grammar_from_str(toml_str);
        assert!(result.is_err());
        match result.unwrap_err() {
            GrammarError::InvalidRegex { pattern, .. } => {
                assert_eq!(pattern, "[invalid");
            }
            other => panic!("expected InvalidRegex, got: {other}"),
        }
    }

    // Test: grammar with category field
    #[test]
    fn test_parse_grammar_with_category() {
        let toml_str = r#"
[tool]
name = "vim"
category = "interactive"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        assert_eq!(grammar.category, Some(Category::Interactive));
    }

    // Test: detect via tool.detect list
    #[test]
    fn test_detect_tool_via_detect_list() {
        let toml_str = r#"
[tool]
name = "npm"
detect = ["npm", "npx"]
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut grammars = HashMap::new();
        grammars.insert("npm".to_string(), grammar);

        let args: Vec<String> = vec!["npx", "create-react-app"]
            .into_iter()
            .map(String::from)
            .collect();
        let result = detect_tool(&args, &grammars);
        assert!(result.is_some());
        assert_eq!(result.unwrap().0.tool.name, "npm");
    }

    // Test: empty args returns None
    #[test]
    fn test_detect_tool_empty_args() {
        let grammars = HashMap::new();
        let args: Vec<String> = vec![];
        let result = detect_tool(&args, &grammars);
        assert!(result.is_none());
    }

    // Test: resolve_action with only command (no subcommand) and no fallback
    #[test]
    fn test_resolve_action_no_subcommand_no_fallback() {
        let toml_str = r#"
[tool]
name = "npm"

[actions.install]
detect = ["install"]

[actions.install.summary]
success = "installed"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let args: Vec<String> = vec!["npm"].into_iter().map(String::from).collect();
        let action = resolve_action(&grammar, &args);
        assert!(action.is_none());
    }

    // Test: shared grammar loading
    #[test]
    fn test_load_shared_rules() {
        let toml_str = r#"
[[rules]]
pattern = '^\s*\d+/\d+\s'
action = "strip"
description = "Counter-style progress"

[[rules]]
pattern = '^\s*\[?[#=\->.]+\]?\s*\d+%'
action = "strip"
description = "Progress bar with percentage"
"#;
        let rules = load_shared_rules_from_str(toml_str).unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].action, RuleAction::Strip);
        assert_eq!(rules[1].action, RuleAction::Strip);
    }

    // -----------------------------------------------------------------------
    // New tests: evaluate_line
    // -----------------------------------------------------------------------

    /// Build a grammar with hazard, outcome, and noise rules for testing evaluate_line.
    fn build_eval_grammar() -> Grammar {
        let toml_str = r#"
[tool]
name = "cargo"

[[global_noise]]
pattern = '^\s+Compiling'
action = "strip"

[actions.build]
detect = ["build"]

[[actions.build.hazard]]
pattern = '^error\[(?P<code>E\d+)\]'
severity = "error"
action = "keep"
captures = ["code"]
multiline = 3

[[actions.build.hazard]]
pattern = '^warning:'
severity = "warning"
action = "dedup"

[[actions.build.outcome]]
pattern = '^\s+Finished .* in (?P<time>.+)'
action = "promote"
captures = ["time"]

[[actions.build.noise]]
pattern = '^\s+Fresh'
action = "strip"

[actions.build.summary]
success = "+ built in {time}"
failure = "! build failed"
"#;
        load_grammar_from_str(toml_str).unwrap()
    }

    #[test]
    fn test_evaluate_line_hazard_match() {
        let grammar = build_eval_grammar();
        let action = grammar.actions.get("build").unwrap();

        let result = evaluate_line(&grammar, Some(action), "error[E0433]: some error");
        match result {
            RuleMatch::Hazard { action, severity, captures, multiline } => {
                assert_eq!(action, RuleAction::Keep);
                assert_eq!(severity, Some(Severity::Error));
                assert_eq!(captures.get("code").map(|s| s.as_str()), Some("E0433"));
                assert_eq!(multiline, Some(3));
            }
            other => panic!("expected Hazard, got: {:?}", other),
        }
    }

    #[test]
    fn test_evaluate_line_outcome_match() {
        let grammar = build_eval_grammar();
        let action = grammar.actions.get("build").unwrap();

        let result = evaluate_line(&grammar, Some(action), "   Finished `dev` profile in 5.2s");
        match result {
            RuleMatch::Outcome { action, captures, .. } => {
                assert_eq!(action, RuleAction::Promote);
                assert_eq!(captures.get("time").map(|s| s.as_str()), Some("5.2s"));
            }
            other => panic!("expected Outcome, got: {:?}", other),
        }
    }

    #[test]
    fn test_evaluate_line_action_noise_match() {
        let grammar = build_eval_grammar();
        let action = grammar.actions.get("build").unwrap();

        let result = evaluate_line(&grammar, Some(action), "   Fresh serde v1.0.0");
        match result {
            RuleMatch::Noise { action, .. } => {
                assert_eq!(action, RuleAction::Strip);
            }
            other => panic!("expected Noise, got: {:?}", other),
        }
    }

    #[test]
    fn test_evaluate_line_global_noise_match() {
        let grammar = build_eval_grammar();
        let action = grammar.actions.get("build").unwrap();

        let result = evaluate_line(&grammar, Some(action), "   Compiling serde v1.0.0");
        match result {
            RuleMatch::Noise { action, .. } => {
                assert_eq!(action, RuleAction::Strip);
            }
            other => panic!("expected Noise (global), got: {:?}", other),
        }
    }

    #[test]
    fn test_evaluate_line_no_match() {
        let grammar = build_eval_grammar();
        let action = grammar.actions.get("build").unwrap();

        let result = evaluate_line(&grammar, Some(action), "some random output line");
        assert_eq!(result, RuleMatch::NoMatch);
    }

    // -----------------------------------------------------------------------
    // Rule evaluation order: hazard > outcome > noise
    // -----------------------------------------------------------------------

    #[test]
    fn test_evaluate_line_hazard_takes_priority_over_outcome() {
        // Create a grammar where a line matches BOTH a hazard and an outcome rule.
        // Hazard should win.
        let toml_str = r#"
[tool]
name = "test-tool"

[actions.check]
detect = ["check"]

[[actions.check.hazard]]
pattern = 'CRITICAL'
severity = "error"
action = "keep"

[[actions.check.outcome]]
pattern = 'CRITICAL'
action = "promote"

[actions.check.summary]
success = "ok"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let action = grammar.actions.get("check").unwrap();

        let result = evaluate_line(&grammar, Some(action), "CRITICAL: something failed");
        match result {
            RuleMatch::Hazard { .. } => { /* expected */ }
            other => panic!("expected Hazard (priority over Outcome), got: {:?}", other),
        }
    }

    #[test]
    fn test_evaluate_line_hazard_takes_priority_over_noise() {
        // A line that matches both hazard and noise should be classified as hazard.
        let toml_str = r#"
[tool]
name = "test-tool"

[actions.run]
detect = ["run"]

[[actions.run.hazard]]
pattern = '^error:'
severity = "error"
action = "keep"

[[actions.run.noise]]
pattern = '^error:'
action = "strip"

[actions.run.summary]
success = "ok"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let action = grammar.actions.get("run").unwrap();

        let result = evaluate_line(&grammar, Some(action), "error: something went wrong");
        match result {
            RuleMatch::Hazard { severity, .. } => {
                assert_eq!(severity, Some(Severity::Error));
            }
            other => panic!("expected Hazard (priority over Noise), got: {:?}", other),
        }
    }

    #[test]
    fn test_evaluate_line_outcome_takes_priority_over_noise() {
        // A line that matches both outcome and noise should be classified as outcome.
        let toml_str = r#"
[tool]
name = "test-tool"

[actions.run]
detect = ["run"]

[[actions.run.outcome]]
pattern = '^done in'
action = "promote"

[[actions.run.noise]]
pattern = '^done'
action = "strip"

[actions.run.summary]
success = "ok"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let action = grammar.actions.get("run").unwrap();

        let result = evaluate_line(&grammar, Some(action), "done in 3s");
        match result {
            RuleMatch::Outcome { action, .. } => {
                assert_eq!(action, RuleAction::Promote);
            }
            other => panic!("expected Outcome (priority over Noise), got: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Inheritance: inherited rules appended (evaluated last)
    // -----------------------------------------------------------------------

    #[test]
    fn test_inheritance_appends_shared_rules_after_own() {
        // Build two shared rule sets and a grammar that inherits them.
        // Verify inherited rules come AFTER the grammar's own global_noise.
        let shared_toml_a = r#"
[[rules]]
pattern = '^SHARED_A'
action = "strip"
"#;
        let shared_toml_b = r#"
[[rules]]
pattern = '^SHARED_B'
action = "strip"
"#;
        let grammar_toml = r#"
[tool]
name = "test"
inherit = ["shared_a", "shared_b"]

[[global_noise]]
pattern = '^OWN_RULE'
action = "strip"
"#;
        let mut grammar = load_grammar_from_str(grammar_toml).unwrap();

        let shared_a = load_shared_rules_from_str(shared_toml_a).unwrap();
        let shared_b = load_shared_rules_from_str(shared_toml_b).unwrap();

        let mut shared_rules: HashMap<String, Vec<Rule>> = HashMap::new();
        shared_rules.insert("shared_a".to_string(), shared_a);
        shared_rules.insert("shared_b".to_string(), shared_b);

        resolve_inherit(&mut grammar, &shared_rules);

        // Own rules should come first
        assert_eq!(grammar.global_noise.len(), 3);
        assert!(grammar.global_noise[0].pattern.is_match("OWN_RULE here"),
            "first rule should be own rule (OWN_RULE)");

        // Inherited rules should come after
        // Note: HashMap iteration order is non-deterministic, but both shared rules
        // should be at indices 1 and 2
        let inherited_patterns: Vec<bool> = grammar.global_noise[1..].iter()
            .map(|r| r.pattern.is_match("SHARED_A test") || r.pattern.is_match("SHARED_B test"))
            .collect();
        assert!(inherited_patterns.iter().all(|&x| x),
            "inherited rules should come after own rules");
    }

    #[test]
    fn test_inheritance_own_rules_take_priority_in_evaluate_line() {
        // When a line matches both an own global_noise rule and an inherited rule,
        // the own rule should match first (because inherited rules are appended after).
        let toml_str = r#"
[tool]
name = "test"

[[global_noise]]
pattern = '^progress'
action = "dedup"
"#;
        let mut grammar = load_grammar_from_str(toml_str).unwrap();

        // Simulate inherited rule that also matches "progress" but with strip action
        let shared_toml = r#"
[[rules]]
pattern = '^progress'
action = "strip"
"#;
        let shared_rules_vec = load_shared_rules_from_str(shared_toml).unwrap();
        let mut shared_rules: HashMap<String, Vec<Rule>> = HashMap::new();
        shared_rules.insert("shared".to_string(), shared_rules_vec);
        grammar.inherit = vec!["shared".to_string()];
        resolve_inherit(&mut grammar, &shared_rules);

        // The own rule (dedup) should be at index 0, inherited (strip) at index 1
        assert_eq!(grammar.global_noise.len(), 2);
        assert_eq!(grammar.global_noise[0].action, RuleAction::Dedup, "own rule should come first");
        assert_eq!(grammar.global_noise[1].action, RuleAction::Strip, "inherited rule should come second");

        // When evaluating, the own rule (dedup) should be found first
        let result = evaluate_line(&grammar, None, "progress: 50%");
        match result {
            RuleMatch::Noise { action, .. } => {
                assert_eq!(action, RuleAction::Dedup, "own rule should win over inherited");
            }
            other => panic!("expected Noise with Dedup, got: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // evaluate_line_with_fallback
    // -----------------------------------------------------------------------

    #[test]
    fn test_evaluate_line_with_fallback() {
        let toml_str = r#"
[tool]
name = "make"

[fallback]

[[fallback.hazard]]
pattern = '^make.*\*\*\*'
severity = "error"
action = "keep"

[[fallback.noise]]
pattern = '^make\[\d+\]:'
action = "strip"

[fallback.summary]
success = "ok"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();

        // Hazard match via fallback
        let result = evaluate_line_with_fallback(&grammar, "make[2]: *** Error 1");
        match result {
            RuleMatch::Hazard { severity, .. } => {
                assert_eq!(severity, Some(Severity::Error));
            }
            other => panic!("expected Hazard via fallback, got: {:?}", other),
        }

        // Noise match via fallback
        let result = evaluate_line_with_fallback(&grammar, "make[1]: Entering directory");
        match result {
            RuleMatch::Noise { action, .. } => {
                assert_eq!(action, RuleAction::Strip);
            }
            other => panic!("expected Noise via fallback, got: {:?}", other),
        }

        // No match
        let result = evaluate_line_with_fallback(&grammar, "some other line");
        assert_eq!(result, RuleMatch::NoMatch);
    }

    // -----------------------------------------------------------------------
    // resolve_category
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_category_from_grammar_front_matter() {
        let toml_str = r#"
[tool]
name = "vim"
category = "interactive"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let result = resolve_category(&grammar, None);
        assert_eq!(result, Category::Interactive);
    }

    #[test]
    fn test_resolve_category_from_categories_map() {
        let toml_str = r#"
[tool]
name = "ls"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut map = HashMap::new();
        map.insert("ls".to_string(), Category::Passthrough);

        let result = resolve_category(&grammar, Some(&map));
        assert_eq!(result, Category::Passthrough);
    }

    #[test]
    fn test_resolve_category_default_condense() {
        let toml_str = r#"
[tool]
name = "unknown-tool"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let result = resolve_category(&grammar, None);
        assert_eq!(result, Category::Condense);
    }

    #[test]
    fn test_resolve_category_grammar_overrides_map() {
        let toml_str = r#"
[tool]
name = "git"
category = "structured"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut map = HashMap::new();
        map.insert("git".to_string(), Category::Passthrough);

        // Grammar front matter (Structured) should win over map (Passthrough)
        let result = resolve_category(&grammar, Some(&map));
        assert_eq!(result, Category::Structured);
    }

    // -----------------------------------------------------------------------
    // evaluate_line with None action (no action, only global noise)
    // -----------------------------------------------------------------------

    #[test]
    fn test_evaluate_line_with_no_action_uses_global_noise() {
        let toml_str = r#"
[tool]
name = "npm"

[[global_noise]]
pattern = '^npm timing'
action = "strip"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();

        let result = evaluate_line(&grammar, None, "npm timing idealTree 12ms");
        match result {
            RuleMatch::Noise { action, .. } => {
                assert_eq!(action, RuleAction::Strip);
            }
            other => panic!("expected Noise, got: {:?}", other),
        }

        // Line that doesn't match global noise
        let result2 = evaluate_line(&grammar, None, "something else");
        assert_eq!(result2, RuleMatch::NoMatch);
    }

    // -----------------------------------------------------------------------
    // Warning severity hazard match
    // -----------------------------------------------------------------------

    #[test]
    fn test_evaluate_line_warning_hazard() {
        let grammar = build_eval_grammar();
        let action = grammar.actions.get("build").unwrap();

        let result = evaluate_line(&grammar, Some(action), "warning: unused variable");
        match result {
            RuleMatch::Hazard { action, severity, .. } => {
                assert_eq!(action, RuleAction::Dedup);
                assert_eq!(severity, Some(Severity::Warning));
            }
            other => panic!("expected Hazard(Warning), got: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Real grammar file tests (load from grammars/ directory)
    // -----------------------------------------------------------------------

    #[test]
    fn test_load_all_grammars_from_directory() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("grammars");
        let grammars = load_all_grammars(&dir).unwrap();

        // All grammars should be loaded
        for name in &["npm", "cargo", "git", "docker", "make", "pip", "pytest", "jest", "webpack", "kubectl", "terraform"] {
            assert!(grammars.contains_key(*name), "should load {name}");
        }

        // npm should have inherited rules appended to global_noise
        let npm = grammars.get("npm").unwrap();
        // npm has 2 own global_noise + ansi-progress (4 rules) + node-stacktrace (1 rule)
        assert!(npm.global_noise.len() >= 5,
            "npm should have own + inherited global_noise, got {}", npm.global_noise.len());

        // npm's own rules should come first
        assert!(npm.global_noise[0].pattern.is_match("npm timing foo"),
            "first global_noise rule should be npm's own (npm timing)");
        assert!(npm.global_noise[1].pattern.is_match("npm warn something"),
            "second global_noise rule should be npm's own (npm warn)");
    }

    #[test]
    fn test_detect_tool_all_five_from_directory() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("grammars");
        let grammars = load_all_grammars(&dir).unwrap();

        let test_cases = vec![
            (vec!["npm", "install"], "npm", true),
            (vec!["npx", "serve"], "npm", false),
            (vec!["cargo", "build"], "cargo", true),
            (vec!["cargo", "t"], "cargo", true),
            (vec!["git", "push"], "git", true),
            (vec!["git", "pull"], "git", true),
            (vec!["git", "clone", "url"], "git", true),
            (vec!["docker", "build", "."], "docker", true),
            (vec!["make", "all"], "make", true),
            (vec!["gmake", "clean"], "make", true),
            (vec!["unknown-cmd"], "none", false),
        ];

        for (args_raw, expected_tool, expect_action) in test_cases {
            let args: Vec<String> = args_raw.iter().map(|s| s.to_string()).collect();
            let result = detect_tool(&args, &grammars);

            if expected_tool == "none" {
                assert!(result.is_none(), "should not detect: {:?}", args_raw);
            } else {
                assert!(result.is_some(), "should detect {} for {:?}", expected_tool, args_raw);
                let (g, a) = result.unwrap();
                assert_eq!(g.tool.name, expected_tool, "wrong grammar for {:?}", args_raw);
                if expect_action {
                    assert!(a.is_some(), "should have action for {:?}", args_raw);
                }
            }
        }
    }

    #[test]
    fn test_evaluate_line_with_real_cargo_grammar() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("grammars");
        let grammars = load_all_grammars(&dir).unwrap();
        let grammar = grammars.get("cargo").unwrap();
        let build = grammar.actions.get("build").unwrap();

        // Hazard: error should match first
        let result = evaluate_line(grammar, Some(build), "error[E0433]: failed to resolve");
        assert!(matches!(result, RuleMatch::Hazard { .. }), "error line should be Hazard");

        // Outcome: Finished line
        let result = evaluate_line(grammar, Some(build), "   Finished `dev` profile in 5s");
        assert!(matches!(result, RuleMatch::Outcome { .. }), "Finished line should be Outcome");

        // Global noise: Compiling line
        let result = evaluate_line(grammar, Some(build), "   Compiling serde v1.0.0");
        assert!(matches!(result, RuleMatch::Noise { .. }), "Compiling line should be Noise");

        // No match
        let result = evaluate_line(grammar, Some(build), "some random output");
        assert_eq!(result, RuleMatch::NoMatch, "random line should be NoMatch");
    }

    #[test]
    fn test_evaluate_line_with_real_make_grammar_fallback() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("grammars");
        let grammars = load_all_grammars(&dir).unwrap();
        let grammar = grammars.get("make").unwrap();

        // Use fallback action (make has no named actions)
        // Hazard: make *** error
        let result = evaluate_line_with_fallback(grammar, "make[1]: *** [Makefile:10: all] Error 2");
        assert!(matches!(result, RuleMatch::Hazard { .. }),
            "make *** should be Hazard, got {:?}", result);

        // Noise: make[N]: Entering directory (from fallback)
        let result = evaluate_line_with_fallback(grammar, "make[1]: Entering directory '/tmp'");
        assert!(matches!(result, RuleMatch::Noise { .. }),
            "make entering dir should be Noise, got {:?}", result);

        // Inherited (global noise): gcc command echo (from c-compiler-output)
        let result = evaluate_line(grammar, None, "gcc -Wall -O2 -c main.c -o main.o");
        assert!(matches!(result, RuleMatch::Noise { .. }),
            "gcc command should be Noise (inherited), got {:?}", result);
    }

    // -----------------------------------------------------------------------
    // LlmHint tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_llm_hint_toml_deserialization_tool_level() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("grammars");
        let grammars = load_all_grammars(&dir).unwrap();

        // pytest has a tool-level llm_hint
        let pytest = grammars.get("pytest").unwrap();
        assert_eq!(pytest.llm_hints.len(), 1, "pytest should have 1 tool-level hint");
        assert_eq!(pytest.llm_hints[0].prefer, "--tb=short");
        assert!(!pytest.llm_hints[0].reason.is_empty());

        // terraform has a tool-level llm_hint
        let tf = grammars.get("terraform").unwrap();
        assert_eq!(tf.llm_hints.len(), 1, "terraform should have 1 tool-level hint");
        assert_eq!(tf.llm_hints[0].prefer, "-no-color");

        // jest has a tool-level llm_hint
        let jest = grammars.get("jest").unwrap();
        assert_eq!(jest.llm_hints.len(), 1, "jest should have 1 tool-level hint");
        assert_eq!(jest.llm_hints[0].prefer, "--silent");
    }

    #[test]
    fn test_llm_hint_toml_deserialization_action_level() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("grammars");
        let grammars = load_all_grammars(&dir).unwrap();

        // git log action has a hint
        let git = grammars.get("git").unwrap();
        let log = git.actions.get("log").unwrap();
        assert_eq!(log.llm_hints.len(), 1, "git log should have 1 hint");
        assert_eq!(log.llm_hints[0].prefer, "--oneline -20");

        // git status action has a hint
        let status = git.actions.get("status").unwrap();
        assert_eq!(status.llm_hints.len(), 1, "git status should have 1 hint");
        assert_eq!(status.llm_hints[0].prefer, "--short");

        // cargo test action has a hint
        let cargo = grammars.get("cargo").unwrap();
        let test = cargo.actions.get("test").unwrap();
        assert_eq!(test.llm_hints.len(), 1, "cargo test should have 1 hint");
        assert_eq!(test.llm_hints[0].prefer, "-- --nocapture");

        // go test action has a hint
        let go = grammars.get("go").unwrap();
        let go_test = go.actions.get("test").unwrap();
        assert_eq!(go_test.llm_hints.len(), 1, "go test should have 1 hint");
        assert_eq!(go_test.llm_hints[0].prefer, "-json");

        // kubectl get action has a hint
        let kubectl = grammars.get("kubectl").unwrap();
        let get = kubectl.actions.get("get").unwrap();
        assert_eq!(get.llm_hints.len(), 1, "kubectl get should have 1 hint");
        assert_eq!(get.llm_hints[0].prefer, "-o json");
    }

    #[test]
    fn test_llm_hint_grammar_without_hints_is_empty() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("grammars");
        let grammars = load_all_grammars(&dir).unwrap();

        // cargo build action has no llm_hints
        let cargo = grammars.get("cargo").unwrap();
        let build = cargo.actions.get("build").unwrap();
        assert!(build.llm_hints.is_empty(), "cargo build should have no hints");

        // cargo itself has no tool-level hints
        assert!(cargo.llm_hints.is_empty(), "cargo tool should have no tool-level hints");
    }
}
