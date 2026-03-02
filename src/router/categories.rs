/// The six command categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// Verbose output -> condensed summary (npm install, cargo build)
    Condense,
    /// Silent commands -> narrated result (cp, mv, mkdir)
    Narrate,
    /// Output verbatim + metadata footer (cat, grep, ls)
    Passthrough,
    /// Machine-readable parse -> formatted view (git status, docker ps)
    Structured,
    /// Transparent passthrough for interactive commands (vim, htop)
    Interactive,
    /// Warn before executing destructive commands (rm -rf, force push)
    Dangerous,
}

impl Default for Category {
    fn default() -> Self {
        Category::Condense
    }
}

/// Execution mode: CLI proxy or MCP server.
///
/// Handlers use this to vary behavior — e.g. Interactive returns an error in
/// MCP mode (can't run vim over stdio), Dangerous returns a structured warning
/// instead of prompting in MCP mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    /// CLI proxy mode (`mish <command>`)
    Cli,
    /// MCP server mode (`mish serve`)
    Mcp,
}

impl Category {
    /// Parse a category name string into a Category variant.
    pub fn from_str(s: &str) -> Option<Category> {
        match s {
            "condense" => Some(Category::Condense),
            "narrate" => Some(Category::Narrate),
            "passthrough" => Some(Category::Passthrough),
            "structured" => Some(Category::Structured),
            "interactive" => Some(Category::Interactive),
            "dangerous" => Some(Category::Dangerous),
            _ => None,
        }
    }
}

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use regex::Regex;
use serde::Deserialize;

use crate::core::grammar::Grammar;

// ---------------------------------------------------------------------------
// Display for Category
// ---------------------------------------------------------------------------

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Category::Condense => "condense",
            Category::Narrate => "narrate",
            Category::Passthrough => "passthrough",
            Category::Structured => "structured",
            Category::Interactive => "interactive",
            Category::Dangerous => "dangerous",
        };
        write!(f, "{}", name)
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum CategoriesError {
    Io(std::io::Error),
    Parse(toml::de::Error),
    InvalidRegex { pattern: String, source: regex::Error },
    InvalidCategory(String),
}

impl fmt::Display for CategoriesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CategoriesError::Io(e) => write!(f, "IO error: {e}"),
            CategoriesError::Parse(e) => write!(f, "TOML parse error: {e}"),
            CategoriesError::InvalidRegex { pattern, source } => {
                write!(f, "invalid regex '{pattern}': {source}")
            }
            CategoriesError::InvalidCategory(c) => write!(f, "invalid category: {c}"),
        }
    }
}

impl std::error::Error for CategoriesError {}

impl From<std::io::Error> for CategoriesError {
    fn from(e: std::io::Error) -> Self {
        CategoriesError::Io(e)
    }
}

impl From<toml::de::Error> for CategoriesError {
    fn from(e: toml::de::Error) -> Self {
        CategoriesError::Parse(e)
    }
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A dangerous command pattern: regex matched against the full command string.
#[derive(Debug, Clone)]
pub struct DangerousPattern {
    pub pattern: Regex,
    pub reason: String,
}

/// Command-name to category mapping loaded from categories.toml.
#[derive(Debug, Clone)]
pub struct CategoriesConfig {
    pub categories: HashMap<String, Category>,
}

// ---------------------------------------------------------------------------
// Raw serde types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawCategoriesConfig {
    #[serde(default)]
    categories: HashMap<String, String>,
}

#[derive(Deserialize)]
struct RawDangerousConfig {
    #[serde(default)]
    patterns: Vec<RawDangerousPattern>,
}

#[derive(Deserialize)]
struct RawDangerousPattern {
    pattern: String,
    reason: String,
}

// ---------------------------------------------------------------------------
// Public API — loading
// ---------------------------------------------------------------------------

/// Load a categories.toml config file.
pub fn load_categories_config(path: &Path) -> Result<CategoriesConfig, CategoriesError> {
    let contents = std::fs::read_to_string(path)?;
    load_categories_config_from_str(&contents)
}

/// Parse a categories config from a TOML string.
pub fn load_categories_config_from_str(
    toml_str: &str,
) -> Result<CategoriesConfig, CategoriesError> {
    let raw: RawCategoriesConfig = toml::from_str(toml_str)?;
    let mut categories = HashMap::new();

    for (cmd, cat_str) in raw.categories {
        let category = Category::from_str(&cat_str).ok_or_else(|| {
            CategoriesError::InvalidCategory(cat_str.clone())
        })?;
        categories.insert(cmd, category);
    }

    Ok(CategoriesConfig { categories })
}

/// Load dangerous patterns from a dangerous.toml file.
pub fn load_dangerous_patterns(
    path: &Path,
) -> Result<Vec<DangerousPattern>, CategoriesError> {
    let contents = std::fs::read_to_string(path)?;
    load_dangerous_patterns_from_str(&contents)
}

/// Parse dangerous patterns from a TOML string.
pub fn load_dangerous_patterns_from_str(
    toml_str: &str,
) -> Result<Vec<DangerousPattern>, CategoriesError> {
    let raw: RawDangerousConfig = toml::from_str(toml_str)?;
    let mut patterns = Vec::new();

    for rp in raw.patterns {
        let regex = Regex::new(&rp.pattern).map_err(|e| CategoriesError::InvalidRegex {
            pattern: rp.pattern.clone(),
            source: e,
        })?;
        patterns.push(DangerousPattern {
            pattern: regex,
            reason: rp.reason,
        });
    }

    Ok(patterns)
}

// ---------------------------------------------------------------------------
// Public API — category resolution
// ---------------------------------------------------------------------------

/// Resolve the category for a command.
///
/// Resolution order (first match wins):
/// 1. Dangerous patterns — regex match against full command string
/// 2. Grammar front matter — category field in the grammar
/// 3. categories.toml — mapping from command name to category
/// 4. Fallback — default to Condense
pub fn categorize(
    command: &[String],
    grammars: &HashMap<String, Grammar>,
    categories_config: &CategoriesConfig,
    dangerous_patterns: &[DangerousPattern],
) -> Category {
    if command.is_empty() {
        return Category::default();
    }

    // Step 1: Check dangerous patterns against full command string
    let full_command = command.join(" ");
    for dp in dangerous_patterns {
        if dp.pattern.is_match(&full_command) {
            return Category::Dangerous;
        }
    }

    // Step 2: Check grammar front matter category
    let cmd_name = &command[0];
    if let Some((_grammar, category)) = find_grammar_category(cmd_name, grammars) {
        return category;
    }

    // Step 3: Check categories.toml
    if let Some(&category) = categories_config.categories.get(cmd_name.as_str()) {
        return category;
    }

    // Step 4: Fallback to Condense
    Category::default()
}

/// Find the grammar for a command and return its declared category, if any.
fn find_grammar_category<'a>(
    cmd_name: &str,
    grammars: &'a HashMap<String, Grammar>,
) -> Option<(&'a Grammar, Category)> {
    for grammar in grammars.values() {
        if grammar.detect.iter().any(|d| d == cmd_name) {
            if let Some(cat) = grammar.category {
                return Some((grammar, cat));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::grammar::load_grammar_from_str;

    // Test 15: categorize returns Dangerous when dangerous pattern matches
    #[test]
    fn test_categorize_dangerous_pattern_overrides() {
        let dangerous = vec![DangerousPattern {
            pattern: Regex::new(r"rm\s+-rf").unwrap(),
            reason: "Force recursive delete".to_string(),
        }];
        let grammars = HashMap::new();
        let config = CategoriesConfig {
            categories: HashMap::from([
                ("rm".to_string(), Category::Narrate),
            ]),
        };

        let command: Vec<String> = vec!["rm", "-rf", "/tmp/foo"]
            .into_iter()
            .map(String::from)
            .collect();
        let result = categorize(&command, &grammars, &config, &dangerous);
        assert_eq!(result, Category::Dangerous);
    }

    // Test 16: categorize returns grammar's category when set
    #[test]
    fn test_categorize_grammar_category() {
        let toml_str = r#"
[tool]
name = "vim"
category = "interactive"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut grammars = HashMap::new();
        grammars.insert("vim".to_string(), grammar);

        let config = CategoriesConfig {
            categories: HashMap::new(),
        };
        let dangerous: Vec<DangerousPattern> = Vec::new();

        let command: Vec<String> = vec!["vim", "file.txt"]
            .into_iter()
            .map(String::from)
            .collect();
        let result = categorize(&command, &grammars, &config, &dangerous);
        assert_eq!(result, Category::Interactive);
    }

    // Test 17: categorize uses categories.toml when no grammar category
    #[test]
    fn test_categorize_uses_categories_config() {
        let grammars = HashMap::new();
        let config = CategoriesConfig {
            categories: HashMap::from([
                ("cat".to_string(), Category::Passthrough),
                ("cp".to_string(), Category::Narrate),
            ]),
        };
        let dangerous: Vec<DangerousPattern> = Vec::new();

        let command: Vec<String> = vec!["cat", "file.txt"]
            .into_iter()
            .map(String::from)
            .collect();
        let result = categorize(&command, &grammars, &config, &dangerous);
        assert_eq!(result, Category::Passthrough);
    }

    // Test 18: categorize falls back to Condense for unknown commands
    #[test]
    fn test_categorize_fallback_condense() {
        let grammars = HashMap::new();
        let config = CategoriesConfig {
            categories: HashMap::new(),
        };
        let dangerous: Vec<DangerousPattern> = Vec::new();

        let command: Vec<String> = vec!["some-unknown-tool", "--flag"]
            .into_iter()
            .map(String::from)
            .collect();
        let result = categorize(&command, &grammars, &config, &dangerous);
        assert_eq!(result, Category::Condense);
    }

    // Test 19: load_dangerous_patterns parses dangerous.toml correctly
    #[test]
    fn test_load_dangerous_patterns() {
        let toml_str = r#"
[[patterns]]
pattern = "rm\\s+-rf"
reason = "Force recursive delete — no confirmation, potential data loss"

[[patterns]]
pattern = "git\\s+push\\s+.*--force"
reason = "Force push — overwrites remote history"

[[patterns]]
pattern = "dd\\s+"
reason = "Direct disk write — can overwrite filesystems"
"#;
        let patterns = load_dangerous_patterns_from_str(toml_str).unwrap();
        assert_eq!(patterns.len(), 3);
        assert!(patterns[0].pattern.is_match("rm -rf /"));
        assert_eq!(
            patterns[0].reason,
            "Force recursive delete — no confirmation, potential data loss"
        );
        assert!(patterns[1].pattern.is_match("git push origin main --force"));
        assert!(patterns[2].pattern.is_match("dd if=/dev/zero of=/dev/sda"));
    }

    // Test 20: load_categories_config parses categories.toml correctly
    #[test]
    fn test_load_categories_config() {
        let toml_str = r#"
[categories]
cp = "narrate"
mv = "narrate"
cat = "passthrough"
vim = "interactive"
"#;
        let config = load_categories_config_from_str(toml_str).unwrap();
        assert_eq!(config.categories.len(), 4);
        assert_eq!(*config.categories.get("cp").unwrap(), Category::Narrate);
        assert_eq!(*config.categories.get("mv").unwrap(), Category::Narrate);
        assert_eq!(
            *config.categories.get("cat").unwrap(),
            Category::Passthrough
        );
        assert_eq!(
            *config.categories.get("vim").unwrap(),
            Category::Interactive
        );
    }

    // Test: empty command returns default (Condense)
    #[test]
    fn test_categorize_empty_command() {
        let grammars = HashMap::new();
        let config = CategoriesConfig {
            categories: HashMap::new(),
        };
        let dangerous: Vec<DangerousPattern> = Vec::new();
        let command: Vec<String> = Vec::new();
        let result = categorize(&command, &grammars, &config, &dangerous);
        assert_eq!(result, Category::Condense);
    }

    // Test: dangerous pattern takes priority over grammar category
    #[test]
    fn test_dangerous_overrides_grammar_category() {
        let toml_str = r#"
[tool]
name = "git"
category = "structured"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut grammars = HashMap::new();
        grammars.insert("git".to_string(), grammar);

        let dangerous = vec![DangerousPattern {
            pattern: Regex::new(r"git\s+push\s+.*--force").unwrap(),
            reason: "Force push".to_string(),
        }];
        let config = CategoriesConfig {
            categories: HashMap::new(),
        };

        let command: Vec<String> = vec!["git", "push", "origin", "main", "--force"]
            .into_iter()
            .map(String::from)
            .collect();
        let result = categorize(&command, &grammars, &config, &dangerous);
        assert_eq!(result, Category::Dangerous);
    }

    // Test: grammar category takes priority over categories.toml
    #[test]
    fn test_grammar_category_overrides_config() {
        let toml_str = r#"
[tool]
name = "git"
category = "structured"
"#;
        let grammar = load_grammar_from_str(toml_str).unwrap();
        let mut grammars = HashMap::new();
        grammars.insert("git".to_string(), grammar);

        let config = CategoriesConfig {
            categories: HashMap::from([
                ("git".to_string(), Category::Passthrough),
            ]),
        };
        let dangerous: Vec<DangerousPattern> = Vec::new();

        let command: Vec<String> = vec!["git", "status"]
            .into_iter()
            .map(String::from)
            .collect();
        let result = categorize(&command, &grammars, &config, &dangerous);
        // Grammar category (Structured) should win over config (Passthrough)
        assert_eq!(result, Category::Structured);
    }

    // Test: Category::from_str works for all variants
    #[test]
    fn test_category_from_str() {
        assert_eq!(Category::from_str("condense"), Some(Category::Condense));
        assert_eq!(Category::from_str("narrate"), Some(Category::Narrate));
        assert_eq!(
            Category::from_str("passthrough"),
            Some(Category::Passthrough)
        );
        assert_eq!(
            Category::from_str("structured"),
            Some(Category::Structured)
        );
        assert_eq!(
            Category::from_str("interactive"),
            Some(Category::Interactive)
        );
        assert_eq!(Category::from_str("dangerous"), Some(Category::Dangerous));
        assert_eq!(Category::from_str("unknown"), None);
    }

    // Test: invalid category in config produces error
    #[test]
    fn test_invalid_category_in_config() {
        let toml_str = r#"
[categories]
foo = "bogus_category"
"#;
        let result = load_categories_config_from_str(toml_str);
        assert!(result.is_err());
    }
}
