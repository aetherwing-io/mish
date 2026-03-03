//! Scope extraction and matching for policy rules.
//!
//! Scope is the tool binary name extracted from a command string.
//! Used by auto_confirm rules to limit which commands a rule applies to.
//!
//! Known limitations:
//! - `"env npm install"` -> scope is `"env"`, not `"npm"`
//! - `"sudo apt install"` -> scope is `"sudo"`, not `"apt"`

use std::path::Path;

/// Extract the scope (tool binary name) from a command string.
///
/// Takes the first whitespace-delimited token and returns its basename.
///
/// # Examples
/// - `"npm install"` -> `"npm"`
/// - `"/usr/bin/npm run"` -> `"npm"`
/// - `""` -> `""`
pub fn extract_scope(command: &str) -> &str {
    let first_token = command.split_whitespace().next().unwrap_or("");
    // Get basename: if token is "/usr/bin/npm", return "npm"
    Path::new(first_token)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(first_token)
}

/// Check if a command matches any scope in the scope list.
///
/// If scope is `None`, matches everything (global rule).
/// If scope is `Some(scopes)`, the command's extracted scope must be in the list.
pub fn scope_matches(command: &str, scope: &Option<Vec<String>>) -> bool {
    match scope {
        None => true,
        Some(scopes) => {
            let cmd_scope = extract_scope(command);
            scopes.iter().any(|s| s == cmd_scope)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // extract_scope tests
    // -----------------------------------------------------------------------

    #[test]
    fn extract_scope_simple_command() {
        assert_eq!(extract_scope("npm install"), "npm");
    }

    #[test]
    fn extract_scope_absolute_path() {
        assert_eq!(extract_scope("/usr/bin/npm run"), "npm");
    }

    #[test]
    fn extract_scope_empty_string() {
        assert_eq!(extract_scope(""), "");
    }

    #[test]
    fn extract_scope_single_token() {
        assert_eq!(extract_scope("ls"), "ls");
    }

    #[test]
    fn extract_scope_with_leading_whitespace() {
        assert_eq!(extract_scope("  cargo build"), "cargo");
    }

    #[test]
    fn extract_scope_relative_path() {
        assert_eq!(extract_scope("./scripts/deploy.sh arg1"), "deploy.sh");
    }

    #[test]
    fn extract_scope_sudo_limitation() {
        // Known limitation: scope is "sudo", not "apt"
        assert_eq!(extract_scope("sudo apt install vim"), "sudo");
    }

    #[test]
    fn extract_scope_env_limitation() {
        // Known limitation: scope is "env", not "npm"
        assert_eq!(extract_scope("env npm install"), "env");
    }

    // -----------------------------------------------------------------------
    // scope_matches tests
    // -----------------------------------------------------------------------

    #[test]
    fn scope_matches_none_matches_everything() {
        assert!(scope_matches("npm install", &None));
        assert!(scope_matches("cargo build", &None));
        assert!(scope_matches("", &None));
    }

    #[test]
    fn scope_matches_exact_match() {
        let scope = Some(vec!["npm".to_string()]);
        assert!(scope_matches("npm install", &scope));
    }

    #[test]
    fn scope_matches_absolute_path() {
        let scope = Some(vec!["npm".to_string()]);
        assert!(scope_matches("/usr/bin/npm run", &scope));
    }

    #[test]
    fn scope_matches_no_match() {
        let scope = Some(vec!["npm".to_string()]);
        assert!(!scope_matches("cargo build", &scope));
    }

    #[test]
    fn scope_matches_multiple_scopes() {
        let scope = Some(vec!["apt".to_string(), "apt-get".to_string()]);
        assert!(scope_matches("apt install vim", &scope));
        assert!(scope_matches("apt-get install vim", &scope));
        assert!(!scope_matches("npm install", &scope));
    }

    #[test]
    fn scope_matches_empty_scope_list() {
        let scope = Some(vec![]);
        assert!(!scope_matches("npm install", &scope));
    }

    #[test]
    fn scope_matches_empty_command_with_scope() {
        let scope = Some(vec!["npm".to_string()]);
        assert!(!scope_matches("", &scope));
    }
}
