//! Hardcoded deny-list for catastrophic commands.
//!
//! Checked before any command execution as a safety net.
//! This is NOT a security boundary — it is trivially bypassable via indirection.
//! The deny list blocks commands that would cause catastrophic damage if run accidentally.

use regex::Regex;
use std::sync::OnceLock;

/// A compiled deny pattern with its human-readable reason.
struct DenyRule {
    pattern: Regex,
    reason: &'static str,
}

/// Raw deny patterns: (regex_pattern, human_readable_reason).
const DENY_PATTERNS: &[(&str, &str)] = &[
    (r"rm\s+-rf\s+/\s*$", "Recursive delete of root filesystem"),
    (r"rm\s+-rf\s+/\*", "Recursive delete of root filesystem"),
    (r"mkfs\.", "Format filesystem"),
    (r"dd\s+.*of=/dev/[sh]d", "Direct write to block device"),
    (r":\(\)\{.*\|.*&\s*\};\s*:", "Fork bomb"),
    (r">\s*/dev/[sh]d", "Redirect to block device"),
    (r"chmod\s+-R\s+777\s+/\s*$", "World-writable root filesystem"),
];

/// Returns the compiled deny rules, compiling them only once.
fn deny_rules() -> &'static [DenyRule] {
    static RULES: OnceLock<Vec<DenyRule>> = OnceLock::new();
    RULES.get_or_init(|| {
        DENY_PATTERNS
            .iter()
            .map(|(pattern, reason)| DenyRule {
                pattern: Regex::new(pattern)
                    .unwrap_or_else(|e| panic!("Invalid deny pattern `{pattern}`: {e}")),
                reason,
            })
            .collect()
    })
}

/// Check if a command is denied by the hardcoded deny-list.
/// Returns `Some(reason)` if blocked, `None` if allowed.
pub fn check_deny_list(cmd: &str) -> Option<String> {
    let rules = deny_rules();
    for rule in rules {
        if rule.pattern.is_match(cmd) {
            return Some(rule.reason.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rm_rf_root_is_blocked() {
        let result = check_deny_list("rm -rf /");
        assert!(result.is_some(), "rm -rf / should be blocked");
        assert!(result.unwrap().contains("Recursive delete"));
    }

    #[test]
    fn rm_rf_root_glob_is_blocked() {
        let result = check_deny_list("rm -rf /*");
        assert!(result.is_some(), "rm -rf /* should be blocked");
        assert!(result.unwrap().contains("Recursive delete"));
    }

    #[test]
    fn rm_rf_specific_path_tmp_is_allowed() {
        assert!(
            check_deny_list("rm -rf /tmp/foo").is_none(),
            "rm -rf /tmp/foo should be allowed"
        );
    }

    #[test]
    fn rm_rf_specific_path_home_is_allowed() {
        assert!(
            check_deny_list("rm -rf /home/user").is_none(),
            "rm -rf /home/user should be allowed"
        );
    }

    #[test]
    fn mkfs_is_blocked() {
        let result = check_deny_list("mkfs.ext4 /dev/sda1");
        assert!(result.is_some(), "mkfs.ext4 should be blocked");
        assert!(result.unwrap().contains("Format filesystem"));
    }

    #[test]
    fn dd_write_to_block_device_is_blocked() {
        let result = check_deny_list("dd if=/dev/zero of=/dev/sda");
        assert!(result.is_some(), "dd write to /dev/sda should be blocked");
        assert!(result.unwrap().contains("block device"));
    }

    #[test]
    fn dd_read_from_block_device_is_allowed() {
        assert!(
            check_deny_list("dd if=/dev/sda of=disk.img").is_none(),
            "dd reading from device should be allowed"
        );
    }

    #[test]
    fn fork_bomb_is_blocked() {
        let result = check_deny_list(":(){ :|:& };:");
        assert!(result.is_some(), "Fork bomb should be blocked");
        assert!(result.unwrap().contains("Fork bomb"));
    }

    #[test]
    fn redirect_to_block_device_is_blocked() {
        let result = check_deny_list("> /dev/sda");
        assert!(result.is_some(), "> /dev/sda should be blocked");
        assert!(result.unwrap().contains("block device"));
    }

    #[test]
    fn chmod_777_root_is_blocked() {
        let result = check_deny_list("chmod -R 777 /");
        assert!(result.is_some(), "chmod -R 777 / should be blocked");
        assert!(result.unwrap().contains("World-writable"));
    }

    #[test]
    fn chmod_777_specific_path_is_allowed() {
        assert!(
            check_deny_list("chmod -R 777 /tmp").is_none(),
            "chmod -R 777 /tmp should be allowed"
        );
    }

    #[test]
    fn normal_commands_pass_through() {
        assert!(check_deny_list("ls -la").is_none());
        assert!(check_deny_list("cat foo").is_none());
        assert!(check_deny_list("npm install").is_none());
        assert!(check_deny_list("cargo build").is_none());
    }

    #[test]
    fn blocked_commands_return_human_readable_reason() {
        let result = check_deny_list("rm -rf /").unwrap();
        // Reason should be a non-empty, human-readable string
        assert!(!result.is_empty());
        assert!(result.chars().next().unwrap().is_uppercase());
    }

    #[test]
    fn empty_command_is_not_blocked() {
        assert!(check_deny_list("").is_none());
    }

    #[test]
    fn complex_safe_commands_are_not_blocked() {
        assert!(
            check_deny_list("rm -rf ./build").is_none(),
            "rm -rf ./build should be allowed"
        );
        assert!(
            check_deny_list("dd if=/dev/zero of=test.img bs=1M count=100").is_none(),
            "dd writing to regular file should be allowed"
        );
    }
}
