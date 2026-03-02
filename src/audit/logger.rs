//! Append-only audit log for tool calls, policy decisions, and process lifecycle events.
//!
//! Writes JSON Lines to `~/.local/share/mish/audit.log` (configurable).
//! The log file descriptor is opened with O_CLOEXEC so child processes
//! cannot inherit it.

use crate::config::AuditConfig;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::SystemTime;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

// ---------------------------------------------------------------------------
// Log level ordering
// ---------------------------------------------------------------------------

/// Numeric log level for filtering. Lower = more verbose.
fn log_level_rank(level: &str) -> u8 {
    match level {
        "trace" => 0,
        "debug" => 1,
        "info" => 2,
        "warn" => 3,
        "error" => 4,
        _ => 2, // default to info
    }
}

/// Return the implicit log level for an `AuditEvent` variant.
fn event_log_level(event: &AuditEvent) -> &'static str {
    match event {
        AuditEvent::ToolCall { .. } => "debug",
        AuditEvent::CommandStarted { .. } => "info",
        AuditEvent::CommandCompleted { .. } => "info",
        AuditEvent::CommandKilled { .. } => "warn",
        AuditEvent::CommandTimedOut { .. } => "warn",
        AuditEvent::PolicyDecision { .. } => "info",
        AuditEvent::HandoffInitiated { .. } => "info",
        AuditEvent::HandoffAttached { .. } => "info",
        AuditEvent::HandoffResolved { .. } => "info",
        AuditEvent::ServerStarted => "info",
        AuditEvent::ServerShutdown => "info",
        AuditEvent::SessionCreated { .. } => "info",
        AuditEvent::SessionClosed { .. } => "info",
        AuditEvent::Error { .. } => "error",
    }
}

use crate::util::expand_tilde;

// ---------------------------------------------------------------------------
// ISO 8601 timestamp from SystemTime (no chrono dependency)
// ---------------------------------------------------------------------------

fn iso8601_now() -> String {
    let now = SystemTime::now();
    let dur = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();

    // Break epoch seconds into date/time components (UTC).
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Civil date from days since 1970-01-01 (algorithm from Howard Hinnant).
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, m, d, hours, minutes, seconds
    )
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single audit log entry.
#[derive(Serialize)]
pub struct AuditEntry {
    pub timestamp: String,
    pub session: String,
    pub tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub event: AuditEvent,
}

/// Event payload — internally tagged via `type`.
#[derive(Serialize)]
#[serde(tag = "type")]
pub enum AuditEvent {
    ToolCall {
        tool_name: String,
        cmd: Option<String>,
        exit_code: Option<i32>,
    },
    CommandStarted {
        alias: Option<String>,
        pid: u32,
    },
    CommandCompleted {
        alias: Option<String>,
        exit_code: i32,
        duration_ms: u64,
    },
    CommandKilled {
        alias: Option<String>,
        signal: String,
    },
    CommandTimedOut {
        alias: Option<String>,
        timeout_sec: u64,
    },
    PolicyDecision {
        rule: String,
        action: String,
    },
    HandoffInitiated {
        alias: String,
        handoff_id: String,
    },
    HandoffAttached {
        handoff_id: String,
    },
    HandoffResolved {
        handoff_id: String,
        duration_ms: u64,
    },
    ServerStarted,
    ServerShutdown,
    SessionCreated {
        session: String,
    },
    SessionClosed {
        session: String,
    },
    Error {
        message: String,
    },
}

// ---------------------------------------------------------------------------
// AuditLogger
// ---------------------------------------------------------------------------

/// Append-only audit logger.
///
/// When the log file cannot be opened (e.g. permission error) the logger
/// stores `file: None` and silently drops all entries — the server must not
/// be prevented from starting.
pub struct AuditLogger {
    file: Option<File>,
    config: AuditConfig,
}

impl AuditLogger {
    /// Create a new audit logger.
    ///
    /// Creates parent directories if they don't exist. If the log file
    /// cannot be opened, a warning is printed to stderr but the logger is
    /// still returned (with logging disabled).
    pub fn new(config: &AuditConfig) -> Result<Self, std::io::Error> {
        let expanded = expand_tilde(&config.log_path);
        let path = Path::new(&expanded);

        // Create parent directories if needed. If this fails, we still
        // attempt to open the file (which will also fail), and gracefully
        // degrade to a disabled logger.
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    eprintln!(
                        "mish: warning: cannot create audit log directory {}: {e}",
                        parent.display()
                    );
                }
            }
        }

        // Open with O_APPEND + O_CLOEXEC.
        let file = match Self::open_log_file(path) {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!(
                    "mish: warning: cannot open audit log at {}: {e}",
                    expanded
                );
                None
            }
        };

        Ok(Self {
            file,
            config: config.clone(),
        })
    }

    /// Open the log file with append + cloexec semantics.
    #[cfg(unix)]
    fn open_log_file(path: &Path) -> Result<File, std::io::Error> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(path)
    }

    /// Open the log file (non-Unix fallback — no O_CLOEXEC).
    #[cfg(not(unix))]
    fn open_log_file(path: &Path) -> Result<File, std::io::Error> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    }

    /// Log an audit entry.
    ///
    /// The entry is serialised as a single JSON line. If the configured log
    /// level filters it out, or the relevant category flag (`log_commands`,
    /// `log_policy_decisions`, `log_handoff_events`) is `false`, the entry
    /// is silently dropped.
    pub fn log(&mut self, entry: AuditEntry) {
        if self.file.is_none() {
            return;
        }

        // Level check.
        let entry_level = log_level_rank(event_log_level(&entry.event));
        let config_level = log_level_rank(&self.config.log_level);
        if entry_level < config_level {
            return;
        }

        // Category check.
        if !self.should_log_event(&entry.event) {
            return;
        }

        // Serialize and write.
        if let Ok(json) = serde_json::to_string(&entry) {
            if let Some(ref mut file) = self.file {
                let _ = writeln!(file, "{json}");
            }
        }
    }

    /// Check category-level config flags.
    fn should_log_event(&self, event: &AuditEvent) -> bool {
        match event {
            AuditEvent::CommandStarted { .. }
            | AuditEvent::CommandCompleted { .. }
            | AuditEvent::CommandKilled { .. }
            | AuditEvent::CommandTimedOut { .. } => self.config.log_commands,
            AuditEvent::PolicyDecision { .. } => self.config.log_policy_decisions,
            AuditEvent::HandoffInitiated { .. }
            | AuditEvent::HandoffAttached { .. }
            | AuditEvent::HandoffResolved { .. } => self.config.log_handoff_events,
            // Everything else (ToolCall, Server*, Session*, Error) is always logged.
            _ => true,
        }
    }

    /// Flush the log file.
    pub fn flush(&mut self) {
        if let Some(ref mut f) = self.file {
            let _ = f.flush();
        }
    }

    /// Close the log file.
    pub fn close(self) {
        // `self.file` is dropped, closing the fd.
        drop(self);
    }
}

// ---------------------------------------------------------------------------
// Convenience constructors
// ---------------------------------------------------------------------------

impl AuditEntry {
    /// Create a new entry with the current UTC timestamp.
    pub fn new(session: String, tool: String, command: Option<String>, event: AuditEvent) -> Self {
        Self {
            timestamp: iso8601_now(),
            session,
            tool,
            command,
            event,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::TempDir;

    /// Helper: create AuditConfig pointing at a temp directory.
    fn config_in(dir: &TempDir) -> AuditConfig {
        AuditConfig {
            log_path: dir.path().join("audit.log").to_string_lossy().to_string(),
            log_level: "trace".into(),
            log_commands: true,
            log_policy_decisions: true,
            log_handoff_events: true,
        }
    }

    /// Read the entire log file contents.
    fn read_log(dir: &TempDir) -> String {
        let mut s = String::new();
        let path = dir.path().join("audit.log");
        if path.exists() {
            File::open(path).unwrap().read_to_string(&mut s).unwrap();
        }
        s
    }

    // 1. Logger creates file at specified path
    #[test]
    fn creates_file_at_specified_path() {
        let dir = TempDir::new().unwrap();
        let cfg = config_in(&dir);
        let _logger = AuditLogger::new(&cfg).unwrap();
        assert!(dir.path().join("audit.log").exists());
    }

    // 2. Logger creates parent directories
    #[test]
    fn creates_parent_directories() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a").join("b").join("c");
        let cfg = AuditConfig {
            log_path: nested.join("audit.log").to_string_lossy().to_string(),
            log_level: "info".into(),
            log_commands: true,
            log_policy_decisions: true,
            log_handoff_events: true,
        };
        let _logger = AuditLogger::new(&cfg).unwrap();
        assert!(nested.join("audit.log").exists());
    }

    // 3. Entries are written in JSON Lines format
    #[test]
    fn writes_json_lines() {
        let dir = TempDir::new().unwrap();
        let cfg = config_in(&dir);
        let mut logger = AuditLogger::new(&cfg).unwrap();

        logger.log(AuditEntry::new(
            "s1".into(),
            "sh_run".into(),
            Some("ls".into()),
            AuditEvent::ServerStarted,
        ));
        logger.flush();

        let content = read_log(&dir);
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(parsed["session"], "s1");
        assert_eq!(parsed["tool"], "sh_run");
        assert_eq!(parsed["command"], "ls");
        assert_eq!(parsed["event"]["type"], "ServerStarted");
    }

    // 4. Multiple entries appear on separate lines
    #[test]
    fn multiple_entries_on_separate_lines() {
        let dir = TempDir::new().unwrap();
        let cfg = config_in(&dir);
        let mut logger = AuditLogger::new(&cfg).unwrap();

        logger.log(AuditEntry::new(
            "s1".into(),
            "sh_run".into(),
            None,
            AuditEvent::ServerStarted,
        ));
        logger.log(AuditEntry::new(
            "s1".into(),
            "sh_run".into(),
            None,
            AuditEvent::ServerShutdown,
        ));
        logger.flush();

        let content = read_log(&dir);
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);

        // Both lines must be valid JSON.
        for line in &lines {
            let _: serde_json::Value = serde_json::from_str(line).unwrap();
        }
    }

    // 5. Flush writes buffered data
    #[test]
    fn flush_writes_data() {
        let dir = TempDir::new().unwrap();
        let cfg = config_in(&dir);
        let mut logger = AuditLogger::new(&cfg).unwrap();

        logger.log(AuditEntry::new(
            "s1".into(),
            "sh_run".into(),
            None,
            AuditEvent::ServerStarted,
        ));
        logger.flush();

        let content = read_log(&dir);
        assert!(!content.is_empty());
    }

    // 6. Tilde expansion works in path
    #[test]
    fn tilde_expansion_in_path() {
        let home = std::env::var("HOME").expect("HOME must be set");
        let expanded = expand_tilde("~/some/path/audit.log");
        assert_eq!(expanded, format!("{home}/some/path/audit.log"));
        assert!(!expanded.starts_with('~'));
    }

    // 7. Logger with disabled logging (file = None) doesn't crash
    #[test]
    fn disabled_logger_no_crash() {
        let cfg = AuditConfig {
            log_path: "/nonexistent_root_path_zzz/no/way/audit.log".into(),
            log_level: "info".into(),
            log_commands: true,
            log_policy_decisions: true,
            log_handoff_events: true,
        };

        // new() should succeed even though the file can't be opened.
        let mut logger = AuditLogger::new(&cfg).unwrap();
        assert!(logger.file.is_none());

        // Logging should not panic.
        logger.log(AuditEntry::new(
            "s1".into(),
            "sh_run".into(),
            None,
            AuditEvent::ServerStarted,
        ));
        logger.flush();
        logger.close();
    }

    // 8. AuditEvent variants serialize correctly to JSON
    #[test]
    fn audit_event_serialization() {
        // ToolCall
        let tc = AuditEvent::ToolCall {
            tool_name: "sh_run".into(),
            cmd: Some("ls -la".into()),
            exit_code: Some(0),
        };
        let j = serde_json::to_value(&tc).unwrap();
        assert_eq!(j["type"], "ToolCall");
        assert_eq!(j["tool_name"], "sh_run");
        assert_eq!(j["cmd"], "ls -la");
        assert_eq!(j["exit_code"], 0);

        // CommandStarted
        let cs = AuditEvent::CommandStarted {
            alias: Some("web".into()),
            pid: 1234,
        };
        let j = serde_json::to_value(&cs).unwrap();
        assert_eq!(j["type"], "CommandStarted");
        assert_eq!(j["alias"], "web");
        assert_eq!(j["pid"], 1234);

        // CommandCompleted
        let cc = AuditEvent::CommandCompleted {
            alias: None,
            exit_code: 0,
            duration_ms: 500,
        };
        let j = serde_json::to_value(&cc).unwrap();
        assert_eq!(j["type"], "CommandCompleted");
        assert!(j["alias"].is_null());
        assert_eq!(j["exit_code"], 0);
        assert_eq!(j["duration_ms"], 500);

        // CommandKilled
        let ck = AuditEvent::CommandKilled {
            alias: Some("bg".into()),
            signal: "SIGTERM".into(),
        };
        let j = serde_json::to_value(&ck).unwrap();
        assert_eq!(j["type"], "CommandKilled");
        assert_eq!(j["signal"], "SIGTERM");

        // CommandTimedOut
        let ct = AuditEvent::CommandTimedOut {
            alias: None,
            timeout_sec: 300,
        };
        let j = serde_json::to_value(&ct).unwrap();
        assert_eq!(j["type"], "CommandTimedOut");
        assert_eq!(j["timeout_sec"], 300);

        // PolicyDecision
        let pd = AuditEvent::PolicyDecision {
            rule: "auto_confirm".into(),
            action: "confirm".into(),
        };
        let j = serde_json::to_value(&pd).unwrap();
        assert_eq!(j["type"], "PolicyDecision");
        assert_eq!(j["rule"], "auto_confirm");

        // HandoffInitiated
        let hi = AuditEvent::HandoffInitiated {
            alias: "web".into(),
            handoff_id: "abc123".into(),
        };
        let j = serde_json::to_value(&hi).unwrap();
        assert_eq!(j["type"], "HandoffInitiated");
        assert_eq!(j["alias"], "web");
        assert_eq!(j["handoff_id"], "abc123");

        // HandoffAttached
        let ha = AuditEvent::HandoffAttached {
            handoff_id: "abc123".into(),
        };
        let j = serde_json::to_value(&ha).unwrap();
        assert_eq!(j["type"], "HandoffAttached");

        // HandoffResolved
        let hr = AuditEvent::HandoffResolved {
            handoff_id: "abc123".into(),
            duration_ms: 45000,
        };
        let j = serde_json::to_value(&hr).unwrap();
        assert_eq!(j["type"], "HandoffResolved");
        assert_eq!(j["duration_ms"], 45000);

        // ServerStarted / ServerShutdown
        let ss = AuditEvent::ServerStarted;
        let j = serde_json::to_value(&ss).unwrap();
        assert_eq!(j["type"], "ServerStarted");

        let sd = AuditEvent::ServerShutdown;
        let j = serde_json::to_value(&sd).unwrap();
        assert_eq!(j["type"], "ServerShutdown");

        // SessionCreated / SessionClosed
        let sc = AuditEvent::SessionCreated {
            session: "s1".into(),
        };
        let j = serde_json::to_value(&sc).unwrap();
        assert_eq!(j["type"], "SessionCreated");
        assert_eq!(j["session"], "s1");

        let scl = AuditEvent::SessionClosed {
            session: "s1".into(),
        };
        let j = serde_json::to_value(&scl).unwrap();
        assert_eq!(j["type"], "SessionClosed");

        // Error
        let err = AuditEvent::Error {
            message: "something broke".into(),
        };
        let j = serde_json::to_value(&err).unwrap();
        assert_eq!(j["type"], "Error");
        assert_eq!(j["message"], "something broke");
    }

    // 9. O_CLOEXEC is set on the file descriptor (Unix)
    #[cfg(unix)]
    #[test]
    fn cloexec_is_set() {
        use std::os::unix::io::AsRawFd;

        let dir = TempDir::new().unwrap();
        let cfg = config_in(&dir);
        let logger = AuditLogger::new(&cfg).unwrap();
        let file = logger.file.as_ref().unwrap();
        let fd = file.as_raw_fd();

        let flags = nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFD).unwrap();
        assert!(
            flags & libc::FD_CLOEXEC != 0,
            "FD_CLOEXEC should be set on audit log fd"
        );
    }

    // 10. Log level filtering works
    #[test]
    fn log_level_filtering() {
        let dir = TempDir::new().unwrap();
        let cfg = AuditConfig {
            log_path: dir.path().join("audit.log").to_string_lossy().to_string(),
            log_level: "warn".into(), // only warn and error
            log_commands: true,
            log_policy_decisions: true,
            log_handoff_events: true,
        };
        let mut logger = AuditLogger::new(&cfg).unwrap();

        // info-level event (ServerStarted) should be filtered out
        logger.log(AuditEntry::new(
            "s1".into(),
            "sh_run".into(),
            None,
            AuditEvent::ServerStarted,
        ));
        // debug-level event (ToolCall) should be filtered out
        logger.log(AuditEntry::new(
            "s1".into(),
            "sh_run".into(),
            None,
            AuditEvent::ToolCall {
                tool_name: "sh_run".into(),
                cmd: None,
                exit_code: None,
            },
        ));
        // warn-level event (CommandKilled) should pass
        logger.log(AuditEntry::new(
            "s1".into(),
            "sh_run".into(),
            Some("server".into()),
            AuditEvent::CommandKilled {
                alias: None,
                signal: "SIGTERM".into(),
            },
        ));
        // error-level event should pass
        logger.log(AuditEntry::new(
            "s1".into(),
            "sh_run".into(),
            None,
            AuditEvent::Error {
                message: "oops".into(),
            },
        ));
        logger.flush();

        let content = read_log(&dir);
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "only warn + error events should be logged");
    }

    // 11. Category flags filter events
    #[test]
    fn category_flag_filtering() {
        let dir = TempDir::new().unwrap();
        let cfg = AuditConfig {
            log_path: dir.path().join("audit.log").to_string_lossy().to_string(),
            log_level: "trace".into(),
            log_commands: false,       // suppress command events
            log_policy_decisions: false, // suppress policy events
            log_handoff_events: false,   // suppress handoff events
        };
        let mut logger = AuditLogger::new(&cfg).unwrap();

        // Command event — suppressed
        logger.log(AuditEntry::new(
            "s1".into(),
            "sh_run".into(),
            Some("ls".into()),
            AuditEvent::CommandStarted {
                alias: None,
                pid: 42,
            },
        ));
        // Policy event — suppressed
        logger.log(AuditEntry::new(
            "s1".into(),
            "sh_run".into(),
            None,
            AuditEvent::PolicyDecision {
                rule: "r".into(),
                action: "a".into(),
            },
        ));
        // Handoff event — suppressed
        logger.log(AuditEntry::new(
            "s1".into(),
            "sh_run".into(),
            None,
            AuditEvent::HandoffInitiated {
                alias: "x".into(),
                handoff_id: "h1".into(),
            },
        ));
        // ServerStarted — always logged
        logger.log(AuditEntry::new(
            "s1".into(),
            "".into(),
            None,
            AuditEvent::ServerStarted,
        ));
        logger.flush();

        let content = read_log(&dir);
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 1, "only ServerStarted should be logged");
    }

    // 12. skip_serializing_if works for command = None
    #[test]
    fn command_none_omitted_from_json() {
        let entry = AuditEntry::new(
            "s1".into(),
            "sh_run".into(),
            None,
            AuditEvent::ServerStarted,
        );
        let json = serde_json::to_string(&entry).unwrap();
        assert!(
            !json.contains("\"command\""),
            "command field should be omitted when None"
        );
    }

    // 13. command = Some is included in JSON
    #[test]
    fn command_some_included_in_json() {
        let entry = AuditEntry::new(
            "s1".into(),
            "sh_run".into(),
            Some("ls -la".into()),
            AuditEvent::ServerStarted,
        );
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"command\""));
        assert!(json.contains("ls -la"));
    }

    // 14. ISO 8601 timestamp format
    #[test]
    fn timestamp_format() {
        let ts = iso8601_now();
        // Should match YYYY-MM-DDTHH:MM:SSZ
        assert!(
            ts.ends_with('Z'),
            "timestamp should end with Z: {ts}"
        );
        assert_eq!(
            ts.len(),
            20,
            "timestamp should be 20 chars (YYYY-MM-DDTHH:MM:SSZ): {ts}"
        );
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[13..14], ":");
        assert_eq!(&ts[16..17], ":");
    }
}
