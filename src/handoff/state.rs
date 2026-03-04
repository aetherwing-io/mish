//! Handoff state machine for operator handoff lifecycle.
//!
//! Manages active handoff sessions: crypto-random IDs, single-use attachment,
//! timeout/fallback, and credential-blind return summaries.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rand::Rng;
use serde::Serialize;

/// A single active handoff session.
#[derive(Debug, Clone)]
pub struct HandoffEntry {
    /// Crypto-random 128-bit ID for operator attachment (format: `hf_<hex>`).
    /// This is the SECRET — communicated out-of-band, never returned to LLM.
    pub handoff_id: String,
    /// Separate reference ID for LLM status checking (format: `ref_<hex>`).
    pub reference_id: String,
    /// Process alias this handoff is for.
    pub alias: String,
    /// Human-readable reason (e.g. "MFA required").
    pub reason: String,
    /// When the handoff was initiated.
    pub initiated_at: Instant,
    /// Whether an operator has attached.
    pub attached: bool,
    /// PID of attached operator process, if any.
    pub operator_pid: Option<u32>,
}

/// Summary returned when a handoff resolves (operator detaches or timeout).
#[derive(Debug, Clone, Serialize)]
pub struct HandoffSummary {
    pub duration_ms: u64,
    pub lines_during_handoff: usize,
    pub outcome: String,
}

/// What to do when a handoff times out.
#[derive(Debug, Clone, PartialEq)]
pub enum TimeoutAction {
    YieldToLlm { alias: String, handoff_id: String },
    Kill { alias: String, handoff_id: String },
}

/// Handoff error conditions.
#[derive(Debug, Clone, PartialEq)]
pub enum HandoffError {
    /// Operator already attached to this handoff.
    AlreadyAttached,
    /// Handoff ID not found.
    NotFound,
    /// Process alias already has an active handoff.
    AlreadyHandedOff { alias: String },
}

impl std::fmt::Display for HandoffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandoffError::AlreadyAttached => write!(f, "already attached"),
            HandoffError::NotFound => write!(f, "handoff not found"),
            HandoffError::AlreadyHandedOff { alias } => {
                write!(f, "alias {alias:?} already has an active handoff")
            }
        }
    }
}

impl std::error::Error for HandoffError {}

/// Manages all active handoff sessions.
#[derive(Default)]
pub struct HandoffManager {
    /// Active handoffs keyed by handoff_id.
    entries: HashMap<String, HandoffEntry>,
    /// reference_id → handoff_id lookup.
    reference_map: HashMap<String, String>,
    /// alias → handoff_id lookup (one active handoff per alias).
    alias_map: HashMap<String, String>,
}

/// Generate a crypto-random 128-bit hex string.
fn random_hex_128() -> String {
    let mut rng = rand::thread_rng();
    let mut bytes = [0u8; 16];
    rng.fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl HandoffManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new handoff for the given process alias.
    /// Returns `(handoff_id, reference_id)`.
    pub fn create(&mut self, alias: &str, reason: &str) -> Result<(String, String), HandoffError> {
        if self.alias_map.contains_key(alias) {
            return Err(HandoffError::AlreadyHandedOff {
                alias: alias.into(),
            });
        }

        let handoff_id = format!("hf_{}", random_hex_128());
        let reference_id = format!("ref_{}", random_hex_128());

        let entry = HandoffEntry {
            handoff_id: handoff_id.clone(),
            reference_id: reference_id.clone(),
            alias: alias.into(),
            reason: reason.into(),
            initiated_at: Instant::now(),
            attached: false,
            operator_pid: None,
        };

        self.alias_map.insert(alias.into(), handoff_id.clone());
        self.reference_map
            .insert(reference_id.clone(), handoff_id.clone());
        self.entries.insert(handoff_id.clone(), entry);

        Ok((handoff_id, reference_id))
    }

    /// Operator attaches to a handoff by its secret handoff_id.
    /// Single-use: second attachment returns `AlreadyAttached`.
    pub fn attach(
        &mut self,
        handoff_id: &str,
        operator_pid: u32,
    ) -> Result<&HandoffEntry, HandoffError> {
        let entry = self
            .entries
            .get_mut(handoff_id)
            .ok_or(HandoffError::NotFound)?;

        if entry.attached {
            return Err(HandoffError::AlreadyAttached);
        }

        entry.attached = true;
        entry.operator_pid = Some(operator_pid);

        Ok(entry)
    }

    /// Operator detaches. Returns a credential-blind summary.
    /// `lines_during_handoff` is provided by caller (process table tracks this).
    pub fn detach(
        &mut self,
        handoff_id: &str,
        lines_during_handoff: usize,
    ) -> Result<HandoffSummary, HandoffError> {
        let entry = self
            .entries
            .get(handoff_id)
            .ok_or(HandoffError::NotFound)?;

        let duration_ms = entry.initiated_at.elapsed().as_millis() as u64;

        let summary = HandoffSummary {
            duration_ms,
            lines_during_handoff,
            outcome: "resolved".into(),
        };

        // Clean up all maps
        let entry = self.entries.remove(handoff_id).unwrap();
        self.reference_map.remove(&entry.reference_id);
        self.alias_map.remove(&entry.alias);

        Ok(summary)
    }

    /// Look up a handoff by the LLM-visible reference ID.
    pub fn get_by_reference(&self, reference_id: &str) -> Option<&HandoffEntry> {
        let handoff_id = self.reference_map.get(reference_id)?;
        self.entries.get(handoff_id)
    }

    /// Look up a handoff by process alias.
    pub fn get_by_alias(&self, alias: &str) -> Option<&HandoffEntry> {
        let handoff_id = self.alias_map.get(alias)?;
        self.entries.get(handoff_id)
    }

    /// Look up a handoff by its secret handoff_id.
    pub fn get(&self, handoff_id: &str) -> Option<&HandoffEntry> {
        self.entries.get(handoff_id)
    }

    /// Find handoffs that have exceeded the timeout.
    /// Returns actions to take based on the fallback config.
    pub fn check_timeouts(&self, timeout_sec: u64, fallback: &str) -> Vec<TimeoutAction> {
        let timeout = Duration::from_secs(timeout_sec);
        self.entries
            .values()
            .filter(|entry| entry.initiated_at.elapsed() > timeout)
            .map(|entry| match fallback {
                "kill" => TimeoutAction::Kill {
                    alias: entry.alias.clone(),
                    handoff_id: entry.handoff_id.clone(),
                },
                _ => TimeoutAction::YieldToLlm {
                    alias: entry.alias.clone(),
                    handoff_id: entry.handoff_id.clone(),
                },
            })
            .collect()
    }

    /// Handle process exit during an active handoff.
    /// Removes the handoff and returns a summary with outcome "process_exited".
    pub fn process_exited(
        &mut self,
        alias: &str,
        lines_during_handoff: usize,
    ) -> Option<HandoffSummary> {
        let handoff_id = self.alias_map.remove(alias)?;
        let entry = self.entries.remove(&handoff_id)?;
        self.reference_map.remove(&entry.reference_id);

        Some(HandoffSummary {
            duration_ms: entry.initiated_at.elapsed().as_millis() as u64,
            lines_during_handoff,
            outcome: "process_exited".into(),
        })
    }

    /// Remove a handoff (after timeout action is taken).
    pub fn remove(&mut self, handoff_id: &str) -> Option<HandoffEntry> {
        let entry = self.entries.remove(handoff_id)?;
        self.reference_map.remove(&entry.reference_id);
        self.alias_map.remove(&entry.alias);
        Some(entry)
    }

    /// Number of active handoffs.
    pub fn active_count(&self) -> usize {
        self.entries.len()
    }

    /// List all active handoffs (for `mish handoffs` command).
    pub fn list_active(&self) -> Vec<&HandoffEntry> {
        self.entries.values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ── Handoff ID generation ──────────────────────────────────────

    #[test]
    fn create_handoff_returns_crypto_random_id() {
        let mut mgr = HandoffManager::new();
        let (hid, _rid) = mgr.create("deploy", "MFA required").unwrap();

        // Format: hf_<32 hex chars> (128 bits = 16 bytes = 32 hex)
        assert!(hid.starts_with("hf_"), "handoff_id should start with hf_");
        let hex_part = &hid[3..];
        assert_eq!(hex_part.len(), 32, "should be 32 hex chars (128 bits)");
        assert!(
            hex_part.chars().all(|c| c.is_ascii_hexdigit()),
            "should be valid hex"
        );
    }

    #[test]
    fn create_handoff_returns_separate_reference_id() {
        let mut mgr = HandoffManager::new();
        let (hid, rid) = mgr.create("deploy", "MFA required").unwrap();

        // Reference ID has different prefix
        assert!(rid.starts_with("ref_"), "reference_id should start with ref_");
        assert_ne!(hid, rid, "handoff_id and reference_id must differ");
    }

    #[test]
    fn create_two_handoffs_produces_unique_ids() {
        let mut mgr = HandoffManager::new();
        let (hid1, rid1) = mgr.create("deploy", "MFA").unwrap();
        // Remove first so we can reuse alias — or use different aliases
        mgr.remove(&hid1);
        let (hid2, rid2) = mgr.create("deploy", "MFA again").unwrap();

        assert_ne!(hid1, hid2, "handoff IDs must be unique");
        assert_ne!(rid1, rid2, "reference IDs must be unique");
    }

    // ── Single-use attachment ──────────────────────────────────────

    #[test]
    fn attach_marks_entry_as_attached() {
        let mut mgr = HandoffManager::new();
        let (hid, _) = mgr.create("deploy", "MFA").unwrap();

        let entry = mgr.attach(&hid, 12345).unwrap();
        assert!(entry.attached);
        assert_eq!(entry.operator_pid, Some(12345));
    }

    #[test]
    fn second_attach_returns_already_attached() {
        let mut mgr = HandoffManager::new();
        let (hid, _) = mgr.create("deploy", "MFA").unwrap();

        mgr.attach(&hid, 12345).unwrap();
        let err = mgr.attach(&hid, 99999).unwrap_err();
        assert_eq!(err, HandoffError::AlreadyAttached);
    }

    #[test]
    fn attach_invalid_id_returns_not_found() {
        let mut mgr = HandoffManager::new();
        let err = mgr.attach("hf_nonexistent", 12345).unwrap_err();
        assert_eq!(err, HandoffError::NotFound);
    }

    // ── Duplicate alias prevention ─────────────────────────────────

    #[test]
    fn create_duplicate_alias_returns_error() {
        let mut mgr = HandoffManager::new();
        mgr.create("deploy", "MFA").unwrap();

        let err = mgr.create("deploy", "another reason").unwrap_err();
        assert_eq!(
            err,
            HandoffError::AlreadyHandedOff {
                alias: "deploy".into()
            }
        );
    }

    // ── Detach / summary ───────────────────────────────────────────

    #[test]
    fn detach_returns_credential_blind_summary() {
        let mut mgr = HandoffManager::new();
        let (hid, _) = mgr.create("deploy", "MFA").unwrap();
        mgr.attach(&hid, 12345).unwrap();

        let summary = mgr.detach(&hid, 42).unwrap();
        assert_eq!(summary.outcome, "resolved");
        assert_eq!(summary.lines_during_handoff, 42);
        assert!(summary.duration_ms < 5000, "duration should be short in test");
    }

    #[test]
    fn detach_removes_handoff_from_active() {
        let mut mgr = HandoffManager::new();
        let (hid, _) = mgr.create("deploy", "MFA").unwrap();
        mgr.attach(&hid, 12345).unwrap();
        mgr.detach(&hid, 0).unwrap();

        assert_eq!(mgr.active_count(), 0);
        assert!(mgr.get(&hid).is_none());
    }

    #[test]
    fn detach_invalid_id_returns_not_found() {
        let mut mgr = HandoffManager::new();
        let err = mgr.detach("hf_nonexistent", 0).unwrap_err();
        assert_eq!(err, HandoffError::NotFound);
    }

    #[test]
    fn detach_frees_alias_for_reuse() {
        let mut mgr = HandoffManager::new();
        let (hid, _) = mgr.create("deploy", "MFA").unwrap();
        mgr.attach(&hid, 12345).unwrap();
        mgr.detach(&hid, 0).unwrap();

        // Should be able to create a new handoff for the same alias
        let result = mgr.create("deploy", "new MFA");
        assert!(result.is_ok());
    }

    // ── Lookup methods ─────────────────────────────────────────────

    #[test]
    fn get_by_reference_returns_entry() {
        let mut mgr = HandoffManager::new();
        let (hid, rid) = mgr.create("deploy", "MFA").unwrap();

        let entry = mgr.get_by_reference(&rid).unwrap();
        assert_eq!(entry.handoff_id, hid);
        assert_eq!(entry.alias, "deploy");
    }

    #[test]
    fn get_by_reference_unknown_returns_none() {
        let mgr = HandoffManager::new();
        assert!(mgr.get_by_reference("ref_unknown").is_none());
    }

    #[test]
    fn get_by_alias_returns_entry() {
        let mut mgr = HandoffManager::new();
        let (hid, _) = mgr.create("deploy", "MFA").unwrap();

        let entry = mgr.get_by_alias("deploy").unwrap();
        assert_eq!(entry.handoff_id, hid);
    }

    #[test]
    fn get_by_alias_unknown_returns_none() {
        let mgr = HandoffManager::new();
        assert!(mgr.get_by_alias("nope").is_none());
    }

    // ── Timeout detection ──────────────────────────────────────────

    #[test]
    fn check_timeouts_no_expired_returns_empty() {
        let mut mgr = HandoffManager::new();
        mgr.create("deploy", "MFA").unwrap();

        // Just created — well within any timeout
        let actions = mgr.check_timeouts(600, "yield_to_llm");
        assert!(actions.is_empty());
    }

    #[test]
    fn check_timeouts_expired_yields_to_llm() {
        let mut mgr = HandoffManager::new();
        let (hid, _) = mgr.create("deploy", "MFA").unwrap();

        // Backdate the initiated_at to simulate timeout
        mgr.entries.get_mut(&hid).unwrap().initiated_at =
            Instant::now() - Duration::from_secs(700);

        let actions = mgr.check_timeouts(600, "yield_to_llm");
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0],
            TimeoutAction::YieldToLlm {
                alias: "deploy".into(),
                handoff_id: hid.clone(),
            }
        );
    }

    #[test]
    fn check_timeouts_expired_kills() {
        let mut mgr = HandoffManager::new();
        let (hid, _) = mgr.create("deploy", "MFA").unwrap();

        mgr.entries.get_mut(&hid).unwrap().initiated_at =
            Instant::now() - Duration::from_secs(700);

        let actions = mgr.check_timeouts(600, "kill");
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0],
            TimeoutAction::Kill {
                alias: "deploy".into(),
                handoff_id: hid.clone(),
            }
        );
    }

    // ── Process exit during handoff ────────────────────────────────

    #[test]
    fn process_exited_returns_summary() {
        let mut mgr = HandoffManager::new();
        mgr.create("deploy", "MFA").unwrap();

        let summary = mgr.process_exited("deploy", 15).unwrap();
        assert_eq!(summary.outcome, "process_exited");
        assert_eq!(summary.lines_during_handoff, 15);
    }

    #[test]
    fn process_exited_removes_handoff() {
        let mut mgr = HandoffManager::new();
        mgr.create("deploy", "MFA").unwrap();
        mgr.process_exited("deploy", 0);

        assert_eq!(mgr.active_count(), 0);
    }

    #[test]
    fn process_exited_unknown_alias_returns_none() {
        let mut mgr = HandoffManager::new();
        assert!(mgr.process_exited("nope", 0).is_none());
    }

    // ── Remove ─────────────────────────────────────────────────────

    #[test]
    fn remove_returns_entry_and_cleans_maps() {
        let mut mgr = HandoffManager::new();
        let (hid, rid) = mgr.create("deploy", "MFA").unwrap();

        let entry = mgr.remove(&hid).unwrap();
        assert_eq!(entry.alias, "deploy");

        // All maps cleaned
        assert!(mgr.get(&hid).is_none());
        assert!(mgr.get_by_reference(&rid).is_none());
        assert!(mgr.get_by_alias("deploy").is_none());
        assert_eq!(mgr.active_count(), 0);
    }

    #[test]
    fn remove_unknown_returns_none() {
        let mut mgr = HandoffManager::new();
        assert!(mgr.remove("hf_nope").is_none());
    }

    // ── List active ────────────────────────────────────────────────

    #[test]
    fn list_active_returns_all_entries() {
        let mut mgr = HandoffManager::new();
        mgr.create("deploy", "MFA").unwrap();
        mgr.create("build", "auth prompt").unwrap();

        let active = mgr.list_active();
        assert_eq!(active.len(), 2);

        let aliases: Vec<&str> = active.iter().map(|e| e.alias.as_str()).collect();
        assert!(aliases.contains(&"deploy"));
        assert!(aliases.contains(&"build"));
    }

    // ── Entry fields ───────────────────────────────────────────────

    #[test]
    fn created_entry_has_correct_fields() {
        let mut mgr = HandoffManager::new();
        let (hid, _) = mgr.create("deploy", "MFA required").unwrap();

        let entry = mgr.get(&hid).unwrap();
        assert_eq!(entry.alias, "deploy");
        assert_eq!(entry.reason, "MFA required");
        assert!(!entry.attached);
        assert!(entry.operator_pid.is_none());
    }

    // ── active_count ───────────────────────────────────────────────

    #[test]
    fn active_count_tracks_correctly() {
        let mut mgr = HandoffManager::new();
        assert_eq!(mgr.active_count(), 0);

        let (hid1, _) = mgr.create("deploy", "MFA").unwrap();
        assert_eq!(mgr.active_count(), 1);

        mgr.create("build", "auth").unwrap();
        assert_eq!(mgr.active_count(), 2);

        mgr.remove(&hid1);
        assert_eq!(mgr.active_count(), 1);
    }
}
