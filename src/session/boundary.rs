use regex::Regex;
use uuid::Uuid;

/// Boundary detection strategy.
#[derive(Debug, Clone, PartialEq)]
pub enum BoundaryStrategy {
    /// PROMPT_COMMAND / precmd (bash/zsh)
    ShellIntegration,
    /// UUID sentinels (sh/dash)
    Sentinel,
}

/// Result of waiting for command boundary.
#[derive(Debug, Clone)]
pub struct BoundaryResult {
    pub exit_code: i32,
    pub cwd: String,
    pub output: String,
}

/// Boundary detector for a shell session.
pub struct BoundaryDetector {
    strategy: BoundaryStrategy,
}

impl BoundaryDetector {
    /// Create detector for the given shell.
    pub fn new(shell: &str) -> Self {
        let strategy = Self::detect_strategy(shell);
        Self { strategy }
    }

    /// Detect shell type and choose strategy.
    /// bash, zsh -> ShellIntegration
    /// sh, dash, other -> Sentinel
    pub fn detect_strategy(shell: &str) -> BoundaryStrategy {
        let basename = shell.rsplit('/').next().unwrap_or(shell);
        match basename {
            "bash" | "zsh" => BoundaryStrategy::ShellIntegration,
            _ => BoundaryStrategy::Sentinel,
        }
    }

    /// Generate the shell hook injection commands for session startup.
    /// For bash: sets PROMPT_COMMAND to emit OSC 133 with exit code and CWD.
    /// For zsh: sets precmd to emit the same sequence.
    pub fn shell_hook_commands(&self) -> String {
        match self.strategy {
            BoundaryStrategy::ShellIntegration => {
                // We need to figure out if it was constructed with bash or zsh.
                // Since we only store the strategy, we'll return a combined hook
                // that works for both (the caller knows the shell). But the spec
                // says bash gets PROMPT_COMMAND and zsh gets precmd.
                //
                // We'll return both; each shell ignores the irrelevant one.
                // PROMPT_COMMAND is bash-only; precmd() is zsh-only.
                //
                // PROMPT_EOL_MARK='' suppresses zsh's PROMPT_SP no-newline indicator.
                // When command output doesn't end with a newline, zsh prints
                // PROMPT_EOL_MARK (default "%") + spaces + CR. Setting it to empty
                // prevents the "%" from appearing in captured output.
                // In bash this is just a harmless variable assignment.
                concat!(
                    "PROMPT_EOL_MARK=''\n",
                    "PS0=$'\\033]133;C\\033\\\\'\n",
                    "PROMPT_COMMAND='printf \"\\033]133;D;%d\\033\\\\\" $?; printf \"\\033]133;P;%s\\033\\\\\" \"$PWD\"'\n",
                    "preexec() { printf '\\033]133;C\\033\\\\'; }\n",
                    "precmd() { printf '\\033]133;D;%d\\033\\\\' $?; printf '\\033]133;P;%s\\033\\\\' \"$PWD\"; }"
                ).to_string()
            }
            BoundaryStrategy::Sentinel => {
                // No hooks needed for sentinel mode; wrapping happens per-command.
                String::new()
            }
        }
    }

    /// Wrap a command with boundary markers.
    /// For ShellIntegration: just the command (hooks fire automatically).
    /// For Sentinel: wraps with echo __LLMSH_START_<uuid>__ / echo __LLMSH_END_<uuid>__ $?
    /// Returns (wrapped_command, optional_sentinel_uuid)
    pub fn wrap_command(&self, cmd: &str) -> (String, Option<String>) {
        match self.strategy {
            BoundaryStrategy::ShellIntegration => (cmd.to_string(), None),
            BoundaryStrategy::Sentinel => {
                let uuid = Uuid::new_v4().to_string();
                let wrapped = format!(
                    "echo __LLMSH_START_{uuid}__; {cmd}; echo __LLMSH_END_{uuid}__ $?"
                );
                (wrapped, Some(uuid))
            }
        }
    }

    /// Parse output buffer to find boundary markers and extract exit code + CWD.
    /// For ShellIntegration: scan for OSC 133;D;<exit_code> and OSC 133;P;<cwd>.
    /// For Sentinel: scan for __LLMSH_END_<uuid>__ <exit_code>.
    /// Returns None if boundary not yet detected (keep reading).
    pub fn detect_boundary(
        &self,
        buffer: &str,
        sentinel_uuid: Option<&str>,
    ) -> Option<BoundaryResult> {
        match self.strategy {
            BoundaryStrategy::ShellIntegration => self.detect_osc133(buffer),
            BoundaryStrategy::Sentinel => {
                let uuid = sentinel_uuid?;
                self.detect_sentinel(buffer, uuid)
            }
        }
    }

    /// Get the strategy being used.
    pub fn strategy(&self) -> &BoundaryStrategy {
        &self.strategy
    }

    /// Detect OSC 133 boundary markers in buffer.
    ///
    /// Uses 133;C (command start) to precisely separate ZLE echo from real output:
    ///   [ZLE echo garbage]\x1b]133;C\x1b\\[real output]\x1b]133;D;0\x1b\\\x1b]133;P;/path\x1b\\
    ///
    /// Falls back to current behavior (strip D/P, return everything) when no C marker present.
    fn detect_osc133(&self, buffer: &str) -> Option<BoundaryResult> {
        let re_d = Regex::new(r"\x1b\]133;D;(-?\d+)\x1b\\").ok()?;
        let re_p = Regex::new(r"\x1b\]133;P;([^\x1b]+)\x1b\\").ok()?;
        let re_c = Regex::new(r"\x1b\]133;C\x1b\\").ok()?;

        // If 133;C marker exists, extract only content between C and D.
        // This precisely discards ZLE echo that appears before C.
        if let Some(c_match) = re_c.find(buffer) {
            let after_c = c_match.end();
            let tail = &buffer[after_c..];

            // Find D and P markers AFTER C (ignore any stale ones before C)
            let cap_d = re_d.captures(tail)?;
            let cap_p = re_p.captures(tail)?;

            let exit_code: i32 = cap_d.get(1)?.as_str().parse().ok()?;
            let cwd = cap_p.get(1)?.as_str().to_string();

            let d_match = re_d.find(tail)?;
            let content = &tail[..d_match.start()];

            // Strip any remaining 133 sequences from the extracted content
            let cleaned = re_c.replace_all(content, "");
            let cleaned = re_d.replace_all(&cleaned, "");
            let cleaned = re_p.replace_all(&cleaned, "");

            Some(BoundaryResult {
                exit_code,
                cwd,
                output: cleaned.to_string(),
            })
        } else {
            // No C marker — fallback: strip D/P and return everything.
            // This handles sentinel-like situations or shells without preexec.
            let cap_d = re_d.captures(buffer)?;
            let cap_p = re_p.captures(buffer)?;

            let exit_code: i32 = cap_d.get(1)?.as_str().parse().ok()?;
            let cwd = cap_p.get(1)?.as_str().to_string();

            let cleaned = re_d.replace_all(buffer, "");
            let cleaned = re_p.replace_all(&cleaned, "");

            Some(BoundaryResult {
                exit_code,
                cwd,
                output: cleaned.to_string(),
            })
        }
    }

    /// Detect sentinel boundary markers in buffer.
    fn detect_sentinel(&self, buffer: &str, uuid: &str) -> Option<BoundaryResult> {
        let start_marker = format!("__LLMSH_START_{uuid}__");
        let end_pattern = format!(r"__LLMSH_END_{uuid}__\s+(\d+)");
        let re_end = Regex::new(&end_pattern).ok()?;

        let cap_end = re_end.captures(buffer)?;
        let exit_code: i32 = cap_end.get(1)?.as_str().parse().ok()?;

        // Extract output between start and end markers
        let start_pos = buffer.find(&start_marker)?;
        let end_match = cap_end.get(0)?;

        // Content starts after the start marker line
        let after_start = &buffer[start_pos + start_marker.len()..];
        // Skip the newline after the start marker
        let content_start = if after_start.starts_with('\n') {
            &after_start[1..]
        } else {
            after_start
        };

        // Find end marker position relative to content_start
        let content_start_abs = buffer.len() - after_start.len()
            + (after_start.len() - content_start.len());
        let content_before_end = &buffer[content_start_abs..end_match.start()];

        // Strip any trailing newline before the end marker
        let output = content_before_end.trim_end_matches('\n').to_string();

        Some(BoundaryResult {
            exit_code,
            cwd: String::new(), // Sentinel mode doesn't capture CWD
            output,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test 1: detect_strategy("bash") returns ShellIntegration
    #[test]
    fn test_detect_strategy_bash() {
        assert_eq!(
            BoundaryDetector::detect_strategy("bash"),
            BoundaryStrategy::ShellIntegration
        );
    }

    // Test 2: detect_strategy("zsh") returns ShellIntegration
    #[test]
    fn test_detect_strategy_zsh() {
        assert_eq!(
            BoundaryDetector::detect_strategy("zsh"),
            BoundaryStrategy::ShellIntegration
        );
    }

    // Test 3: detect_strategy("/bin/bash") returns ShellIntegration (full path)
    #[test]
    fn test_detect_strategy_bin_bash() {
        assert_eq!(
            BoundaryDetector::detect_strategy("/bin/bash"),
            BoundaryStrategy::ShellIntegration
        );
    }

    // Test 4: detect_strategy("/usr/bin/zsh") returns ShellIntegration
    #[test]
    fn test_detect_strategy_usr_bin_zsh() {
        assert_eq!(
            BoundaryDetector::detect_strategy("/usr/bin/zsh"),
            BoundaryStrategy::ShellIntegration
        );
    }

    // Test 5: detect_strategy("sh") returns Sentinel
    #[test]
    fn test_detect_strategy_sh() {
        assert_eq!(
            BoundaryDetector::detect_strategy("sh"),
            BoundaryStrategy::Sentinel
        );
    }

    // Test 6: detect_strategy("dash") returns Sentinel
    #[test]
    fn test_detect_strategy_dash() {
        assert_eq!(
            BoundaryDetector::detect_strategy("dash"),
            BoundaryStrategy::Sentinel
        );
    }

    // Test 7: detect_strategy("fish") returns Sentinel (unknown = fallback)
    #[test]
    fn test_detect_strategy_unknown_fallback() {
        assert_eq!(
            BoundaryDetector::detect_strategy("fish"),
            BoundaryStrategy::Sentinel
        );
    }

    // Test 8: Shell hook commands for bash contain PROMPT_COMMAND
    #[test]
    fn test_shell_hook_bash_contains_prompt_command() {
        let detector = BoundaryDetector::new("bash");
        let hooks = detector.shell_hook_commands();
        assert!(
            hooks.contains("PROMPT_COMMAND"),
            "bash hooks should contain PROMPT_COMMAND, got: {hooks}"
        );
    }

    // Test 9: Shell hook commands for zsh contain precmd
    #[test]
    fn test_shell_hook_zsh_contains_precmd() {
        let detector = BoundaryDetector::new("zsh");
        let hooks = detector.shell_hook_commands();
        assert!(
            hooks.contains("precmd"),
            "zsh hooks should contain precmd, got: {hooks}"
        );
    }

    // Test 10: wrap_command for ShellIntegration returns command unchanged, no UUID
    #[test]
    fn test_wrap_command_shell_integration() {
        let detector = BoundaryDetector::new("bash");
        let (wrapped, uuid) = detector.wrap_command("ls -la");
        assert_eq!(wrapped, "ls -la");
        assert!(uuid.is_none());
    }

    // Test 11: wrap_command for Sentinel returns wrapped command with UUID
    #[test]
    fn test_wrap_command_sentinel() {
        let detector = BoundaryDetector::new("sh");
        let (wrapped, uuid) = detector.wrap_command("ls -la");
        assert!(uuid.is_some());
        let uuid = uuid.unwrap();
        assert!(wrapped.contains(&format!("__LLMSH_START_{uuid}__")));
        assert!(wrapped.contains(&format!("__LLMSH_END_{uuid}__")));
        assert!(wrapped.contains("ls -la"));
        assert!(wrapped.contains("$?"));
    }

    // Test 12: detect_boundary with OSC 133;D;0 and 133;P;/tmp returns exit_code=0, cwd="/tmp"
    #[test]
    fn test_detect_boundary_osc133_success() {
        let detector = BoundaryDetector::new("bash");
        let buffer = "some output\x1b]133;D;0\x1b\\\x1b]133;P;/tmp\x1b\\";
        let result = detector.detect_boundary(buffer, None);
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.cwd, "/tmp");
    }

    // Test 13: detect_boundary with OSC 133;D;1 and 133;P;/home returns exit_code=1
    #[test]
    fn test_detect_boundary_osc133_failure() {
        let detector = BoundaryDetector::new("bash");
        let buffer = "error output\x1b]133;D;1\x1b\\\x1b]133;P;/home\x1b\\";
        let result = detector.detect_boundary(buffer, None);
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(result.exit_code, 1);
        assert_eq!(result.cwd, "/home");
    }

    // Test 14: detect_boundary with only OSC 133;D (no P) returns None (incomplete)
    #[test]
    fn test_detect_boundary_osc133_incomplete() {
        let detector = BoundaryDetector::new("bash");
        let buffer = "some output\x1b]133;D;0\x1b\\";
        let result = detector.detect_boundary(buffer, None);
        assert!(result.is_none());
    }

    // Test 15: detect_boundary with sentinel mode parses __LLMSH_END_<uuid>__ 0
    #[test]
    fn test_detect_boundary_sentinel_success() {
        let detector = BoundaryDetector::new("sh");
        let uuid = "test-uuid-1234";
        let buffer = format!(
            "__LLMSH_START_{uuid}__\ncommand output here\n__LLMSH_END_{uuid}__ 0"
        );
        let result = detector.detect_boundary(&buffer, Some(uuid));
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.output, "command output here");
    }

    // Test 16: detect_boundary with sentinel returns None when UUID doesn't match
    #[test]
    fn test_detect_boundary_sentinel_uuid_mismatch() {
        let detector = BoundaryDetector::new("sh");
        let buffer = "__LLMSH_START_abc123__\noutput\n__LLMSH_END_abc123__ 0";
        let result = detector.detect_boundary(buffer, Some("different-uuid"));
        assert!(result.is_none());
    }

    // Test 17: Sentinel lines are stripped from captured output
    #[test]
    fn test_sentinel_lines_stripped() {
        let detector = BoundaryDetector::new("sh");
        let uuid = "strip-test-uuid";
        let buffer = format!(
            "__LLMSH_START_{uuid}__\nline1\nline2\n__LLMSH_END_{uuid}__ 0"
        );
        let result = detector.detect_boundary(&buffer, Some(uuid));
        assert!(result.is_some());
        let result = result.unwrap();
        // Output should not contain sentinel markers
        assert!(!result.output.contains("__LLMSH_START_"));
        assert!(!result.output.contains("__LLMSH_END_"));
        assert_eq!(result.output, "line1\nline2");
    }

    // Test 18: OSC 133 sequences are stripped from captured output
    #[test]
    fn test_osc133_sequences_stripped() {
        let detector = BoundaryDetector::new("bash");
        let buffer = "hello world\x1b]133;D;0\x1b\\\x1b]133;P;/tmp\x1b\\";
        let result = detector.detect_boundary(buffer, None);
        assert!(result.is_some());
        let result = result.unwrap();
        assert!(!result.output.contains("\x1b]133;D;"));
        assert!(!result.output.contains("\x1b]133;P;"));
        assert_eq!(result.output, "hello world");
    }

    // Test 19: detect_boundary returns None when no boundary markers found (keep reading)
    #[test]
    fn test_detect_boundary_no_markers() {
        let detector_bash = BoundaryDetector::new("bash");
        let result = detector_bash.detect_boundary("just some output", None);
        assert!(result.is_none());

        let detector_sh = BoundaryDetector::new("sh");
        let result = detector_sh.detect_boundary("just some output", Some("some-uuid"));
        assert!(result.is_none());
    }

    // Test 20: detect_boundary handles command output mixed with boundary markers
    #[test]
    fn test_detect_boundary_mixed_output() {
        let detector = BoundaryDetector::new("bash");
        let buffer =
            "line1\nline2\nline3\x1b]133;D;42\x1b\\\x1b]133;P;/home/user/project\x1b\\";
        let result = detector.detect_boundary(buffer, None);
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(result.exit_code, 42);
        assert_eq!(result.cwd, "/home/user/project");
        assert_eq!(result.output, "line1\nline2\nline3");
    }

    // Test 21: Shell hooks include PROMPT_EOL_MARK suppression for zsh
    #[test]
    fn test_shell_hooks_suppress_prompt_eol_mark() {
        let detector = BoundaryDetector::new("zsh");
        let hooks = detector.shell_hook_commands();
        assert!(
            hooks.contains("PROMPT_EOL_MARK=''"),
            "hooks should set PROMPT_EOL_MARK='', got: {hooks}"
        );
    }

    // Test 22: Shell hooks contain preexec (zsh 133;C emitter)
    #[test]
    fn test_shell_hooks_contain_preexec() {
        let detector = BoundaryDetector::new("zsh");
        let hooks = detector.shell_hook_commands();
        assert!(
            hooks.contains("preexec()"),
            "hooks should contain preexec for 133;C emission, got: {hooks}"
        );
        assert!(
            hooks.contains("133;C"),
            "hooks should contain 133;C marker, got: {hooks}"
        );
    }

    // Test 23: Shell hooks contain PS0 (bash 133;C emitter)
    #[test]
    fn test_shell_hooks_contain_ps0() {
        let detector = BoundaryDetector::new("bash");
        let hooks = detector.shell_hook_commands();
        assert!(
            hooks.contains("PS0="),
            "hooks should contain PS0 for bash 133;C emission, got: {hooks}"
        );
        assert!(
            hooks.contains("133;C"),
            "hooks should contain 133;C marker, got: {hooks}"
        );
    }

    // Test 24: detect_osc133 with C marker extracts only content between C and D
    #[test]
    fn test_detect_osc133_with_c_marker() {
        let detector = BoundaryDetector::new("bash");
        let buffer = "zle garbage\x1b]133;C\x1b\\real output\x1b]133;D;0\x1b\\\x1b]133;P;/tmp\x1b\\";
        let result = detector.detect_boundary(buffer, None);
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.cwd, "/tmp");
        assert_eq!(result.output, "real output");
    }

    // Test 25: detect_osc133 with C marker strips ZLE echo before C
    #[test]
    fn test_detect_osc133_c_strips_echo() {
        let detector = BoundaryDetector::new("bash");
        // Simulate: ZLE echoes "lls -la" garbled, then 133;C, then real ls output
        let buffer = "lls -la /path/pathls -la /path\x1b]133;C\x1b\\file1.txt\nfile2.txt\x1b]133;D;0\x1b\\\x1b]133;P;/home\x1b\\";
        let result = detector.detect_boundary(buffer, None);
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(result.output, "file1.txt\nfile2.txt");
        assert!(!result.output.contains("lls"));
    }

    // Test 26: detect_osc133 without C marker falls back to current behavior
    #[test]
    fn test_detect_osc133_without_c_fallback() {
        let detector = BoundaryDetector::new("bash");
        // No C marker — should fall back to stripping D/P and returning everything
        let buffer = "some output\x1b]133;D;0\x1b\\\x1b]133;P;/tmp\x1b\\";
        let result = detector.detect_boundary(buffer, None);
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.cwd, "/tmp");
        assert_eq!(result.output, "some output");
    }

    // Test 27: stale D marker before C is ignored — only D after C matters
    #[test]
    fn test_detect_osc133_stale_d_before_c_ignored() {
        let detector = BoundaryDetector::new("bash");
        // Stale D from previous command, then C, then real output, then current D
        let buffer = "\x1b]133;D;1\x1b\\\x1b]133;P;/old\x1b\\stale stuff\x1b]133;C\x1b\\real output\x1b]133;D;0\x1b\\\x1b]133;P;/new\x1b\\";
        let result = detector.detect_boundary(buffer, None);
        assert!(result.is_some());
        let result = result.unwrap();
        // Should use the D after C (exit code 0), not the stale D (exit code 1)
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.cwd, "/new");
        assert_eq!(result.output, "real output");
    }

    // Test 28: C marker with multiline output between C and D
    #[test]
    fn test_detect_osc133_c_multiline_output() {
        let detector = BoundaryDetector::new("bash");
        let buffer = "echo garbage\x1b]133;C\x1b\\line1\nline2\nline3\x1b]133;D;0\x1b\\\x1b]133;P;/tmp\x1b\\";
        let result = detector.detect_boundary(buffer, None);
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(result.output, "line1\nline2\nline3");
    }
}
