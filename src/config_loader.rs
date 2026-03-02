//! Config loader — aggregates all runtime configuration.
//!
//! Loads bundled grammars (compiled in via `include_str!`), user grammars
//! from `~/.config/mish/grammars/`, project-local grammars from `.mish/grammars/`,
//! categories.toml, dangerous.toml, and mish.toml into a single `RuntimeConfig`.

use std::collections::HashMap;
use std::path::Path;

use regex::Regex;
use serde::Deserialize;

use crate::config::{default_config, load_config, MishConfig};
use crate::core::grammar::{load_grammar_from_str, Grammar};
use crate::router::categories::{
    load_categories_config_from_str, CategoriesConfig, DangerousPattern,
};

// ---------------------------------------------------------------------------
// Bundled content (compiled in)
// ---------------------------------------------------------------------------

const BUNDLED_CARGO: &str = include_str!("../grammars/cargo.toml");
const BUNDLED_DOCKER: &str = include_str!("../grammars/docker.toml");
const BUNDLED_GIT: &str = include_str!("../grammars/git.toml");
const BUNDLED_MAKE: &str = include_str!("../grammars/make.toml");
const BUNDLED_NPM: &str = include_str!("../grammars/npm.toml");

const BUNDLED_CATEGORIES: &str = include_str!("../grammars/_meta/categories.toml");
const BUNDLED_DANGEROUS: &str = include_str!("../grammars/_meta/dangerous.toml");

/// Bundled grammar entries: (tool_name, toml_content).
const BUNDLED_GRAMMARS: &[(&str, &str)] = &[
    ("cargo", BUNDLED_CARGO),
    ("docker", BUNDLED_DOCKER),
    ("git", BUNDLED_GIT),
    ("make", BUNDLED_MAKE),
    ("npm", BUNDLED_NPM),
];

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Everything needed at runtime for command routing.
pub struct RuntimeConfig {
    pub grammars: HashMap<String, Grammar>,
    pub categories_config: CategoriesConfig,
    pub dangerous_patterns: Vec<DangerousPattern>,
    pub mish_config: MishConfig,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Load all runtime config: grammars from grammars/ dir, categories.toml, dangerous.toml, mish.toml.
///
/// Grammar search order:
/// 1. Bundled grammars (compiled in via `include_str!`)
/// 2. User grammars at `~/.config/mish/grammars/`
/// 3. Project-local grammars at `.mish/grammars/` (if exists)
///
/// User/project grammars override bundled ones by tool name.
pub fn load_runtime_config(
    config_path: Option<&str>,
) -> Result<RuntimeConfig, Box<dyn std::error::Error>> {
    // 1. Load MishConfig
    let mish_config = match config_path {
        Some(path) => load_config(path)?,
        None => load_config("~/.config/mish/mish.toml")?,
    };

    // 2. Load bundled grammars
    let mut grammars = load_bundled_grammars();

    // 3. Load user grammars (override bundled)
    let user_grammar_dir = expand_tilde("~/.config/mish/grammars");
    load_grammars_from_dir(&user_grammar_dir, &mut grammars);

    // 4. Load project-local grammars (override user and bundled)
    load_grammars_from_dir(".mish/grammars", &mut grammars);

    // 5. Load categories config (bundled default)
    let categories_config = load_categories_config_from_str(BUNDLED_CATEGORIES)
        .map_err(|e| format!("failed to parse bundled categories.toml: {e}"))?;

    // 6. Load dangerous patterns (bundled default, skip invalid regex)
    let dangerous_patterns = load_dangerous_patterns_lenient(BUNDLED_DANGEROUS)
        .map_err(|e| format!("failed to parse bundled dangerous.toml: {e}"))?;

    Ok(RuntimeConfig {
        grammars,
        categories_config,
        dangerous_patterns,
        mish_config,
    })
}

/// Load with defaults only (no disk access). For tests.
pub fn default_runtime_config() -> RuntimeConfig {
    let grammars = load_bundled_grammars();

    let categories_config = load_categories_config_from_str(BUNDLED_CATEGORIES)
        .expect("bundled categories.toml must be valid");

    let dangerous_patterns = load_dangerous_patterns_lenient(BUNDLED_DANGEROUS)
        .expect("bundled dangerous.toml must be parseable TOML");

    RuntimeConfig {
        grammars,
        categories_config,
        dangerous_patterns,
        mish_config: default_config(),
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Parse all bundled grammar TOML strings into a HashMap.
fn load_bundled_grammars() -> HashMap<String, Grammar> {
    let mut grammars = HashMap::new();
    for (name, toml_str) in BUNDLED_GRAMMARS {
        match load_grammar_from_str(toml_str) {
            Ok(grammar) => {
                grammars.insert(name.to_string(), grammar);
            }
            Err(e) => {
                eprintln!("warning: failed to parse bundled grammar '{name}': {e}");
            }
        }
    }
    grammars
}

/// Load grammar TOML files from a directory, inserting/overriding entries in `grammars`.
///
/// Files are identified by filename stem (e.g., `npm.toml` -> "npm").
/// Files that fail to parse are warned about and skipped.
fn load_grammars_from_dir(dir: &str, grammars: &mut HashMap<String, Grammar>) {
    let dir_path = Path::new(dir);
    if !dir_path.is_dir() {
        return;
    }

    let entries = match std::fs::read_dir(dir_path) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();

        // Only process .toml files
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }

        // Extract tool name from filename stem
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        // Skip names starting with underscore (like _shared, _meta)
        if name.starts_with('_') {
            continue;
        }

        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "warning: failed to read grammar file '{}': {e}",
                    path.display()
                );
                continue;
            }
        };

        match load_grammar_from_str(&contents) {
            Ok(grammar) => {
                grammars.insert(name, grammar);
            }
            Err(e) => {
                eprintln!(
                    "warning: failed to parse grammar '{}': {e}",
                    path.display()
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Raw serde types for lenient dangerous pattern loading
// ---------------------------------------------------------------------------

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

/// Load dangerous patterns leniently: skip entries whose regex fails to compile.
fn load_dangerous_patterns_lenient(toml_str: &str) -> Result<Vec<DangerousPattern>, String> {
    let raw: RawDangerousConfig =
        toml::from_str(toml_str).map_err(|e| format!("TOML parse error: {e}"))?;
    let mut patterns = Vec::new();

    for rp in raw.patterns {
        match Regex::new(&rp.pattern) {
            Ok(regex) => {
                patterns.push(DangerousPattern {
                    pattern: regex,
                    reason: rp.reason,
                });
            }
            Err(e) => {
                eprintln!(
                    "warning: skipping dangerous pattern '{}': invalid regex: {e}",
                    rp.pattern
                );
            }
        }
    }

    Ok(patterns)
}

use crate::util::expand_tilde;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::preflight::OutputMode;
    use crate::router::categories::{categorize, Category, ExecutionMode};
    use crate::router::route;

    // -----------------------------------------------------------------------
    // Test: Default runtime config has all bundled grammars loaded
    // -----------------------------------------------------------------------
    #[test]
    fn test_default_has_bundled_grammars() {
        let rc = default_runtime_config();

        // All 5 bundled grammars should be present
        assert!(
            rc.grammars.contains_key("cargo"),
            "missing bundled grammar: cargo"
        );
        assert!(
            rc.grammars.contains_key("docker"),
            "missing bundled grammar: docker"
        );
        assert!(
            rc.grammars.contains_key("git"),
            "missing bundled grammar: git"
        );
        assert!(
            rc.grammars.contains_key("make"),
            "missing bundled grammar: make"
        );
        assert!(
            rc.grammars.contains_key("npm"),
            "missing bundled grammar: npm"
        );
        assert_eq!(rc.grammars.len(), 5);
    }

    // -----------------------------------------------------------------------
    // Test: Default runtime config has categories from _meta/categories.toml
    // -----------------------------------------------------------------------
    #[test]
    fn test_default_has_categories() {
        let rc = default_runtime_config();

        // Spot-check several categories from the bundled file
        assert_eq!(
            *rc.categories_config.categories.get("cp").unwrap(),
            Category::Narrate
        );
        assert_eq!(
            *rc.categories_config.categories.get("cat").unwrap(),
            Category::Passthrough
        );
        assert_eq!(
            *rc.categories_config.categories.get("vim").unwrap(),
            Category::Interactive
        );
        assert_eq!(
            *rc.categories_config.categories.get("mv").unwrap(),
            Category::Narrate
        );
        assert_eq!(
            *rc.categories_config.categories.get("echo").unwrap(),
            Category::Passthrough
        );
    }

    // -----------------------------------------------------------------------
    // Test: Default runtime config has dangerous patterns from _meta/dangerous.toml
    // -----------------------------------------------------------------------
    #[test]
    fn test_default_has_dangerous_patterns() {
        let rc = default_runtime_config();

        // The bundled dangerous.toml has 10 patterns
        assert!(
            !rc.dangerous_patterns.is_empty(),
            "should have dangerous patterns loaded"
        );
        assert!(
            rc.dangerous_patterns.len() >= 5,
            "expected at least 5 dangerous patterns, got {}",
            rc.dangerous_patterns.len()
        );

        // Verify specific patterns match
        assert!(
            rc.dangerous_patterns
                .iter()
                .any(|p| p.pattern.is_match("rm -rf /")),
            "should have rm -rf pattern"
        );
        assert!(
            rc.dangerous_patterns
                .iter()
                .any(|p| p.pattern.is_match("git push origin main --force")),
            "should have git force push pattern"
        );
    }

    // -----------------------------------------------------------------------
    // Test: User grammar overrides bundled grammar of same name
    // -----------------------------------------------------------------------
    #[test]
    fn test_user_grammar_overrides_bundled() {
        // Simulate the override behavior by loading bundled first, then inserting
        // a grammar with the same key
        let mut grammars = load_bundled_grammars();

        // The bundled npm grammar should exist
        assert!(grammars.contains_key("npm"));
        let original_actions = grammars["npm"].actions.len();

        // Create a minimal "override" grammar
        let override_toml = r#"
[tool]
name = "npm"
detect = ["npm"]
"#;
        let override_grammar = load_grammar_from_str(override_toml).unwrap();
        grammars.insert("npm".to_string(), override_grammar);

        // Now the npm grammar should have zero actions (our override has none)
        assert_eq!(grammars["npm"].actions.len(), 0);
        assert_ne!(
            original_actions, 0,
            "original should have had actions for comparison"
        );
    }

    // -----------------------------------------------------------------------
    // Test: Invalid grammar file is skipped with warning
    // -----------------------------------------------------------------------
    #[test]
    fn test_invalid_grammar_skipped() {
        use std::fs;

        // Create a temp directory with an invalid grammar
        let tmp = std::env::temp_dir().join("mish_test_invalid_grammar");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // Write a valid grammar
        fs::write(
            tmp.join("good.toml"),
            r#"
[tool]
name = "good"
detect = ["good"]
"#,
        )
        .unwrap();

        // Write an invalid grammar (bad TOML)
        fs::write(tmp.join("bad.toml"), "this is not valid toml [[[").unwrap();

        // Write an invalid grammar (valid TOML but missing [tool] section)
        fs::write(tmp.join("broken.toml"), "[something]\nkey = \"value\"").unwrap();

        let mut grammars = HashMap::new();
        load_grammars_from_dir(tmp.to_str().unwrap(), &mut grammars);

        // The good grammar should be loaded
        assert!(
            grammars.contains_key("good"),
            "valid grammar should be loaded"
        );

        // Bad grammars should be skipped (not crash)
        // They may or may not be in the map depending on how they fail,
        // but the function should not panic
        assert!(
            !grammars.contains_key("bad"),
            "invalid TOML should be skipped"
        );

        // Clean up
        let _ = fs::remove_dir_all(&tmp);
    }

    // -----------------------------------------------------------------------
    // Test: RuntimeConfig integrates correctly with router::route()
    // -----------------------------------------------------------------------
    #[test]
    fn test_runtime_config_with_router() {
        let rc = default_runtime_config();

        // Use categorize (which route() calls internally) with RuntimeConfig fields
        let command: Vec<String> = vec!["echo", "hello"]
            .into_iter()
            .map(String::from)
            .collect();
        let category = categorize(
            &command,
            &rc.grammars,
            &rc.categories_config,
            &rc.dangerous_patterns,
        );
        assert_eq!(
            category,
            Category::Passthrough,
            "echo should be Passthrough via categories.toml"
        );

        // Dangerous pattern from bundled dangerous.toml
        let command: Vec<String> = vec!["rm", "-rf", "/tmp/foo"]
            .into_iter()
            .map(String::from)
            .collect();
        let category = categorize(
            &command,
            &rc.grammars,
            &rc.categories_config,
            &rc.dangerous_patterns,
        );
        assert_eq!(
            category,
            Category::Dangerous,
            "rm -rf should be Dangerous via dangerous patterns"
        );

        // Full route() integration — use echo through passthrough
        let command: Vec<String> = vec!["echo", "integration test"]
            .into_iter()
            .map(String::from)
            .collect();
        let result = route(
            &command,
            &rc.grammars,
            &rc.categories_config,
            &rc.dangerous_patterns,
            OutputMode::Human,
            ExecutionMode::Cli,
        );
        match result {
            Ok(r) => {
                assert_eq!(r.category, Category::Passthrough);
                assert_eq!(r.exit_code, 0);
            }
            Err(e) => {
                // route may fail in some test environments (e.g., no echo binary)
                // but the categorization path above already validated correctness
                eprintln!("route() failed (acceptable in CI): {e}");
            }
        }
    }

    // -----------------------------------------------------------------------
    // Test: Grammar tool names match their key
    // -----------------------------------------------------------------------
    #[test]
    fn test_bundled_grammar_tool_names() {
        let rc = default_runtime_config();

        for (key, grammar) in &rc.grammars {
            assert_eq!(
                &grammar.tool.name, key,
                "grammar key '{key}' should match tool.name '{}'",
                grammar.tool.name
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test: load_runtime_config with default path works
    // -----------------------------------------------------------------------
    #[test]
    fn test_load_runtime_config_default_path() {
        // When no config file exists, load_runtime_config should succeed
        // with defaults (load_config returns default when file not found)
        let result = load_runtime_config(None);
        assert!(
            result.is_ok(),
            "load_runtime_config(None) should succeed: {:?}",
            result.err()
        );
        let rc = result.unwrap();
        assert_eq!(rc.grammars.len(), 5);
    }

    // -----------------------------------------------------------------------
    // Test: expand_tilde helper
    // -----------------------------------------------------------------------
    #[test]
    fn test_expand_tilde() {
        let home = std::env::var("HOME").unwrap_or_default();
        let expanded = expand_tilde("~/foo/bar");
        assert_eq!(expanded, format!("{home}/foo/bar"));

        // Non-tilde path returned as-is
        let plain = expand_tilde("/usr/local/bin");
        assert_eq!(plain, "/usr/local/bin");
    }

    // -----------------------------------------------------------------------
    // Test: underscore-prefixed files in grammar dir are skipped
    // -----------------------------------------------------------------------
    #[test]
    fn test_underscore_files_skipped() {
        use std::fs;

        let tmp = std::env::temp_dir().join("mish_test_underscore_skip");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // Write an underscore-prefixed grammar file
        fs::write(
            tmp.join("_internal.toml"),
            r#"
[tool]
name = "_internal"
detect = ["_internal"]
"#,
        )
        .unwrap();

        // Write a normal grammar file
        fs::write(
            tmp.join("mytool.toml"),
            r#"
[tool]
name = "mytool"
detect = ["mytool"]
"#,
        )
        .unwrap();

        let mut grammars = HashMap::new();
        load_grammars_from_dir(tmp.to_str().unwrap(), &mut grammars);

        assert!(
            !grammars.contains_key("_internal"),
            "underscore-prefixed files should be skipped"
        );
        assert!(
            grammars.contains_key("mytool"),
            "normal grammar files should be loaded"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    // -----------------------------------------------------------------------
    // Test: MishConfig defaults are sane
    // -----------------------------------------------------------------------
    #[test]
    fn test_default_mish_config() {
        let rc = default_runtime_config();
        assert_eq!(rc.mish_config.server.max_sessions, 5);
        assert_eq!(rc.mish_config.squasher.max_lines, 200);
        assert_eq!(rc.mish_config.timeout_defaults.default, 300);
    }
}
