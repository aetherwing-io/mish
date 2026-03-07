//! Configuration system for mish.
//!
//! Parses and validates `mish.toml`. Config is read once at startup,
//! immutable for server lifetime. Both CLI proxy and MCP server modes use this.

use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;

// ---------------------------------------------------------------------------
// App Profiles — agent IPC submit semantics per TUI application
// ---------------------------------------------------------------------------

/// Input framing mode for an app profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputMode {
    /// Standard line input — submit_sequence appended after input.
    Line,
    /// Bracketed paste — input wrapped in ESC[200~ ... ESC[201~ then submit.
    BracketedPaste,
}

/// Profile describing how to submit input to a specific TUI app and
/// detect when its turn is complete.
#[derive(Debug, Clone)]
pub struct AppProfile {
    pub name: String,
    pub submit_sequence: String,
    pub prompt_pattern: String,
    pub input_mode: InputMode,
}

impl AppProfile {
    /// Wrap input according to the profile's input mode.
    /// Returns a string with key tokens (e.g. `<enter>`) ready for `expand_keys()`.
    pub fn wrap_input(&self, input: &str) -> String {
        match self.input_mode {
            InputMode::Line => {
                format!("{input}{}", self.submit_sequence)
            }
            InputMode::BracketedPaste => {
                // ESC[200~ starts paste mode, ESC[201~ ends it, then submit
                format!(
                    "\x1b[200~{input}\x1b[201~{}",
                    self.submit_sequence
                )
            }
        }
    }
}

/// Return built-in app profiles.
pub fn builtin_profiles() -> Vec<AppProfile> {
    vec![
        AppProfile {
            name: "claude".to_string(),
            submit_sequence: "<enter>".to_string(),
            prompt_pattern: r"❯\s*$".to_string(),
            input_mode: InputMode::BracketedPaste,
        },
        AppProfile {
            name: "gemini".to_string(),
            submit_sequence: "<enter>".to_string(),
            prompt_pattern: r">\s*$".to_string(),
            input_mode: InputMode::Line,
        },
        AppProfile {
            name: "generic".to_string(),
            submit_sequence: "<enter>".to_string(),
            prompt_pattern: r"[$#>❯]\s*$".to_string(),
            input_mode: InputMode::Line,
        },
    ]
}

/// Look up a profile by name. Falls back to "generic" if not found.
pub fn resolve_profile(name: Option<&str>) -> AppProfile {
    let profiles = builtin_profiles();
    let target = name.unwrap_or("generic");
    profiles
        .into_iter()
        .find(|p| p.name == target)
        .unwrap_or_else(|| {
            // Fallback to generic
            AppProfile {
                name: "generic".to_string(),
                submit_sequence: "<enter>".to_string(),
                prompt_pattern: r"[$#>❯]\s*$".to_string(),
                input_mode: InputMode::Line,
            }
        })
}

// ---------------------------------------------------------------------------
// ConfigError
// ---------------------------------------------------------------------------

/// Errors that can occur when loading configuration.
#[derive(Debug)]
pub enum ConfigError {
    IoError(std::io::Error),
    ParseError(String),
    ValidationError(Vec<String>),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::IoError(e) => write!(f, "config I/O error: {e}"),
            ConfigError::ParseError(e) => write!(f, "config parse error: {e}"),
            ConfigError::ValidationError(errs) => {
                write!(f, "config validation errors: {}", errs.join("; "))
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::IoError(e)
    }
}

// ---------------------------------------------------------------------------
// Public config structs
// ---------------------------------------------------------------------------

/// Top-level mish configuration.
#[derive(Debug, Clone, Default)]
pub struct MishConfig {
    pub server: ServerConfig,
    pub squasher: SquasherConfig,
    pub yield_config: YieldConfig,
    pub timeout_defaults: TimeoutDefaults,
    pub watch_presets: HashMap<String, String>,
    pub audit: AuditConfig,
    pub policy: PolicyConfig,
    pub handoff: HandoffConfig,
    pub sandbox: Option<SandboxConfig>,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub max_sessions: usize,
    pub max_processes: usize,
    pub max_spool_bytes_total: usize,
    pub idle_session_timeout_sec: u64,
}

#[derive(Debug, Clone)]
pub struct SquasherConfig {
    pub max_lines: usize,
    pub max_bytes: usize,
    pub oreo_head: usize,
    pub oreo_tail: usize,
    pub spool_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct YieldConfig {
    pub silence_timeout_ms: u64,
    pub prompt_patterns: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TimeoutDefaults {
    pub default: u64,
    pub scope: HashMap<String, u64>,
}

#[derive(Debug, Clone)]
pub struct AuditConfig {
    pub log_path: String,
    pub log_level: String,
    pub log_commands: bool,
    pub log_policy_decisions: bool,
    pub log_handoff_events: bool,
    /// Raw output sidecar retention.  `"none"` disables sidecar creation.
    /// Any other value (e.g. `"7d"`, `"30d"`) enables it; retention
    /// enforcement is handled externally — this field controls only whether
    /// raw sidecar files are written at all.
    pub raw_retention: String,
}

#[derive(Debug, Clone)]
#[derive(Default)]
pub struct PolicyConfig {
    pub auto_confirm: Vec<AutoConfirmRule>,
    pub yield_to_operator: Vec<YieldToOperatorRule>,
    pub forbidden: Vec<ForbiddenRule>,
}

#[derive(Debug, Clone)]
pub struct AutoConfirmRule {
    pub match_pattern: String,
    pub respond: String,
    pub scope: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct YieldToOperatorRule {
    pub match_pattern: String,
    pub notify: bool,
}

#[derive(Debug, Clone)]
pub struct ForbiddenRule {
    pub pattern: String,
    pub action: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct HandoffConfig {
    pub timeout_sec: u64,
    pub fallback: String,
}

#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub enabled: bool,
    pub allowed_paths: Vec<String>,
    pub readonly_paths: Vec<String>,
    pub network: String,
    pub max_pids: Option<u32>,
    pub max_memory_mb: Option<u32>,
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_sessions: 12,
            max_processes: 50,
            max_spool_bytes_total: 209_715_200, // 200 MB
            idle_session_timeout_sec: 3600,
        }
    }
}

impl Default for SquasherConfig {
    fn default() -> Self {
        Self {
            max_lines: 200,
            max_bytes: 65_536, // 64 KB
            oreo_head: 50,
            oreo_tail: 150,
            spool_bytes: 4_194_304, // 4 MB
        }
    }
}

impl Default for YieldConfig {
    fn default() -> Self {
        Self {
            silence_timeout_ms: 2500,
            prompt_patterns: vec![
                "[?]$".into(),
                ":$".into(),
                ">$".into(),
                "Password".into(),
                "passphrase".into(),
                "[y/N]".into(),
                "[Y/n]".into(),
            ],
        }
    }
}

impl Default for TimeoutDefaults {
    fn default() -> Self {
        let mut scope = HashMap::new();
        scope.insert("terraform".into(), 1800);
        scope.insert("docker".into(), 600);
        scope.insert("cargo".into(), 600);
        scope.insert("npm".into(), 300);
        scope.insert("pip".into(), 300);
        Self {
            default: 300,
            scope,
        }
    }
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            log_path: "~/.local/share/mish/audit.log".into(),
            log_level: "info".into(),
            log_commands: true,
            log_policy_decisions: true,
            log_handoff_events: true,
            raw_retention: "none".into(),
        }
    }
}


impl Default for HandoffConfig {
    fn default() -> Self {
        Self {
            timeout_sec: 900,
            fallback: "yield_to_llm".into(),
        }
    }
}


// ---------------------------------------------------------------------------
// Serde intermediate structs (raw TOML representation)
// ---------------------------------------------------------------------------

/// Raw TOML document. Uses `#[serde(default)]` so every section is optional.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawConfig {
    server: RawServerConfig,
    squasher: RawSquasherConfig,
    #[serde(rename = "yield")]
    yield_section: RawYieldConfig,
    timeout_defaults: RawTimeoutDefaults,
    watch_presets: HashMap<String, String>,
    audit: RawAuditConfig,
    policy: RawPolicyConfig,
    handoff: RawHandoffConfig,
    sandbox: Option<RawSandboxConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RawServerConfig {
    max_sessions: usize,
    max_processes: usize,
    max_spool_bytes_total: usize,
    idle_session_timeout_sec: u64,
}

impl Default for RawServerConfig {
    fn default() -> Self {
        let d = ServerConfig::default();
        Self {
            max_sessions: d.max_sessions,
            max_processes: d.max_processes,
            max_spool_bytes_total: d.max_spool_bytes_total,
            idle_session_timeout_sec: d.idle_session_timeout_sec,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RawSquasherConfig {
    max_lines: usize,
    max_bytes: usize,
    oreo_head: usize,
    oreo_tail: usize,
    spool_bytes: usize,
}

impl Default for RawSquasherConfig {
    fn default() -> Self {
        let d = SquasherConfig::default();
        Self {
            max_lines: d.max_lines,
            max_bytes: d.max_bytes,
            oreo_head: d.oreo_head,
            oreo_tail: d.oreo_tail,
            spool_bytes: d.spool_bytes,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RawYieldConfig {
    silence_timeout_ms: u64,
    prompt_patterns: Vec<String>,
}

impl Default for RawYieldConfig {
    fn default() -> Self {
        let d = YieldConfig::default();
        Self {
            silence_timeout_ms: d.silence_timeout_ms,
            prompt_patterns: d.prompt_patterns,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RawTimeoutDefaults {
    default: u64,
    scope: HashMap<String, u64>,
}

impl Default for RawTimeoutDefaults {
    fn default() -> Self {
        let d = TimeoutDefaults::default();
        Self {
            default: d.default,
            scope: d.scope,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RawAuditConfig {
    log_path: String,
    log_level: String,
    log_commands: bool,
    log_policy_decisions: bool,
    log_handoff_events: bool,
    raw_retention: String,
}

impl Default for RawAuditConfig {
    fn default() -> Self {
        let d = AuditConfig::default();
        Self {
            log_path: d.log_path,
            log_level: d.log_level,
            log_commands: d.log_commands,
            log_policy_decisions: d.log_policy_decisions,
            log_handoff_events: d.log_handoff_events,
            raw_retention: d.raw_retention,
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawPolicyConfig {
    auto_confirm: Vec<RawAutoConfirmRule>,
    yield_to_operator: Vec<RawYieldToOperatorRule>,
    forbidden: Vec<RawForbiddenRule>,
}

#[derive(Debug, Deserialize)]
struct RawAutoConfirmRule {
    #[serde(rename = "match")]
    match_pattern: String,
    respond: String,
    scope: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct RawYieldToOperatorRule {
    #[serde(rename = "match")]
    match_pattern: String,
    #[serde(default)]
    notify: bool,
}

#[derive(Debug, Deserialize)]
struct RawForbiddenRule {
    pattern: String,
    action: String,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RawHandoffConfig {
    timeout_sec: u64,
    fallback: String,
}

impl Default for RawHandoffConfig {
    fn default() -> Self {
        let d = HandoffConfig::default();
        Self {
            timeout_sec: d.timeout_sec,
            fallback: d.fallback,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RawSandboxConfig {
    enabled: bool,
    allowed_paths: Vec<String>,
    readonly_paths: Vec<String>,
    network: String,
    max_pids: Option<u32>,
    max_memory_mb: Option<u32>,
}

impl Default for RawSandboxConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allowed_paths: Vec::new(),
            readonly_paths: Vec::new(),
            network: "none".into(),
            max_pids: None,
            max_memory_mb: None,
        }
    }
}

use crate::util::expand_tilde;

// ---------------------------------------------------------------------------
// Conversion: Raw -> Public
// ---------------------------------------------------------------------------

impl From<RawServerConfig> for ServerConfig {
    fn from(r: RawServerConfig) -> Self {
        Self {
            max_sessions: r.max_sessions,
            max_processes: r.max_processes,
            max_spool_bytes_total: r.max_spool_bytes_total,
            idle_session_timeout_sec: r.idle_session_timeout_sec,
        }
    }
}

impl From<RawSquasherConfig> for SquasherConfig {
    fn from(r: RawSquasherConfig) -> Self {
        Self {
            max_lines: r.max_lines,
            max_bytes: r.max_bytes,
            oreo_head: r.oreo_head,
            oreo_tail: r.oreo_tail,
            spool_bytes: r.spool_bytes,
        }
    }
}

impl From<RawYieldConfig> for YieldConfig {
    fn from(r: RawYieldConfig) -> Self {
        Self {
            silence_timeout_ms: r.silence_timeout_ms,
            prompt_patterns: r.prompt_patterns,
        }
    }
}

impl From<RawTimeoutDefaults> for TimeoutDefaults {
    fn from(r: RawTimeoutDefaults) -> Self {
        Self {
            default: r.default,
            scope: r.scope,
        }
    }
}

impl From<RawAuditConfig> for AuditConfig {
    fn from(r: RawAuditConfig) -> Self {
        Self {
            log_path: expand_tilde(&r.log_path),
            log_level: r.log_level,
            log_commands: r.log_commands,
            log_policy_decisions: r.log_policy_decisions,
            log_handoff_events: r.log_handoff_events,
            raw_retention: r.raw_retention,
        }
    }
}

impl From<RawAutoConfirmRule> for AutoConfirmRule {
    fn from(r: RawAutoConfirmRule) -> Self {
        Self {
            match_pattern: r.match_pattern,
            respond: r.respond,
            scope: r.scope,
        }
    }
}

impl From<RawYieldToOperatorRule> for YieldToOperatorRule {
    fn from(r: RawYieldToOperatorRule) -> Self {
        Self {
            match_pattern: r.match_pattern,
            notify: r.notify,
        }
    }
}

impl From<RawForbiddenRule> for ForbiddenRule {
    fn from(r: RawForbiddenRule) -> Self {
        Self {
            pattern: r.pattern,
            action: r.action,
            message: r.message,
        }
    }
}

impl From<RawPolicyConfig> for PolicyConfig {
    fn from(r: RawPolicyConfig) -> Self {
        Self {
            auto_confirm: r.auto_confirm.into_iter().map(Into::into).collect(),
            yield_to_operator: r.yield_to_operator.into_iter().map(Into::into).collect(),
            forbidden: r.forbidden.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<RawHandoffConfig> for HandoffConfig {
    fn from(r: RawHandoffConfig) -> Self {
        Self {
            timeout_sec: r.timeout_sec,
            fallback: r.fallback,
        }
    }
}

impl From<RawSandboxConfig> for SandboxConfig {
    fn from(r: RawSandboxConfig) -> Self {
        Self {
            enabled: r.enabled,
            allowed_paths: r.allowed_paths.into_iter().map(|p| expand_tilde(&p)).collect(),
            readonly_paths: r.readonly_paths.into_iter().map(|p| expand_tilde(&p)).collect(),
            network: r.network,
            max_pids: r.max_pids,
            max_memory_mb: r.max_memory_mb,
        }
    }
}

impl From<RawConfig> for MishConfig {
    fn from(r: RawConfig) -> Self {
        Self {
            server: r.server.into(),
            squasher: r.squasher.into(),
            yield_config: r.yield_section.into(),
            timeout_defaults: r.timeout_defaults.into(),
            watch_presets: r.watch_presets,
            audit: r.audit.into(),
            policy: r.policy.into(),
            handoff: r.handoff.into(),
            sandbox: r.sandbox.map(Into::into),
        }
    }
}

// ---------------------------------------------------------------------------
// Unknown-key detection
// ---------------------------------------------------------------------------

/// Known top-level TOML keys.
const KNOWN_SECTIONS: &[&str] = &[
    "server",
    "squasher",
    "yield",
    "timeout_defaults",
    "watch_presets",
    "audit",
    "policy",
    "handoff",
    "sandbox",
];

/// Warn to stderr about unknown top-level sections.
fn warn_unknown_sections(toml_str: &str) {
    // Parse as a generic TOML table to inspect top-level keys.
    if let Ok(table) = toml_str.parse::<toml::Table>() {
        for key in table.keys() {
            if !KNOWN_SECTIONS.contains(&key.as_str()) {
                eprintln!("mish: warning: unknown config section [{key}]");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

const VALID_LOG_LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];

fn validate(config: &MishConfig) -> Vec<String> {
    let mut errors = Vec::new();

    if config.server.max_sessions == 0 {
        errors.push("server.max_sessions must be > 0".into());
    }
    if config.server.max_processes == 0 {
        errors.push("server.max_processes must be > 0".into());
    }
    if config.server.max_spool_bytes_total == 0 {
        errors.push("server.max_spool_bytes_total must be > 0".into());
    }

    if config.squasher.max_lines == 0 {
        errors.push("squasher.max_lines must be > 0".into());
    }
    if config.squasher.max_bytes == 0 {
        errors.push("squasher.max_bytes must be > 0".into());
    }
    if config.squasher.oreo_head == 0 {
        errors.push("squasher.oreo_head must be > 0".into());
    }
    if config.squasher.oreo_tail == 0 {
        errors.push("squasher.oreo_tail must be > 0".into());
    }

    if config.timeout_defaults.default == 0 {
        errors.push("timeout_defaults.default must be > 0".into());
    }

    if !VALID_LOG_LEVELS.contains(&config.audit.log_level.as_str()) {
        errors.push(format!(
            "audit.log_level '{}' is invalid; expected one of: {}",
            config.audit.log_level,
            VALID_LOG_LEVELS.join(", ")
        ));
    }

    errors
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Return the default configuration (no file needed).
pub fn default_config() -> MishConfig {
    MishConfig::default()
}

/// Load configuration from a TOML file path.
///
/// - If the file does not exist, returns `default_config()` (no error).
/// - If the file exists but is malformed, returns `ConfigError::ParseError`.
/// - Warns to stderr about unknown top-level sections.
/// - Runs validation; returns `ConfigError::ValidationError` if any checks fail.
pub fn load_config(path: &str) -> Result<MishConfig, ConfigError> {
    let expanded = expand_tilde(path);
    let content = match std::fs::read_to_string(&expanded) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(default_config());
        }
        Err(e) => return Err(ConfigError::IoError(e)),
    };

    parse_and_validate(&content)
}

/// Validate a config file without returning the parsed config.
///
/// Returns `Ok(())` if valid, or `Err(errors)` listing every validation failure.
pub fn validate_config(path: &str) -> Result<(), Vec<String>> {
    let expanded = expand_tilde(path);
    let content = std::fs::read_to_string(&expanded)
        .map_err(|e| vec![format!("cannot read config: {e}")])?;

    let raw: RawConfig = toml::from_str(&content)
        .map_err(|e| vec![format!("TOML parse error: {e}")])?;

    let config: MishConfig = raw.into();
    let errors = validate(&config);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Parse a TOML string into a validated `MishConfig`.
pub fn load_config_from_str(content: &str) -> Result<MishConfig, ConfigError> {
    parse_and_validate(content)
}

/// Internal: parse TOML string and validate.
fn parse_and_validate(content: &str) -> Result<MishConfig, ConfigError> {
    warn_unknown_sections(content);

    let raw: RawConfig =
        toml::from_str(content).map_err(|e| ConfigError::ParseError(e.to_string()))?;

    let config: MishConfig = raw.into();

    let errors = validate(&config);
    if !errors.is_empty() {
        return Err(ConfigError::ValidationError(errors));
    }

    Ok(config)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 1. Parse valid config with all sections present
    #[test]
    fn parse_full_config() {
        let toml = r#"
[server]
max_sessions = 10
max_processes = 40
max_spool_bytes_total = 104857600
idle_session_timeout_sec = 7200

[squasher]
max_lines = 300
max_bytes = 131072
oreo_head = 100
oreo_tail = 200
spool_bytes = 2097152

[yield]
silence_timeout_ms = 5000
prompt_patterns = ['Password', 'passphrase']

[timeout_defaults]
default = 600

[timeout_defaults.scope]
terraform = 3600
docker = 1200

[watch_presets]
errors = "error|fatal"
warnings = "warn"

[[policy.auto_confirm]]
match = "Continue?"
respond = "Y\n"
scope = ["apt"]

[[policy.yield_to_operator]]
match = "Password"
notify = true

[[policy.forbidden]]
pattern = "rm -rf /"
action = "block"
message = "Blocked"

[handoff]
timeout_sec = 300
fallback = "yield_to_llm"

[audit]
log_path = "/tmp/mish.log"
log_level = "debug"
log_commands = false
log_policy_decisions = false
log_handoff_events = false

[sandbox]
enabled = true
allowed_paths = ["/home/user"]
readonly_paths = ["/etc"]
network = "host"
max_pids = 100
max_memory_mb = 512
"#;

        let config = parse_and_validate(toml).expect("should parse");

        assert_eq!(config.server.max_sessions, 10);
        assert_eq!(config.server.max_processes, 40);
        assert_eq!(config.server.max_spool_bytes_total, 104_857_600);
        assert_eq!(config.server.idle_session_timeout_sec, 7200);

        assert_eq!(config.squasher.max_lines, 300);
        assert_eq!(config.squasher.max_bytes, 131_072);
        assert_eq!(config.squasher.oreo_head, 100);
        assert_eq!(config.squasher.oreo_tail, 200);
        assert_eq!(config.squasher.spool_bytes, 2_097_152);

        assert_eq!(config.yield_config.silence_timeout_ms, 5000);
        assert_eq!(config.yield_config.prompt_patterns.len(), 2);

        assert_eq!(config.timeout_defaults.default, 600);
        assert_eq!(config.timeout_defaults.scope.get("terraform"), Some(&3600));
        assert_eq!(config.timeout_defaults.scope.get("docker"), Some(&1200));

        assert_eq!(config.watch_presets.get("errors"), Some(&"error|fatal".to_string()));

        assert_eq!(config.policy.auto_confirm.len(), 1);
        assert_eq!(config.policy.auto_confirm[0].match_pattern, "Continue?");
        assert_eq!(config.policy.auto_confirm[0].respond, "Y\n");
        assert_eq!(
            config.policy.auto_confirm[0].scope,
            Some(vec!["apt".to_string()])
        );

        assert_eq!(config.policy.yield_to_operator.len(), 1);
        assert!(config.policy.yield_to_operator[0].notify);

        assert_eq!(config.policy.forbidden.len(), 1);
        assert_eq!(config.policy.forbidden[0].pattern, "rm -rf /");
        assert_eq!(config.policy.forbidden[0].action, "block");

        assert_eq!(config.handoff.timeout_sec, 300);
        assert_eq!(config.handoff.fallback, "yield_to_llm");

        assert_eq!(config.audit.log_path, "/tmp/mish.log");
        assert_eq!(config.audit.log_level, "debug");
        assert!(!config.audit.log_commands);
        assert_eq!(config.audit.raw_retention, "none"); // default when not specified

        let sandbox = config.sandbox.as_ref().unwrap();
        assert!(sandbox.enabled);
        assert_eq!(sandbox.allowed_paths, vec!["/home/user"]);
        assert_eq!(sandbox.readonly_paths, vec!["/etc"]);
        assert_eq!(sandbox.network, "host");
        assert_eq!(sandbox.max_pids, Some(100));
        assert_eq!(sandbox.max_memory_mb, Some(512));
    }

    /// 2. Parse config with missing sections (defaults applied)
    #[test]
    fn parse_empty_config_uses_defaults() {
        let toml = "";
        let config = parse_and_validate(toml).expect("should parse");

        let defaults = default_config();
        assert_eq!(config.server.max_sessions, defaults.server.max_sessions);
        assert_eq!(config.squasher.max_lines, defaults.squasher.max_lines);
        assert_eq!(
            config.yield_config.silence_timeout_ms,
            defaults.yield_config.silence_timeout_ms
        );
        assert_eq!(
            config.timeout_defaults.default,
            defaults.timeout_defaults.default
        );
        assert_eq!(config.audit.log_level, defaults.audit.log_level);
        assert_eq!(config.handoff.timeout_sec, defaults.handoff.timeout_sec);
        assert!(config.sandbox.is_none());
    }

    /// 3. Parse config with only `[server]` section
    #[test]
    fn parse_only_server_section() {
        let toml = r#"
[server]
max_sessions = 3
max_processes = 10
"#;
        let config = parse_and_validate(toml).expect("should parse");
        assert_eq!(config.server.max_sessions, 3);
        assert_eq!(config.server.max_processes, 10);
        // Other sections should be defaults
        assert_eq!(
            config.squasher.max_lines,
            SquasherConfig::default().max_lines
        );
        assert_eq!(
            config.yield_config.silence_timeout_ms,
            YieldConfig::default().silence_timeout_ms
        );
    }

    /// 4. Tilde expansion in `audit.log_path`
    #[test]
    fn tilde_expansion_in_audit_path() {
        let toml = r#"
[audit]
log_path = "~/logs/mish.log"
"#;
        let config = parse_and_validate(toml).expect("should parse");

        let home = std::env::var("HOME").expect("HOME should be set");
        assert_eq!(config.audit.log_path, format!("{home}/logs/mish.log"));
        assert!(!config.audit.log_path.starts_with('~'));
    }

    /// 5. Invalid TOML syntax produces clear error
    #[test]
    fn invalid_toml_syntax() {
        let toml = r#"
[server
max_sessions = 5
"#;
        let result = parse_and_validate(toml);
        assert!(result.is_err());
        match result.unwrap_err() {
            ConfigError::ParseError(msg) => {
                assert!(!msg.is_empty(), "parse error message should not be empty");
            }
            other => panic!("expected ParseError, got {other:?}"),
        }
    }

    /// 6. Unknown section produces warning but parses successfully
    #[test]
    fn unknown_section_parses_ok() {
        // `toml::from_str` with `#[serde(default)]` ignores unknown keys,
        // and `warn_unknown_sections` prints the warning to stderr.
        let toml = r#"
[server]
max_sessions = 5

[future_feature]
foo = "bar"
"#;
        let config = parse_and_validate(toml).expect("should parse despite unknown section");
        assert_eq!(config.server.max_sessions, 5);
    }

    /// 7. Missing config file returns default config without error
    #[test]
    fn missing_file_returns_defaults() {
        let config = load_config("/nonexistent/path/mish.toml").expect("should not error");
        let defaults = default_config();
        assert_eq!(config.server.max_sessions, defaults.server.max_sessions);
        assert_eq!(config.squasher.max_lines, defaults.squasher.max_lines);
    }

    /// 8. Validation rejects zero values for limits
    #[test]
    fn validation_rejects_zero_limits() {
        let toml = r#"
[server]
max_sessions = 0
max_processes = 0
max_spool_bytes_total = 0

[squasher]
max_lines = 0
max_bytes = 0
oreo_head = 0
oreo_tail = 0

[timeout_defaults]
default = 0
"#;
        let result = parse_and_validate(toml);
        match result {
            Err(ConfigError::ValidationError(errors)) => {
                assert!(
                    errors.len() >= 7,
                    "expected at least 7 validation errors, got {}: {errors:?}",
                    errors.len()
                );
                assert!(errors.iter().any(|e| e.contains("max_sessions")));
                assert!(errors.iter().any(|e| e.contains("max_processes")));
                assert!(errors.iter().any(|e| e.contains("max_spool_bytes_total")));
                assert!(errors.iter().any(|e| e.contains("max_lines")));
                assert!(errors.iter().any(|e| e.contains("max_bytes")));
                assert!(errors.iter().any(|e| e.contains("oreo_head")));
                assert!(errors.iter().any(|e| e.contains("oreo_tail")));
                assert!(errors.iter().any(|e| e.contains("timeout_defaults.default")));
            }
            other => panic!("expected ValidationError, got {other:?}"),
        }
    }

    /// 9. Validation rejects invalid log_level values
    #[test]
    fn validation_rejects_invalid_log_level() {
        let toml = r#"
[audit]
log_level = "verbose"
"#;
        let result = parse_and_validate(toml);
        match result {
            Err(ConfigError::ValidationError(errors)) => {
                assert_eq!(errors.len(), 1);
                assert!(errors[0].contains("log_level"));
                assert!(errors[0].contains("verbose"));
            }
            other => panic!("expected ValidationError, got {other:?}"),
        }
    }

    /// 10. `validate_config()` returns all errors, not just the first
    #[test]
    fn validate_config_returns_all_errors() {
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        write!(
            tmpfile,
            r#"
[server]
max_sessions = 0
max_processes = 0

[audit]
log_level = "bogus"
"#
        )
        .unwrap();

        let result = validate_config(tmpfile.path().to_str().unwrap());
        match result {
            Err(errors) => {
                assert!(
                    errors.len() >= 3,
                    "expected at least 3 errors, got {}: {errors:?}",
                    errors.len()
                );
                assert!(errors.iter().any(|e| e.contains("max_sessions")));
                assert!(errors.iter().any(|e| e.contains("max_processes")));
                assert!(errors.iter().any(|e| e.contains("log_level")));
            }
            Ok(()) => panic!("expected errors"),
        }
    }

    /// 11. Per-scope timeout overrides parse correctly
    #[test]
    fn per_scope_timeout_overrides() {
        let toml = r#"
[timeout_defaults]
default = 120

[timeout_defaults.scope]
terraform = 3600
docker = 1200
cargo = 900
custom_tool = 60
"#;
        let config = parse_and_validate(toml).expect("should parse");
        assert_eq!(config.timeout_defaults.default, 120);
        assert_eq!(config.timeout_defaults.scope.len(), 4);
        assert_eq!(config.timeout_defaults.scope["terraform"], 3600);
        assert_eq!(config.timeout_defaults.scope["docker"], 1200);
        assert_eq!(config.timeout_defaults.scope["cargo"], 900);
        assert_eq!(config.timeout_defaults.scope["custom_tool"], 60);
    }

    /// 12. Watch presets parse as HashMap
    #[test]
    fn watch_presets_parse() {
        let toml = r#"
[watch_presets]
errors = "error|ERR!|fatal|panic"
warnings = "warn|deprecat"
test_results = "passed|failed|error|skip"
"#;
        let config = parse_and_validate(toml).expect("should parse");
        assert_eq!(config.watch_presets.len(), 3);
        assert_eq!(
            config.watch_presets["errors"],
            "error|ERR!|fatal|panic"
        );
        assert_eq!(config.watch_presets["warnings"], "warn|deprecat");
        assert_eq!(
            config.watch_presets["test_results"],
            "passed|failed|error|skip"
        );
    }

    /// 13. Policy sections parse into structs
    #[test]
    fn policy_sections_parse() {
        let toml = r#"
[[policy.auto_confirm]]
match = "Do you want to continue"
respond = "Y\n"
scope = ["apt", "apt-get"]

[[policy.auto_confirm]]
match = "Proceed?"
respond = "yes\n"

[[policy.yield_to_operator]]
match = '[Pp]assword|MFA|OTP'
notify = true

[[policy.yield_to_operator]]
match = 'token'
notify = false

[[policy.forbidden]]
pattern = "rm -rf /"
action = "block"
message = "Command blocked by policy"

[[policy.forbidden]]
pattern = ":(){ :|:& };:"
action = "block"
message = "Fork bomb blocked"
"#;
        let config = parse_and_validate(toml).expect("should parse");

        // auto_confirm
        assert_eq!(config.policy.auto_confirm.len(), 2);
        assert_eq!(
            config.policy.auto_confirm[0].match_pattern,
            "Do you want to continue"
        );
        assert_eq!(config.policy.auto_confirm[0].respond, "Y\n");
        assert_eq!(
            config.policy.auto_confirm[0].scope,
            Some(vec!["apt".to_string(), "apt-get".to_string()])
        );
        assert_eq!(config.policy.auto_confirm[1].match_pattern, "Proceed?");
        assert!(config.policy.auto_confirm[1].scope.is_none());

        // yield_to_operator
        assert_eq!(config.policy.yield_to_operator.len(), 2);
        assert!(config.policy.yield_to_operator[0].notify);
        assert!(!config.policy.yield_to_operator[1].notify);

        // forbidden
        assert_eq!(config.policy.forbidden.len(), 2);
        assert_eq!(config.policy.forbidden[0].pattern, "rm -rf /");
        assert_eq!(config.policy.forbidden[0].action, "block");
        assert_eq!(config.policy.forbidden[1].pattern, ":(){ :|:& };:");
    }

    /// 14. Config with only defaults matches `default_config()`
    #[test]
    fn empty_config_matches_default() {
        let config = parse_and_validate("").expect("should parse");
        let defaults = default_config();

        // Server
        assert_eq!(config.server.max_sessions, defaults.server.max_sessions);
        assert_eq!(config.server.max_processes, defaults.server.max_processes);
        assert_eq!(
            config.server.max_spool_bytes_total,
            defaults.server.max_spool_bytes_total
        );
        assert_eq!(
            config.server.idle_session_timeout_sec,
            defaults.server.idle_session_timeout_sec
        );

        // Squasher
        assert_eq!(config.squasher.max_lines, defaults.squasher.max_lines);
        assert_eq!(config.squasher.max_bytes, defaults.squasher.max_bytes);
        assert_eq!(config.squasher.oreo_head, defaults.squasher.oreo_head);
        assert_eq!(config.squasher.oreo_tail, defaults.squasher.oreo_tail);
        assert_eq!(config.squasher.spool_bytes, defaults.squasher.spool_bytes);

        // Yield
        assert_eq!(
            config.yield_config.silence_timeout_ms,
            defaults.yield_config.silence_timeout_ms
        );
        assert_eq!(
            config.yield_config.prompt_patterns,
            defaults.yield_config.prompt_patterns
        );

        // Timeout defaults
        assert_eq!(
            config.timeout_defaults.default,
            defaults.timeout_defaults.default
        );
        assert_eq!(
            config.timeout_defaults.scope,
            defaults.timeout_defaults.scope
        );

        // Audit (compare after tilde expansion)
        assert_eq!(config.audit.log_level, defaults.audit.log_level);
        assert_eq!(config.audit.log_commands, defaults.audit.log_commands);
        assert_eq!(
            config.audit.log_policy_decisions,
            defaults.audit.log_policy_decisions
        );
        assert_eq!(
            config.audit.log_handoff_events,
            defaults.audit.log_handoff_events
        );

        // Policy
        assert!(config.policy.auto_confirm.is_empty());
        assert!(config.policy.yield_to_operator.is_empty());
        assert!(config.policy.forbidden.is_empty());

        // Handoff
        assert_eq!(config.handoff.timeout_sec, defaults.handoff.timeout_sec);
        assert_eq!(config.handoff.fallback, defaults.handoff.fallback);

        // Sandbox
        assert!(config.sandbox.is_none());
    }

    /// Bonus: load_config from a real temp file
    #[test]
    fn load_config_from_file() {
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        write!(
            tmpfile,
            r#"
[server]
max_sessions = 8

[squasher]
max_lines = 500
"#
        )
        .unwrap();

        let config = load_config(tmpfile.path().to_str().unwrap()).expect("should load");
        assert_eq!(config.server.max_sessions, 8);
        assert_eq!(config.squasher.max_lines, 500);
        // Defaults for everything else
        assert_eq!(config.squasher.oreo_head, 50);
    }

    /// Bonus: validate_config on valid file
    #[test]
    fn validate_config_valid_file() {
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        write!(
            tmpfile,
            r#"
[server]
max_sessions = 5
"#
        )
        .unwrap();

        let result = validate_config(tmpfile.path().to_str().unwrap());
        assert!(result.is_ok());
    }

    /// Bonus: ConfigError Display impls
    #[test]
    fn config_error_display() {
        let io_err = ConfigError::IoError(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "not found",
        ));
        assert!(format!("{io_err}").contains("I/O error"));

        let parse_err = ConfigError::ParseError("bad toml".into());
        assert!(format!("{parse_err}").contains("parse error"));

        let val_err = ConfigError::ValidationError(vec!["a".into(), "b".into()]);
        let msg = format!("{val_err}");
        assert!(msg.contains("a"));
        assert!(msg.contains("b"));
    }

    /// Bonus: tilde expansion helper
    #[test]
    fn tilde_expansion_basic() {
        let home = std::env::var("HOME").expect("HOME");
        assert_eq!(expand_tilde("~/foo"), format!("{home}/foo"));
        assert_eq!(expand_tilde("/absolute/path"), "/absolute/path");
        assert_eq!(expand_tilde("relative/path"), "relative/path");
        assert_eq!(expand_tilde("~"), home);
    }

    /// raw_retention parses from TOML and defaults to "none"
    #[test]
    fn raw_retention_parses_and_defaults() {
        // Explicit value
        let toml = r#"
[audit]
raw_retention = "7d"
"#;
        let config = parse_and_validate(toml).expect("should parse");
        assert_eq!(config.audit.raw_retention, "7d");

        // Default when not specified
        let config = parse_and_validate("").expect("should parse");
        assert_eq!(config.audit.raw_retention, "none");
    }
}
