//! Append-only audit log for tool calls, policy decisions, and process lifecycle events.
//!
//! Each mish session writes to its own JSONL file under an `audit/`
//! subdirectory derived from the configured `log_path`.  For example, if
//! `log_path` is `~/.local/share/mish/audit.log`, session files are
//! created at `~/.local/share/mish/audit/{session_id}.jsonl`.
//!
//! The log file descriptor is opened with O_CLOEXEC so child processes
//! cannot inherit it.

use crate::config::AuditConfig;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
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
        AuditEvent::SessionStart { .. } => "info",
        AuditEvent::SessionEnd { .. } => "info",
        AuditEvent::Error { .. } => "error",
        AuditEvent::CommandRecord { .. } => "info",
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
    SessionStart {
        session_id: String,
        server_version: String,
    },
    SessionEnd {
        session_id: String,
        total_commands: u64,
        total_raw_bytes: u64,
        total_squashed_bytes: u64,
        aggregate_ratio: f64,
        grammars_used: Vec<String>,
        duration_ms: u64,
    },
    Error {
        message: String,
    },
    CommandRecord {
        category: String,
        grammar: Option<String>,
        exit_code: i32,
        wall_ms: u64,
        raw_bytes: u64,
        squashed_bytes: u64,
        compression_ratio: f64,
        safety_action: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_output_sha256: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// SessionEndStats
// ---------------------------------------------------------------------------

/// Aggregated metrics for a session, used to emit `SessionEnd` audit records.
#[derive(Debug, Clone)]
pub struct SessionEndStats {
    pub total_commands: u64,
    pub total_raw_bytes: u64,
    pub total_squashed_bytes: u64,
    pub aggregate_ratio: f64,
    pub grammars_used: Vec<String>,
    pub duration_ms: u64,
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
    /// Directory where session JSONL and raw sidecar files live.
    audit_dir: PathBuf,
    /// Session identifier (used for the sidecar subdirectory).
    session_id: String,
    /// Sequence counter for raw sidecar files, incremented per `log_command_with_raw`.
    seq: u32,
}

impl AuditLogger {
    /// Create a new audit logger for a specific session.
    ///
    /// Derives the audit directory from `config.log_path`: the parent
    /// directory of `log_path` gets an `audit/` subdirectory, and the
    /// session file is `{audit_dir}/{session_id}.jsonl`.
    ///
    /// Creates directories if they don't exist. If the log file cannot be
    /// opened, a warning is printed to stderr but the logger is still
    /// returned (with logging disabled).
    pub fn new(config: &AuditConfig, session_id: &str) -> Result<Self, std::io::Error> {
        let expanded = expand_tilde(&config.log_path);
        let base = Path::new(&expanded)
            .parent()
            .unwrap_or(Path::new("."));
        let audit_dir = base.join("audit");

        // Create audit directory if needed. If this fails, we still
        // attempt to open the file (which will also fail), and gracefully
        // degrade to a disabled logger.
        if !audit_dir.exists() {
            if let Err(e) = std::fs::create_dir_all(&audit_dir) {
                eprintln!(
                    "mish: warning: cannot create audit directory {}: {e}",
                    audit_dir.display()
                );
            }
        }

        let session_file = audit_dir.join(format!("{session_id}.jsonl"));

        // Open with O_APPEND + O_CLOEXEC.
        let file = match Self::open_log_file(&session_file) {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!(
                    "mish: warning: cannot open audit log at {}: {e}",
                    session_file.display()
                );
                None
            }
        };

        Ok(Self {
            file,
            config: config.clone(),
            audit_dir: audit_dir.clone(),
            session_id: session_id.to_string(),
            seq: 0,
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
            | AuditEvent::CommandTimedOut { .. }
            | AuditEvent::CommandRecord { .. } => self.config.log_commands,
            AuditEvent::PolicyDecision { .. } => self.config.log_policy_decisions,
            AuditEvent::HandoffInitiated { .. }
            | AuditEvent::HandoffAttached { .. }
            | AuditEvent::HandoffResolved { .. } => self.config.log_handoff_events,
            // Everything else (ToolCall, Server*, Session*, SessionStart/End, Error)
            // is always logged.
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

    /// Log a `SessionStart` event.
    pub fn log_session_start(&mut self, session_id: &str) {
        let entry = AuditEntry::new(
            session_id.to_string(),
            String::new(),
            None,
            AuditEvent::SessionStart {
                session_id: session_id.to_string(),
                server_version: env!("CARGO_PKG_VERSION").to_string(),
            },
        );
        self.log(entry);
    }

    /// Log a `SessionEnd` event with aggregated metrics.
    pub fn log_session_end(&mut self, session_id: &str, stats: SessionEndStats) {
        let entry = AuditEntry::new(
            session_id.to_string(),
            String::new(),
            None,
            AuditEvent::SessionEnd {
                session_id: session_id.to_string(),
                total_commands: stats.total_commands,
                total_raw_bytes: stats.total_raw_bytes,
                total_squashed_bytes: stats.total_squashed_bytes,
                aggregate_ratio: stats.aggregate_ratio,
                grammars_used: stats.grammars_used,
                duration_ms: stats.duration_ms,
            },
        );
        self.log(entry);
    }

    /// Whether raw sidecar writing is enabled (i.e. `raw_retention != "none"`).
    pub fn raw_sidecar_enabled(&self) -> bool {
        self.config.raw_retention != "none"
    }

    /// Log a `CommandRecord` with optional raw output sidecar.
    ///
    /// When `raw_retention` is enabled and `raw_output` is `Some`:
    /// 1. Increments the internal sequence counter.
    /// 2. Computes SHA-256 of the raw output bytes.
    /// 3. Compresses with zstd and writes to
    ///    `{audit_dir}/{session_id}/{seq:03}.raw.zst`.
    /// 4. Sets `raw_output_sha256` on the `CommandRecord`.
    ///
    /// When disabled or `raw_output` is `None`, the entry is logged without
    /// a sidecar and `raw_output_sha256` is `None`.
    pub fn log_command_with_raw(
        &mut self,
        session: String,
        tool: String,
        command: Option<String>,
        category: String,
        grammar: Option<String>,
        exit_code: i32,
        wall_ms: u64,
        raw_bytes: u64,
        squashed_bytes: u64,
        compression_ratio: f64,
        safety_action: String,
        raw_output: Option<&[u8]>,
    ) {
        let sha256 = if self.raw_sidecar_enabled() {
            if let Some(data) = raw_output {
                self.seq += 1;
                match self.write_raw_sidecar(self.seq, data) {
                    Ok(hash) => Some(hash),
                    Err(e) => {
                        eprintln!(
                            "mish: warning: failed to write raw sidecar for seq {}: {e}",
                            self.seq
                        );
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        let entry = AuditEntry::new(
            session,
            tool,
            command,
            AuditEvent::CommandRecord {
                category,
                grammar,
                exit_code,
                wall_ms,
                raw_bytes,
                squashed_bytes,
                compression_ratio,
                safety_action,
                raw_output_sha256: sha256,
            },
        );
        self.log(entry);
    }

    /// Write a raw output sidecar file: `{audit_dir}/{session_id}/{seq:03}.raw.zst`.
    ///
    /// Returns the hex-encoded SHA-256 of the *uncompressed* raw output.
    fn write_raw_sidecar(&self, seq: u32, data: &[u8]) -> Result<String, std::io::Error> {
        // Compute SHA-256 of the uncompressed data.
        let mut hasher = Sha256::new();
        hasher.update(data);
        let hash = format!("{:x}", hasher.finalize());

        // Create the sidecar directory: {audit_dir}/{session_id}/
        let sidecar_dir = self.audit_dir.join(&self.session_id);
        if !sidecar_dir.exists() {
            std::fs::create_dir_all(&sidecar_dir)?;
        }

        // Compress with zstd and write.
        let sidecar_path = sidecar_dir.join(format!("{seq:03}.raw.zst"));
        let compressed = zstd::encode_all(data, 3)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(&sidecar_path, &compressed)?;

        Ok(hash)
    }

    /// Return the current sequence number (for testing).
    #[cfg(test)]
    pub fn seq(&self) -> u32 {
        self.seq
    }
}

// ---------------------------------------------------------------------------
// Programmatic access — read session audit entries
// ---------------------------------------------------------------------------

/// Compute the session log file path from config + session_id.
pub fn session_log_path(config: &AuditConfig, session_id: &str) -> PathBuf {
    let expanded = expand_tilde(&config.log_path);
    let base = Path::new(&expanded)
        .parent()
        .unwrap_or(Path::new("."));
    base.join("audit").join(format!("{session_id}.jsonl"))
}

/// Read audit entries from a session log file.
///
/// Returns parsed JSONL entries as `Vec<serde_json::Value>`.
/// Returns an empty vec if the file doesn't exist or can't be read.
pub fn read_session_entries(config: &AuditConfig, session_id: &str) -> Vec<serde_json::Value> {
    let path = session_log_path(config, session_id);
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    content
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
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

    /// Default test session ID.
    const TEST_SESSION: &str = "test-session";

    /// Helper: create AuditConfig pointing at a temp directory.
    fn config_in(dir: &TempDir) -> AuditConfig {
        AuditConfig {
            log_path: dir.path().join("audit.log").to_string_lossy().to_string(),
            log_level: "trace".into(),
            log_commands: true,
            log_policy_decisions: true,
            log_handoff_events: true,
            raw_retention: "none".into(),
        }
    }

    /// Read the entire session log file contents.
    fn read_log(dir: &TempDir) -> String {
        read_session_log(dir, TEST_SESSION)
    }

    /// Read the log file for a specific session.
    fn read_session_log(dir: &TempDir, session_id: &str) -> String {
        let mut s = String::new();
        let path = dir.path().join("audit").join(format!("{session_id}.jsonl"));
        if path.exists() {
            File::open(path).unwrap().read_to_string(&mut s).unwrap();
        }
        s
    }

    // 1. Logger creates session file at the correct path
    #[test]
    fn creates_file_at_specified_path() {
        let dir = TempDir::new().unwrap();
        let cfg = config_in(&dir);
        let _logger = AuditLogger::new(&cfg, TEST_SESSION).unwrap();
        assert!(dir.path().join("audit").join(format!("{TEST_SESSION}.jsonl")).exists());
    }

    // 2. Logger creates parent directories (including audit subdirectory)
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
            raw_retention: "none".into(),
        };
        let _logger = AuditLogger::new(&cfg, TEST_SESSION).unwrap();
        assert!(nested.join("audit").join(format!("{TEST_SESSION}.jsonl")).exists());
    }

    // 3. Entries are written in JSON Lines format
    #[test]
    fn writes_json_lines() {
        let dir = TempDir::new().unwrap();
        let cfg = config_in(&dir);
        let mut logger = AuditLogger::new(&cfg, TEST_SESSION).unwrap();

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
        let mut logger = AuditLogger::new(&cfg, TEST_SESSION).unwrap();

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
        let mut logger = AuditLogger::new(&cfg, TEST_SESSION).unwrap();

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
            raw_retention: "none".into(),
        };

        // new() should succeed even though the file can't be opened.
        let mut logger = AuditLogger::new(&cfg, TEST_SESSION).unwrap();
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
        let logger = AuditLogger::new(&cfg, TEST_SESSION).unwrap();
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
            raw_retention: "none".into(),
        };
        let mut logger = AuditLogger::new(&cfg, TEST_SESSION).unwrap();

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
            raw_retention: "none".into(),
        };
        let mut logger = AuditLogger::new(&cfg, TEST_SESSION).unwrap();

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

    // --- CommandRecord tests ---

    /// Helper: build a CommandRecord event with typical values.
    fn sample_command_record() -> AuditEvent {
        AuditEvent::CommandRecord {
            category: "condense".into(),
            grammar: Some("cargo".into()),
            exit_code: 0,
            wall_ms: 1234,
            raw_bytes: 50000,
            squashed_bytes: 5000,
            compression_ratio: 0.10,
            safety_action: "allow".into(),
            raw_output_sha256: None,
        }
    }

    // 15. CommandRecord serializes to JSON with correct type tag
    #[test]
    fn command_record_serializes_with_type_tag() {
        let event = sample_command_record();
        let j = serde_json::to_value(&event).unwrap();

        assert_eq!(j["type"], "CommandRecord");
        assert_eq!(j["category"], "condense");
        assert_eq!(j["grammar"], "cargo");
        assert_eq!(j["exit_code"], 0);
        assert_eq!(j["wall_ms"], 1234);
        assert_eq!(j["raw_bytes"], 50000);
        assert_eq!(j["squashed_bytes"], 5000);
        assert_eq!(j["compression_ratio"], 0.10);
        assert_eq!(j["safety_action"], "allow");
    }

    // 16. CommandRecord with grammar = None serializes correctly
    #[test]
    fn command_record_null_grammar() {
        let event = AuditEvent::CommandRecord {
            category: "passthrough".into(),
            grammar: None,
            exit_code: 1,
            wall_ms: 42,
            raw_bytes: 100,
            squashed_bytes: 100,
            compression_ratio: 1.0,
            safety_action: "allow".into(),
            raw_output_sha256: None,
        };
        let j = serde_json::to_value(&event).unwrap();
        assert_eq!(j["type"], "CommandRecord");
        assert!(j["grammar"].is_null(), "grammar should be null when None");
        assert_eq!(j["category"], "passthrough");
        assert_eq!(j["exit_code"], 1);
    }

    // 17. CommandRecord is logged at info level
    #[test]
    fn command_record_log_level_is_info() {
        let event = sample_command_record();
        assert_eq!(event_log_level(&event), "info");
    }

    // 18. CommandRecord respects log_commands flag (enabled)
    #[test]
    fn command_record_respects_log_commands_enabled() {
        let dir = TempDir::new().unwrap();
        let cfg = config_in(&dir); // log_commands = true
        let mut logger = AuditLogger::new(&cfg, TEST_SESSION).unwrap();

        logger.log(AuditEntry::new(
            "s1".into(),
            "sh_run".into(),
            Some("cargo build".into()),
            sample_command_record(),
        ));
        logger.flush();

        let content = read_log(&dir);
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 1, "CommandRecord should be logged when log_commands=true");

        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["event"]["type"], "CommandRecord");
    }

    // 19. CommandRecord respects log_commands flag (disabled)
    #[test]
    fn command_record_respects_log_commands_disabled() {
        let dir = TempDir::new().unwrap();
        let cfg = AuditConfig {
            log_path: dir.path().join("audit.log").to_string_lossy().to_string(),
            log_level: "trace".into(),
            log_commands: false, // suppress command events
            log_policy_decisions: true,
            log_handoff_events: true,
            raw_retention: "none".into(),
        };
        let mut logger = AuditLogger::new(&cfg, TEST_SESSION).unwrap();

        logger.log(AuditEntry::new(
            "s1".into(),
            "sh_run".into(),
            Some("cargo build".into()),
            sample_command_record(),
        ));
        logger.flush();

        let content = read_log(&dir);
        assert!(
            content.is_empty(),
            "CommandRecord should be suppressed when log_commands=false"
        );
    }

    // 20. CommandRecord appears in log file with all fields
    #[test]
    fn command_record_full_entry_in_log() {
        let dir = TempDir::new().unwrap();
        let cfg = config_in(&dir);
        let mut logger = AuditLogger::new(&cfg, TEST_SESSION).unwrap();

        logger.log(AuditEntry::new(
            "session-42".into(),
            "sh_run".into(),
            Some("cargo test".into()),
            AuditEvent::CommandRecord {
                category: "condense".into(),
                grammar: Some("cargo".into()),
                exit_code: 0,
                wall_ms: 5678,
                raw_bytes: 100_000,
                squashed_bytes: 8_000,
                compression_ratio: 0.08,
                safety_action: "allow".into(),
                raw_output_sha256: None,
            },
        ));
        logger.flush();

        let content = read_log(&dir);
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();

        // Top-level fields
        assert_eq!(parsed["session"], "session-42");
        assert_eq!(parsed["tool"], "sh_run");
        assert_eq!(parsed["command"], "cargo test");
        assert!(parsed["timestamp"].as_str().unwrap().ends_with('Z'));

        // Event fields
        let ev = &parsed["event"];
        assert_eq!(ev["type"], "CommandRecord");
        assert_eq!(ev["category"], "condense");
        assert_eq!(ev["grammar"], "cargo");
        assert_eq!(ev["exit_code"], 0);
        assert_eq!(ev["wall_ms"], 5678);
        assert_eq!(ev["raw_bytes"], 100_000);
        assert_eq!(ev["squashed_bytes"], 8_000);
        assert_eq!(ev["compression_ratio"], 0.08);
        assert_eq!(ev["safety_action"], "allow");
    }

    // 21. CommandRecord filtered by log level (warn threshold)
    #[test]
    fn command_record_filtered_by_log_level() {
        let dir = TempDir::new().unwrap();
        let cfg = AuditConfig {
            log_path: dir.path().join("audit.log").to_string_lossy().to_string(),
            log_level: "warn".into(), // only warn and error
            log_commands: true,
            log_policy_decisions: true,
            log_handoff_events: true,
            raw_retention: "none".into(),
        };
        let mut logger = AuditLogger::new(&cfg, TEST_SESSION).unwrap();

        logger.log(AuditEntry::new(
            "s1".into(),
            "sh_run".into(),
            Some("ls".into()),
            sample_command_record(),
        ));
        logger.flush();

        let content = read_log(&dir);
        assert!(
            content.is_empty(),
            "CommandRecord (info level) should be filtered out at warn threshold"
        );
    }

    // --- Session-based JSONL file tests ---

    // 22. Session file is created at {dir}/audit/{session_id}.jsonl
    #[test]
    fn session_file_created_at_correct_path() {
        let dir = TempDir::new().unwrap();
        let cfg = config_in(&dir);
        let sid = "my-session-123";
        let _logger = AuditLogger::new(&cfg, sid).unwrap();

        let expected = dir.path().join("audit").join("my-session-123.jsonl");
        assert!(
            expected.exists(),
            "session file should exist at {}",
            expected.display()
        );
    }

    // 23. Two loggers with different session IDs create separate files
    #[test]
    fn separate_session_files() {
        let dir = TempDir::new().unwrap();
        let cfg = config_in(&dir);

        let mut logger_a = AuditLogger::new(&cfg, "session-a").unwrap();
        let mut logger_b = AuditLogger::new(&cfg, "session-b").unwrap();

        logger_a.log(AuditEntry::new(
            "session-a".into(),
            "sh_run".into(),
            Some("ls".into()),
            AuditEvent::ServerStarted,
        ));
        logger_b.log(AuditEntry::new(
            "session-b".into(),
            "sh_run".into(),
            Some("pwd".into()),
            AuditEvent::ServerShutdown,
        ));
        logger_a.flush();
        logger_b.flush();

        let content_a = read_session_log(&dir, "session-a");
        let content_b = read_session_log(&dir, "session-b");

        assert!(!content_a.is_empty(), "session-a log should not be empty");
        assert!(!content_b.is_empty(), "session-b log should not be empty");

        // Each file should contain exactly one entry
        assert_eq!(content_a.lines().count(), 1);
        assert_eq!(content_b.lines().count(), 1);

        // Verify entries are in the correct files
        let parsed_a: serde_json::Value = serde_json::from_str(content_a.trim()).unwrap();
        assert_eq!(parsed_a["session"], "session-a");
        assert_eq!(parsed_a["command"], "ls");

        let parsed_b: serde_json::Value = serde_json::from_str(content_b.trim()).unwrap();
        assert_eq!(parsed_b["session"], "session-b");
        assert_eq!(parsed_b["command"], "pwd");
    }

    // 24. Entries are written to the correct session file (not to old audit.log)
    #[test]
    fn entries_written_to_session_file_not_old_path() {
        let dir = TempDir::new().unwrap();
        let cfg = config_in(&dir);
        let sid = "correct-path-test";
        let mut logger = AuditLogger::new(&cfg, sid).unwrap();

        logger.log(AuditEntry::new(
            "s1".into(),
            "sh_run".into(),
            None,
            AuditEvent::ServerStarted,
        ));
        logger.flush();

        // The old audit.log should NOT exist
        assert!(
            !dir.path().join("audit.log").exists(),
            "old audit.log should not be created"
        );

        // The session file should have the entry
        let content = read_session_log(&dir, sid);
        assert!(!content.is_empty(), "session file should have entries");
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(parsed["event"]["type"], "ServerStarted");
    }

    // 25. Audit directory is an `audit/` subdirectory of log_path's parent
    #[test]
    fn audit_dir_is_subdirectory_of_log_path_parent() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("custom").join("data");
        let cfg = AuditConfig {
            log_path: nested.join("my.log").to_string_lossy().to_string(),
            log_level: "trace".into(),
            log_commands: true,
            log_policy_decisions: true,
            log_handoff_events: true,
            raw_retention: "none".into(),
        };
        let sid = "subdir-test";
        let _logger = AuditLogger::new(&cfg, sid).unwrap();

        // The audit directory should be {nested}/audit/
        let expected_dir = nested.join("audit");
        assert!(expected_dir.is_dir(), "audit/ subdirectory should exist");

        let expected_file = expected_dir.join("subdir-test.jsonl");
        assert!(expected_file.exists(), "session file should exist");
    }

    // --- SessionStart/SessionEnd tests ---

    // 26. SessionStart serializes correctly with type tag
    #[test]
    fn session_start_serialization() {
        let event = AuditEvent::SessionStart {
            session_id: "sess-abc".into(),
            server_version: "0.1.0".into(),
        };
        let j = serde_json::to_value(&event).unwrap();
        assert_eq!(j["type"], "SessionStart");
        assert_eq!(j["session_id"], "sess-abc");
        assert_eq!(j["server_version"], "0.1.0");
    }

    // 27. SessionEnd serializes correctly with all aggregate fields
    #[test]
    fn session_end_serialization() {
        let event = AuditEvent::SessionEnd {
            session_id: "sess-xyz".into(),
            total_commands: 42,
            total_raw_bytes: 100_000,
            total_squashed_bytes: 25_000,
            aggregate_ratio: 0.25,
            grammars_used: vec!["git".into(), "cargo".into(), "npm".into()],
            duration_ms: 120_000,
        };
        let j = serde_json::to_value(&event).unwrap();
        assert_eq!(j["type"], "SessionEnd");
        assert_eq!(j["session_id"], "sess-xyz");
        assert_eq!(j["total_commands"], 42);
        assert_eq!(j["total_raw_bytes"], 100_000);
        assert_eq!(j["total_squashed_bytes"], 25_000);
        assert_eq!(j["aggregate_ratio"], 0.25);
        assert_eq!(j["duration_ms"], 120_000);
        let grammars = j["grammars_used"].as_array().unwrap();
        assert_eq!(grammars.len(), 3);
        assert_eq!(grammars[0], "git");
        assert_eq!(grammars[1], "cargo");
        assert_eq!(grammars[2], "npm");
    }

    // 28. SessionEnd with empty grammars_used serializes as empty array
    #[test]
    fn session_end_empty_grammars() {
        let event = AuditEvent::SessionEnd {
            session_id: "sess-empty".into(),
            total_commands: 0,
            total_raw_bytes: 0,
            total_squashed_bytes: 0,
            aggregate_ratio: 0.0,
            grammars_used: vec![],
            duration_ms: 500,
        };
        let j = serde_json::to_value(&event).unwrap();
        assert_eq!(j["type"], "SessionEnd");
        let grammars = j["grammars_used"].as_array().unwrap();
        assert!(grammars.is_empty(), "grammars_used should be empty array");
    }

    // 29. SessionStart and SessionEnd respect log level filtering
    #[test]
    fn session_start_end_log_level_filtering() {
        let dir = TempDir::new().unwrap();
        let cfg = AuditConfig {
            log_path: dir.path().join("audit.log").to_string_lossy().to_string(),
            log_level: "warn".into(), // only warn and error pass
            log_commands: true,
            log_policy_decisions: true,
            log_handoff_events: true,
            raw_retention: "none".into(),
        };
        let mut logger = AuditLogger::new(&cfg, TEST_SESSION).unwrap();

        logger.log(AuditEntry::new(
            "s1".into(),
            "".into(),
            None,
            AuditEvent::SessionStart {
                session_id: "s1".into(),
                server_version: "0.1.0".into(),
            },
        ));
        logger.log(AuditEntry::new(
            "s1".into(),
            "".into(),
            None,
            AuditEvent::SessionEnd {
                session_id: "s1".into(),
                total_commands: 10,
                total_raw_bytes: 5000,
                total_squashed_bytes: 1000,
                aggregate_ratio: 0.2,
                grammars_used: vec!["git".into()],
                duration_ms: 60_000,
            },
        ));
        logger.flush();

        let content = read_log(&dir);
        assert!(content.is_empty(), "info-level session events should be filtered at warn threshold");
    }

    // 30. Convenience method log_session_start produces correct entry
    #[test]
    fn log_session_start_convenience() {
        let dir = TempDir::new().unwrap();
        let cfg = config_in(&dir);
        let mut logger = AuditLogger::new(&cfg, "sess-conv").unwrap();

        logger.log_session_start("sess-conv");
        logger.flush();

        let content = read_session_log(&dir, "sess-conv");
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(parsed["session"], "sess-conv");
        assert_eq!(parsed["event"]["type"], "SessionStart");
        assert_eq!(parsed["event"]["session_id"], "sess-conv");
        assert_eq!(parsed["event"]["server_version"], env!("CARGO_PKG_VERSION"));
    }

    // 31. Convenience method log_session_end produces correct entry
    #[test]
    fn log_session_end_convenience() {
        let dir = TempDir::new().unwrap();
        let cfg = config_in(&dir);
        let mut logger = AuditLogger::new(&cfg, "sess-end").unwrap();

        let stats = SessionEndStats {
            total_commands: 7,
            total_raw_bytes: 50_000,
            total_squashed_bytes: 12_000,
            aggregate_ratio: 0.24,
            grammars_used: vec!["cargo".into(), "git".into()],
            duration_ms: 90_000,
        };
        logger.log_session_end("sess-end", stats);
        logger.flush();

        let content = read_session_log(&dir, "sess-end");
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(parsed["session"], "sess-end");
        assert_eq!(parsed["event"]["type"], "SessionEnd");
        assert_eq!(parsed["event"]["session_id"], "sess-end");
        assert_eq!(parsed["event"]["total_commands"], 7);
        assert_eq!(parsed["event"]["total_raw_bytes"], 50_000);
        assert_eq!(parsed["event"]["total_squashed_bytes"], 12_000);
        assert_eq!(parsed["event"]["aggregate_ratio"], 0.24);
        assert_eq!(parsed["event"]["duration_ms"], 90_000);
        let grammars = parsed["event"]["grammars_used"].as_array().unwrap();
        assert_eq!(grammars.len(), 2);
        assert_eq!(grammars[0], "cargo");
        assert_eq!(grammars[1], "git");
    }

    // 32. log_session_end with computed aggregate_ratio
    #[test]
    fn log_session_end_computed_ratio() {
        let dir = TempDir::new().unwrap();
        let cfg = config_in(&dir);
        let mut logger = AuditLogger::new(&cfg, "sess-ratio").unwrap();

        let raw = 80_000_u64;
        let squashed = 20_000_u64;
        let ratio = squashed as f64 / raw as f64;

        let stats = SessionEndStats {
            total_commands: 15,
            total_raw_bytes: raw,
            total_squashed_bytes: squashed,
            aggregate_ratio: ratio,
            grammars_used: vec!["make".into()],
            duration_ms: 45_000,
        };
        logger.log_session_end("sess-ratio", stats);
        logger.flush();

        let content = read_session_log(&dir, "sess-ratio");
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        let logged_ratio = parsed["event"]["aggregate_ratio"].as_f64().unwrap();
        assert!(
            (logged_ratio - 0.25).abs() < 1e-10,
            "aggregate_ratio should be 0.25 (20000/80000), got {logged_ratio}"
        );
    }

    // 33. SessionStart and SessionEnd always pass category filtering
    #[test]
    fn session_start_end_always_pass_category_filter() {
        let dir = TempDir::new().unwrap();
        let cfg = AuditConfig {
            log_path: dir.path().join("audit.log").to_string_lossy().to_string(),
            log_level: "trace".into(),
            log_commands: false,
            log_policy_decisions: false,
            log_handoff_events: false,
            raw_retention: "none".into(),
        };
        let mut logger = AuditLogger::new(&cfg, "sess-cat").unwrap();

        logger.log_session_start("sess-cat");
        logger.log_session_end("sess-cat", SessionEndStats {
            total_commands: 1,
            total_raw_bytes: 100,
            total_squashed_bytes: 50,
            aggregate_ratio: 0.5,
            grammars_used: vec![],
            duration_ms: 1000,
        });
        logger.flush();

        let content = read_session_log(&dir, "sess-cat");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "SessionStart and SessionEnd should always be logged regardless of category flags");
    }

    // --- Raw Output Sidecar tests ---

    /// Helper: create AuditConfig with raw sidecar enabled.
    fn config_with_raw(dir: &TempDir) -> AuditConfig {
        AuditConfig {
            log_path: dir.path().join("audit.log").to_string_lossy().to_string(),
            log_level: "trace".into(),
            log_commands: true,
            log_policy_decisions: true,
            log_handoff_events: true,
            raw_retention: "7d".into(),
        }
    }

    // 32. raw_sidecar_enabled returns false for "none"
    #[test]
    fn raw_sidecar_disabled_by_default() {
        let dir = TempDir::new().unwrap();
        let cfg = config_in(&dir);
        let logger = AuditLogger::new(&cfg, TEST_SESSION).unwrap();
        assert!(!logger.raw_sidecar_enabled());
    }

    // 33. raw_sidecar_enabled returns true for non-"none" values
    #[test]
    fn raw_sidecar_enabled_when_configured() {
        let dir = TempDir::new().unwrap();
        let cfg = config_with_raw(&dir);
        let logger = AuditLogger::new(&cfg, TEST_SESSION).unwrap();
        assert!(logger.raw_sidecar_enabled());
    }

    // 34. Sidecar file is created at correct path
    #[test]
    fn sidecar_file_created_at_correct_path() {
        let dir = TempDir::new().unwrap();
        let cfg = config_with_raw(&dir);
        let mut logger = AuditLogger::new(&cfg, "sidecar-test").unwrap();

        let raw = b"hello world raw output";
        logger.log_command_with_raw(
            "sidecar-test".into(),
            "sh_run".into(),
            Some("echo hello".into()),
            "condense".into(),
            None,
            0,
            50,
            raw.len() as u64,
            5,
            0.23,
            "allow".into(),
            Some(raw),
        );
        logger.flush();

        let sidecar_path = dir.path().join("audit").join("sidecar-test").join("001.raw.zst");
        assert!(
            sidecar_path.exists(),
            "sidecar file should exist at {}",
            sidecar_path.display()
        );
    }

    // 35. SHA-256 in CommandRecord matches decompressed sidecar content
    #[test]
    fn sha256_matches_decompressed_content() {
        let dir = TempDir::new().unwrap();
        let cfg = config_with_raw(&dir);
        let mut logger = AuditLogger::new(&cfg, "sha-test").unwrap();

        let raw = b"this is the raw command output\nwith multiple lines\n";
        logger.log_command_with_raw(
            "sha-test".into(),
            "sh_run".into(),
            Some("cat file.txt".into()),
            "condense".into(),
            Some("cat".into()),
            0,
            100,
            raw.len() as u64,
            10,
            0.20,
            "allow".into(),
            Some(raw),
        );
        logger.flush();

        // Read the JSONL entry and extract raw_output_sha256
        let content = read_session_log(&dir, "sha-test");
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        let sha256_from_log = parsed["event"]["raw_output_sha256"]
            .as_str()
            .expect("raw_output_sha256 should be present");

        // Decompress the sidecar and verify SHA-256
        let sidecar_path = dir.path().join("audit").join("sha-test").join("001.raw.zst");
        let compressed = std::fs::read(&sidecar_path).unwrap();
        let decompressed = zstd::decode_all(compressed.as_slice()).unwrap();
        assert_eq!(
            decompressed, raw,
            "decompressed content should match original raw output"
        );

        // Compute SHA-256 of decompressed and compare
        let mut hasher = Sha256::new();
        hasher.update(&decompressed);
        let computed_sha256 = format!("{:x}", hasher.finalize());
        assert_eq!(
            sha256_from_log, computed_sha256,
            "SHA-256 in log should match decompressed content"
        );
    }

    // 36. Disabled raw_retention skips sidecar creation
    #[test]
    fn disabled_config_skips_sidecar() {
        let dir = TempDir::new().unwrap();
        let cfg = config_in(&dir); // raw_retention = "none"
        let mut logger = AuditLogger::new(&cfg, "no-sidecar").unwrap();

        let raw = b"should not be written";
        logger.log_command_with_raw(
            "no-sidecar".into(),
            "sh_run".into(),
            Some("ls".into()),
            "condense".into(),
            None,
            0,
            10,
            raw.len() as u64,
            5,
            0.25,
            "allow".into(),
            Some(raw),
        );
        logger.flush();

        // No sidecar directory should exist
        let sidecar_dir = dir.path().join("audit").join("no-sidecar");
        assert!(
            !sidecar_dir.exists(),
            "sidecar directory should not exist when raw_retention=none"
        );

        // JSONL entry should not have raw_output_sha256
        let content = read_session_log(&dir, "no-sidecar");
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert!(
            parsed["event"]["raw_output_sha256"].is_null(),
            "raw_output_sha256 should be null when disabled"
        );
    }

    // 37. zstd roundtrip: compress then decompress matches original
    #[test]
    fn zstd_roundtrip() {
        let original = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
            Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. \
            Repeated line for compression benefit.\n\
            Repeated line for compression benefit.\n\
            Repeated line for compression benefit.\n";

        let compressed = zstd::encode_all(original.as_slice(), 3).unwrap();
        let decompressed = zstd::decode_all(compressed.as_slice()).unwrap();

        assert_eq!(decompressed, original.as_slice());
        // Compressed should be smaller than original for repetitive data
        assert!(
            compressed.len() < original.len(),
            "compressed ({}) should be smaller than original ({})",
            compressed.len(),
            original.len()
        );
    }

    // 38. Multiple commands increment sequence numbers correctly
    #[test]
    fn multiple_commands_increment_seq() {
        let dir = TempDir::new().unwrap();
        let cfg = config_with_raw(&dir);
        let mut logger = AuditLogger::new(&cfg, "seq-test").unwrap();

        for i in 1..=3 {
            let raw = format!("output from command {i}");
            logger.log_command_with_raw(
                "seq-test".into(),
                "sh_run".into(),
                Some(format!("cmd{i}")),
                "condense".into(),
                None,
                0,
                10 * i as u64,
                raw.len() as u64,
                5,
                0.5,
                "allow".into(),
                Some(raw.as_bytes()),
            );
        }
        logger.flush();

        assert_eq!(logger.seq(), 3);

        // Check that all three sidecar files exist
        let sidecar_dir = dir.path().join("audit").join("seq-test");
        assert!(sidecar_dir.join("001.raw.zst").exists());
        assert!(sidecar_dir.join("002.raw.zst").exists());
        assert!(sidecar_dir.join("003.raw.zst").exists());

        // Verify contents of each
        for i in 1..=3u32 {
            let path = sidecar_dir.join(format!("{i:03}.raw.zst"));
            let compressed = std::fs::read(&path).unwrap();
            let decompressed = zstd::decode_all(compressed.as_slice()).unwrap();
            let expected = format!("output from command {i}");
            assert_eq!(
                String::from_utf8(decompressed).unwrap(),
                expected,
                "sidecar {i:03} should contain correct content"
            );
        }
    }

    // 39. log_command_with_raw with raw_output=None does not create sidecar
    #[test]
    fn raw_output_none_no_sidecar() {
        let dir = TempDir::new().unwrap();
        let cfg = config_with_raw(&dir); // enabled, but no raw_output
        let mut logger = AuditLogger::new(&cfg, "no-raw").unwrap();

        logger.log_command_with_raw(
            "no-raw".into(),
            "sh_run".into(),
            Some("ls".into()),
            "condense".into(),
            None,
            0,
            10,
            100,
            50,
            0.5,
            "allow".into(),
            None, // no raw output
        );
        logger.flush();

        let sidecar_dir = dir.path().join("audit").join("no-raw");
        assert!(
            !sidecar_dir.exists(),
            "sidecar directory should not exist when raw_output is None"
        );

        // Seq should not be incremented
        assert_eq!(logger.seq(), 0);
    }

    // 40. Sidecar with empty raw output creates valid file
    #[test]
    fn sidecar_empty_raw_output() {
        let dir = TempDir::new().unwrap();
        let cfg = config_with_raw(&dir);
        let mut logger = AuditLogger::new(&cfg, "empty-raw").unwrap();

        let raw = b"";
        logger.log_command_with_raw(
            "empty-raw".into(),
            "sh_run".into(),
            Some("true".into()),
            "condense".into(),
            None,
            0,
            1,
            0,
            0,
            0.0,
            "allow".into(),
            Some(raw),
        );
        logger.flush();

        let sidecar_path = dir.path().join("audit").join("empty-raw").join("001.raw.zst");
        assert!(sidecar_path.exists(), "sidecar should exist even for empty output");

        let compressed = std::fs::read(&sidecar_path).unwrap();
        let decompressed = zstd::decode_all(compressed.as_slice()).unwrap();
        assert!(decompressed.is_empty(), "decompressed empty input should be empty");
    }

    // 41. raw_output_sha256 is omitted from JSON when None (skip_serializing_if)
    #[test]
    fn raw_output_sha256_omitted_when_none() {
        let event = AuditEvent::CommandRecord {
            category: "condense".into(),
            grammar: None,
            exit_code: 0,
            wall_ms: 10,
            raw_bytes: 100,
            squashed_bytes: 50,
            compression_ratio: 0.5,
            safety_action: "allow".into(),
            raw_output_sha256: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            !json.contains("raw_output_sha256"),
            "raw_output_sha256 should be omitted when None"
        );
    }

    // 42. raw_output_sha256 is present in JSON when Some
    #[test]
    fn raw_output_sha256_present_when_some() {
        let event = AuditEvent::CommandRecord {
            category: "condense".into(),
            grammar: None,
            exit_code: 0,
            wall_ms: 10,
            raw_bytes: 100,
            squashed_bytes: 50,
            compression_ratio: 0.5,
            safety_action: "allow".into(),
            raw_output_sha256: Some("abc123def456".into()),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"raw_output_sha256\":\"abc123def456\""));
    }
}
