//! Process table with digest generation for MCP server mode.
//!
//! Tracks all spawned processes, their lifecycle state, watch matches, and
//! yield/prompt state. Generates a digest (full, changed, or none) attached
//! to every MCP tool response so the LLM has ambient awareness of process state.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::MishConfig;
use crate::interpreter::ManagedProcess;
use crate::mcp::types::{
    ProcessDigestEntry, ERR_ALIAS_NOT_FOUND, ERR_ALIAS_IN_USE, ERR_PROCESS_LIMIT,
    ERR_INVALID_ACTION,
};
use crate::process::spool::{OutputSpool, SpoolManager};
use crate::process::state::ProcessState;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from process table operations.
#[derive(Debug, PartialEq, Eq)]
pub enum ProcessTableError {
    AliasNotFound,
    AliasInUse,
    ProcessLimitReached,
    InvalidStateTransition,
}

impl ProcessTableError {
    /// Map to JSON-RPC error code per spec.
    pub fn error_code(&self) -> i32 {
        match self {
            ProcessTableError::AliasNotFound => ERR_ALIAS_NOT_FOUND,
            ProcessTableError::AliasInUse => ERR_ALIAS_IN_USE,
            ProcessTableError::ProcessLimitReached => ERR_PROCESS_LIMIT,
            ProcessTableError::InvalidStateTransition => ERR_INVALID_ACTION,
        }
    }
}

impl std::fmt::Display for ProcessTableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcessTableError::AliasNotFound => write!(f, "process alias not found"),
            ProcessTableError::AliasInUse => write!(f, "process alias already in use"),
            ProcessTableError::ProcessLimitReached => write!(f, "process limit reached"),
            ProcessTableError::InvalidStateTransition => write!(f, "invalid state transition"),
        }
    }
}

impl std::error::Error for ProcessTableError {}

// ---------------------------------------------------------------------------
// ProcessEntry
// ---------------------------------------------------------------------------

/// A tracked process entry.
pub struct ProcessEntry {
    pub alias: String,
    pub session: String,
    pub state: ProcessState,
    pub pid: u32,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    pub started_at: Instant,
    pub completed_at: Option<Instant>,
    pub spool: Arc<OutputSpool>,

    // Watch-related:
    pub watch_pattern: Option<String>,
    pub last_match: Option<String>,
    pub match_count: u32,

    // Yield-related:
    pub prompt_tail: Option<String>,

    // Completion-related:
    pub output_summary: Option<String>,
    pub error_tail: Option<String>,

    // Managed process (REPL interpreter or dedicated PTY):
    pub interpreter: Option<Arc<ManagedProcess>>,

    // Tracking:
    pub last_modified_seq: u64,
    pub seen_by_client: bool,
}

// ---------------------------------------------------------------------------
// DigestMode
// ---------------------------------------------------------------------------

/// Digest mode controls what's included.
pub enum DigestMode {
    /// Return all tracked processes.
    Full,
    /// Return only entries modified since last response (default).
    Changed,
    /// Omit process table entirely.
    None,
}

// ---------------------------------------------------------------------------
// ProcessTable
// ---------------------------------------------------------------------------

/// Max terminal entries retained when digest exceeds 20 entries.
const DIGEST_CAP: usize = 20;
/// How many terminal entries to keep when capping.
const TERMINAL_RETAIN: usize = 10;

/// Global process table.
pub struct ProcessTable {
    entries: HashMap<String, ProcessEntry>,
    sequence_counter: u64,
    last_client_seq: u64,
    spool_manager: SpoolManager,
    max_processes: usize,
    spool_capacity: usize,
}

impl ProcessTable {
    /// Create a new process table from config.
    pub fn new(config: &MishConfig) -> Self {
        Self {
            entries: HashMap::new(),
            sequence_counter: 0,
            last_client_seq: 0,
            spool_manager: SpoolManager::new(config.server.max_spool_bytes_total),
            max_processes: config.server.max_processes,
            spool_capacity: config.squasher.spool_bytes,
        }
    }

    /// Advance the sequence counter and return the new value.
    fn next_seq(&mut self) -> u64 {
        self.sequence_counter += 1;
        self.sequence_counter
    }

    /// Register a new process.
    pub fn register(
        &mut self,
        alias: &str,
        session: &str,
        pid: u32,
        watch: Option<&str>,
    ) -> Result<(), ProcessTableError> {
        if self.entries.contains_key(alias) {
            return Err(ProcessTableError::AliasInUse);
        }
        if self.entries.len() >= self.max_processes {
            return Err(ProcessTableError::ProcessLimitReached);
        }

        // Create spool — if spool creation fails due to aggregate limit,
        // map it to ProcessLimitReached (closest semantic match).
        let spool = self
            .spool_manager
            .create_spool(alias, self.spool_capacity)
            .map_err(|_| ProcessTableError::ProcessLimitReached)?;

        let seq = self.next_seq();
        let entry = ProcessEntry {
            alias: alias.to_string(),
            session: session.to_string(),
            state: ProcessState::Running,
            pid,
            exit_code: None,
            signal: None,
            started_at: Instant::now(),
            completed_at: None,
            spool,
            watch_pattern: watch.map(|w| w.to_string()),
            last_match: None,
            match_count: 0,
            prompt_tail: None,
            output_summary: None,
            error_tail: None,
            interpreter: None,
            last_modified_seq: seq,
            seen_by_client: false,
        };

        self.entries.insert(alias.to_string(), entry);
        Ok(())
    }

    /// Attach a managed process (interpreter or dedicated PTY) to a process entry.
    pub fn set_interpreter(&mut self, alias: &str, interp: Arc<ManagedProcess>) {
        if let Some(entry) = self.entries.get_mut(alias) {
            entry.interpreter = Some(interp);
        }
    }

    /// Update the state of a process. Validates state transition.
    pub fn update_state(
        &mut self,
        alias: &str,
        new_state: ProcessState,
    ) -> Result<(), ProcessTableError> {
        let seq = self.next_seq();
        let entry = self
            .entries
            .get_mut(alias)
            .ok_or(ProcessTableError::AliasNotFound)?;

        if !entry.state.can_transition_to(new_state) {
            return Err(ProcessTableError::InvalidStateTransition);
        }

        entry.state = new_state;
        if new_state.is_terminal() {
            entry.completed_at = Some(Instant::now());
        }
        entry.last_modified_seq = seq;
        entry.seen_by_client = false;
        Ok(())
    }

    /// Set exit code for a process.
    pub fn set_exit_code(&mut self, alias: &str, code: i32) {
        let seq = self.next_seq();
        if let Some(entry) = self.entries.get_mut(alias) {
            entry.exit_code = Some(code);
            entry.last_modified_seq = seq;
            entry.seen_by_client = false;
        }
    }

    /// Set termination signal for a process.
    pub fn set_signal(&mut self, alias: &str, signal: &str) {
        let seq = self.next_seq();
        if let Some(entry) = self.entries.get_mut(alias) {
            entry.signal = Some(signal.to_string());
            entry.last_modified_seq = seq;
            entry.seen_by_client = false;
        }
    }

    /// Record a watch pattern match.
    pub fn record_watch_match(&mut self, alias: &str, line: &str) {
        let seq = self.next_seq();
        if let Some(entry) = self.entries.get_mut(alias) {
            entry.last_match = Some(line.to_string());
            entry.match_count += 1;
            entry.last_modified_seq = seq;
            entry.seen_by_client = false;
        }
    }

    /// Set the prompt tail (yield detection).
    pub fn set_prompt_tail(&mut self, alias: &str, prompt: &str) {
        let seq = self.next_seq();
        if let Some(entry) = self.entries.get_mut(alias) {
            entry.prompt_tail = Some(prompt.to_string());
            entry.last_modified_seq = seq;
            entry.seen_by_client = false;
        }
    }

    /// Set output summary for a completed process.
    pub fn set_output_summary(&mut self, alias: &str, summary: &str) {
        let seq = self.next_seq();
        if let Some(entry) = self.entries.get_mut(alias) {
            entry.output_summary = Some(summary.to_string());
            entry.last_modified_seq = seq;
            entry.seen_by_client = false;
        }
    }

    /// Set error tail for a failed process.
    pub fn set_error_tail(&mut self, alias: &str, tail: &str) {
        let seq = self.next_seq();
        if let Some(entry) = self.entries.get_mut(alias) {
            entry.error_tail = Some(tail.to_string());
            entry.last_modified_seq = seq;
            entry.seen_by_client = false;
        }
    }

    /// Get a process entry by alias.
    pub fn get(&self, alias: &str) -> Option<&ProcessEntry> {
        self.entries.get(alias)
    }

    /// Check if an alias exists.
    pub fn alias_exists(&self, alias: &str) -> bool {
        self.entries.contains_key(alias)
    }

    /// Count active (non-terminal) processes.
    pub fn active_count(&self) -> usize {
        self.entries
            .values()
            .filter(|e| !e.state.is_terminal())
            .count()
    }

    /// Generate a process digest according to the requested mode.
    pub fn digest(&mut self, mode: DigestMode) -> Vec<ProcessDigestEntry> {
        match mode {
            DigestMode::None => Vec::new(),
            DigestMode::Full => self.build_digest(false),
            DigestMode::Changed => self.build_digest(true),
        }
    }

    /// Build the digest, optionally filtering to only changed entries.
    fn build_digest(&mut self, changed_only: bool) -> Vec<ProcessDigestEntry> {
        let first_call = self.last_client_seq == 0;
        let client_seq = self.last_client_seq;

        // Collect entries: if changed_only and not first call, filter.
        let mut digest_entries: Vec<ProcessDigestEntry> = self
            .entries
            .values()
            .filter(|e| !changed_only || first_call || e.last_modified_seq > client_seq)
            .map(|e| self.entry_to_digest(e))
            .collect();

        // Sort: awaiting_input first, then running, then terminal.
        // Within each category, sort by elapsed_ms descending (longest first).
        digest_entries.sort_by(|a, b| {
            let cat_a = state_sort_key(&a.state);
            let cat_b = state_sort_key(&b.state);
            cat_a
                .cmp(&cat_b)
                .then_with(|| b.elapsed_ms.cmp(&a.elapsed_ms))
        });

        // Cap: if >20, keep all running/awaiting_input + 10 most recent terminal.
        if digest_entries.len() > DIGEST_CAP {
            let (mut non_terminal, mut terminal): (Vec<_>, Vec<_>) = digest_entries
                .into_iter()
                .partition(|e| !is_terminal_state(&e.state));

            // Terminal entries: keep 10 most recent (lowest elapsed_ms = most recent).
            terminal.sort_by(|a, b| a.elapsed_ms.cmp(&b.elapsed_ms));
            terminal.truncate(TERMINAL_RETAIN);

            // Re-sort terminal by standard order.
            terminal.sort_by(|a, b| b.elapsed_ms.cmp(&a.elapsed_ms));

            non_terminal.append(&mut terminal);

            // Final sort.
            non_terminal.sort_by(|a, b| {
                let cat_a = state_sort_key(&a.state);
                let cat_b = state_sort_key(&b.state);
                cat_a
                    .cmp(&cat_b)
                    .then_with(|| b.elapsed_ms.cmp(&a.elapsed_ms))
            });

            digest_entries = non_terminal;
        }

        // Update client sequence and mark entries as seen.
        let current_seq = self.sequence_counter;
        self.last_client_seq = current_seq;

        // Mark all returned entries as seen.
        for de in &digest_entries {
            if let Some(entry) = self.entries.get_mut(&de.alias) {
                entry.seen_by_client = true;
            }
        }

        digest_entries
    }

    /// Convert a ProcessEntry to a ProcessDigestEntry.
    fn entry_to_digest(&self, entry: &ProcessEntry) -> ProcessDigestEntry {
        let elapsed_ms = entry.started_at.elapsed().as_millis() as u64;
        let duration_ms = entry.completed_at.map(|c| {
            c.duration_since(entry.started_at).as_millis() as u64
        });

        // Compact stub: if terminal, already seen by client, reduce to stub.
        if entry.state.is_terminal() && entry.seen_by_client {
            return ProcessDigestEntry {
                alias: entry.alias.clone(),
                session: entry.session.clone(),
                state: entry.state.as_str().to_string(),
                pid: entry.pid,
                exit_code: entry.exit_code,
                signal: None,
                elapsed_ms,
                duration_ms,
                prompt_tail: None,
                last_match: None,
                match_count: None,
                handoff_id: None,
                output_summary: None,
                error_tail: None,
                notify_operator: None,
            };
        }

        ProcessDigestEntry {
            alias: entry.alias.clone(),
            session: entry.session.clone(),
            state: entry.state.as_str().to_string(),
            pid: entry.pid,
            exit_code: entry.exit_code,
            signal: entry.signal.clone(),
            elapsed_ms,
            duration_ms,
            prompt_tail: entry.prompt_tail.clone(),
            last_match: entry.last_match.clone(),
            match_count: if entry.match_count > 0 {
                Some(entry.match_count)
            } else {
                None
            },
            handoff_id: None,
            output_summary: entry.output_summary.clone(),
            error_tail: entry.error_tail.clone(),
            notify_operator: if entry.state == ProcessState::AwaitingInput {
                Some(true)
            } else {
                None
            },
        }
    }

    /// Remove entries that completed more than `retention` ago.
    pub fn cleanup_expired(&mut self, retention: Duration) {
        let now = Instant::now();
        let expired: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| {
                if let Some(completed) = e.completed_at {
                    e.state.is_terminal() && now.duration_since(completed) > retention
                } else {
                    false
                }
            })
            .map(|(alias, _)| alias.clone())
            .collect();

        for alias in &expired {
            self.entries.remove(alias);
            self.spool_manager.remove_spool(alias);
        }
    }

    /// Explicitly dismiss (remove) a process entry.
    pub fn dismiss(&mut self, alias: &str) -> Result<(), ProcessTableError> {
        if !self.entries.contains_key(alias) {
            return Err(ProcessTableError::AliasNotFound);
        }
        self.entries.remove(alias);
        self.spool_manager.remove_spool(alias);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Sort key for digest ordering: 0 = awaiting_input, 1 = running/handed_off, 2 = terminal.
fn state_sort_key(state: &str) -> u8 {
    match state {
        "awaiting_input" => 0,
        "running" | "handed_off" => 1,
        _ => 2, // completed, failed, killed, timed_out
    }
}

/// Check if a state string represents a terminal state.
fn is_terminal_state(state: &str) -> bool {
    matches!(state, "completed" | "failed" | "killed" | "timed_out")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MishConfig;
    use std::thread;
    use std::time::Duration;

    fn test_config() -> MishConfig {
        let mut config = MishConfig::default();
        config.server.max_processes = 20;
        config.server.max_spool_bytes_total = 52_428_800;
        config.squasher.spool_bytes = 1024;
        config
    }

    fn small_config(max_processes: usize) -> MishConfig {
        let mut config = MishConfig::default();
        config.server.max_processes = max_processes;
        config.server.max_spool_bytes_total = 52_428_800;
        config.squasher.spool_bytes = 1024;
        config
    }

    // ── Register ──────────────────────────────────────────────────────

    #[test]
    fn register_process_with_unique_alias() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);

        let result = table.register("build", "default", 1234, None);
        assert!(result.is_ok());
        assert!(table.alias_exists("build"));
        assert_eq!(table.active_count(), 1);
    }

    #[test]
    fn register_duplicate_alias_returns_alias_in_use() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);

        table.register("build", "default", 1234, None).unwrap();
        let result = table.register("build", "default", 5678, None);
        assert_eq!(result, Err(ProcessTableError::AliasInUse));
    }

    #[test]
    fn register_at_process_limit_returns_error() {
        let config = small_config(2);
        let mut table = ProcessTable::new(&config);

        table.register("proc1", "default", 100, None).unwrap();
        table.register("proc2", "default", 101, None).unwrap();
        let result = table.register("proc3", "default", 102, None);
        assert_eq!(result, Err(ProcessTableError::ProcessLimitReached));
    }

    // ── State transitions ────────────────────────────────────────────

    #[test]
    fn running_to_completed() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);
        table.register("build", "default", 1234, None).unwrap();

        let result = table.update_state("build", ProcessState::Completed);
        assert!(result.is_ok());

        let entry = table.get("build").unwrap();
        assert_eq!(entry.state, ProcessState::Completed);
        assert!(entry.completed_at.is_some());
    }

    #[test]
    fn running_to_failed() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);
        table.register("build", "default", 1234, None).unwrap();

        let result = table.update_state("build", ProcessState::Failed);
        assert!(result.is_ok());

        let entry = table.get("build").unwrap();
        assert_eq!(entry.state, ProcessState::Failed);
    }

    #[test]
    fn running_to_killed() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);
        table.register("build", "default", 1234, None).unwrap();

        let result = table.update_state("build", ProcessState::Killed);
        assert!(result.is_ok());

        let entry = table.get("build").unwrap();
        assert_eq!(entry.state, ProcessState::Killed);
    }

    #[test]
    fn running_to_timed_out() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);
        table.register("build", "default", 1234, None).unwrap();

        let result = table.update_state("build", ProcessState::TimedOut);
        assert!(result.is_ok());

        let entry = table.get("build").unwrap();
        assert_eq!(entry.state, ProcessState::TimedOut);
    }

    #[test]
    fn invalid_state_transition_completed_to_running() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);
        table.register("build", "default", 1234, None).unwrap();
        table
            .update_state("build", ProcessState::Completed)
            .unwrap();

        let result = table.update_state("build", ProcessState::Running);
        assert_eq!(result, Err(ProcessTableError::InvalidStateTransition));
    }

    #[test]
    fn update_state_alias_not_found() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);

        let result = table.update_state("nonexistent", ProcessState::Running);
        assert_eq!(result, Err(ProcessTableError::AliasNotFound));
    }

    // ── Digest Full ──────────────────────────────────────────────────

    #[test]
    fn digest_full_returns_all_entries() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);

        table.register("proc1", "default", 100, None).unwrap();
        table.register("proc2", "default", 101, None).unwrap();
        table
            .update_state("proc2", ProcessState::Completed)
            .unwrap();

        let digest = table.digest(DigestMode::Full);
        assert_eq!(digest.len(), 2);
    }

    // ── Digest Changed ───────────────────────────────────────────────

    #[test]
    fn digest_changed_first_call_returns_full_table() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);

        table.register("proc1", "default", 100, None).unwrap();
        table.register("proc2", "default", 101, None).unwrap();

        // First call should return everything (last_client_seq == 0).
        let digest = table.digest(DigestMode::Changed);
        assert_eq!(digest.len(), 2);
    }

    #[test]
    fn digest_changed_returns_only_modified_entries() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);

        table.register("proc1", "default", 100, None).unwrap();
        table.register("proc2", "default", 101, None).unwrap();

        // First digest sees everything.
        let _ = table.digest(DigestMode::Changed);

        // Only modify proc1.
        table
            .update_state("proc1", ProcessState::Completed)
            .unwrap();

        let digest = table.digest(DigestMode::Changed);
        assert_eq!(digest.len(), 1);
        assert_eq!(digest[0].alias, "proc1");
    }

    // ── Digest None ──────────────────────────────────────────────────

    #[test]
    fn digest_none_returns_empty() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);

        table.register("proc1", "default", 100, None).unwrap();

        let digest = table.digest(DigestMode::None);
        assert!(digest.is_empty());
    }

    // ── Digest ordering ──────────────────────────────────────────────

    #[test]
    fn digest_ordering_awaiting_input_first_then_running_then_terminal() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);

        table.register("completed_proc", "default", 100, None).unwrap();
        table
            .update_state("completed_proc", ProcessState::Completed)
            .unwrap();

        table.register("running_proc", "default", 101, None).unwrap();

        table.register("awaiting_proc", "default", 102, None).unwrap();
        table
            .update_state("awaiting_proc", ProcessState::AwaitingInput)
            .unwrap();

        let digest = table.digest(DigestMode::Full);
        assert_eq!(digest.len(), 3);
        assert_eq!(digest[0].state, "awaiting_input");
        assert_eq!(digest[1].state, "running");
        assert_eq!(digest[2].state, "completed");
    }

    #[test]
    fn digest_ordering_within_category_elapsed_ms_descending() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);

        // Register proc_old first (will have higher elapsed_ms).
        table.register("proc_old", "default", 100, None).unwrap();

        // Small sleep to ensure measurable elapsed difference.
        thread::sleep(Duration::from_millis(10));

        table.register("proc_new", "default", 101, None).unwrap();

        let digest = table.digest(DigestMode::Full);
        assert_eq!(digest.len(), 2);
        // Both running, proc_old has higher elapsed_ms, should come first.
        assert_eq!(digest[0].alias, "proc_old");
        assert_eq!(digest[1].alias, "proc_new");
    }

    // ── Digest cap ───────────────────────────────────────────────────

    #[test]
    fn digest_cap_retains_10_most_recent_terminal() {
        let config = small_config(30);
        let mut table = ProcessTable::new(&config);

        // Create 5 running processes.
        for i in 0..5 {
            let alias = format!("running_{i}");
            table.register(&alias, "default", 100 + i as u32, None).unwrap();
        }

        // Create 18 terminal processes.
        for i in 0..18 {
            let alias = format!("done_{i}");
            table.register(&alias, "default", 200 + i as u32, None).unwrap();
            table
                .update_state(&alias, ProcessState::Completed)
                .unwrap();
        }

        // Total: 5 running + 18 terminal = 23 > 20.
        let digest = table.digest(DigestMode::Full);

        let running_count = digest.iter().filter(|e| e.state == "running").count();
        let terminal_count = digest
            .iter()
            .filter(|e| is_terminal_state(&e.state))
            .count();

        // All running processes should be retained.
        assert_eq!(running_count, 5);
        // Terminal capped at 10.
        assert_eq!(terminal_count, 10);
        // Total should be 15.
        assert_eq!(digest.len(), 15);
    }

    // ── Compact stubs ────────────────────────────────────────────────

    #[test]
    fn compact_stubs_after_seen_by_client() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);

        table.register("build", "default", 1234, None).unwrap();
        table.set_error_tail("build", "error[E0308]: mismatched types");
        table.set_output_summary("build", "Build failed with 3 errors");
        table.set_signal("build", "SIGTERM");
        table
            .update_state("build", ProcessState::Failed)
            .unwrap();
        table.set_exit_code("build", 1);

        // First digest: full entry with all fields.
        let digest = table.digest(DigestMode::Full);
        assert_eq!(digest.len(), 1);
        assert!(digest[0].error_tail.is_some());
        assert!(digest[0].output_summary.is_some());
        assert!(digest[0].signal.is_some());

        // Second digest: compact stub (terminal + seen_by_client).
        let digest = table.digest(DigestMode::Full);
        assert_eq!(digest.len(), 1);
        assert_eq!(digest[0].alias, "build");
        assert_eq!(digest[0].state, "failed");
        assert_eq!(digest[0].exit_code, Some(1));
        // Stub omits these fields.
        assert!(digest[0].error_tail.is_none());
        assert!(digest[0].output_summary.is_none());
        assert!(digest[0].signal.is_none());
    }

    // ── Cleanup ──────────────────────────────────────────────────────

    #[test]
    fn cleanup_removes_expired_terminal_entries() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);

        table.register("old_proc", "default", 100, None).unwrap();
        table
            .update_state("old_proc", ProcessState::Completed)
            .unwrap();

        // Set completed_at to the past by manipulating via a short sleep + zero retention.
        thread::sleep(Duration::from_millis(5));

        table.cleanup_expired(Duration::from_millis(1));

        assert!(!table.alias_exists("old_proc"));
    }

    #[test]
    fn cleanup_keeps_running_processes() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);

        table.register("running_proc", "default", 100, None).unwrap();

        table.cleanup_expired(Duration::from_millis(0));

        assert!(table.alias_exists("running_proc"));
    }

    // ── Dismiss ──────────────────────────────────────────────────────

    #[test]
    fn dismiss_removes_entry() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);

        table.register("build", "default", 1234, None).unwrap();
        assert!(table.alias_exists("build"));

        let result = table.dismiss("build");
        assert!(result.is_ok());
        assert!(!table.alias_exists("build"));
    }

    #[test]
    fn dismiss_nonexistent_returns_error() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);

        let result = table.dismiss("nonexistent");
        assert_eq!(result, Err(ProcessTableError::AliasNotFound));
    }

    // ── Sequence counter ─────────────────────────────────────────────

    #[test]
    fn sequence_counter_increments_on_every_mutation() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);

        // Register: seq 1.
        table.register("proc1", "default", 100, None).unwrap();
        let seq1 = table.get("proc1").unwrap().last_modified_seq;
        assert_eq!(seq1, 1);

        // State change: seq 2.
        table
            .update_state("proc1", ProcessState::AwaitingInput)
            .unwrap();
        let seq2 = table.get("proc1").unwrap().last_modified_seq;
        assert_eq!(seq2, 2);

        // Set prompt tail: seq 3.
        table.set_prompt_tail("proc1", "$ ");
        let seq3 = table.get("proc1").unwrap().last_modified_seq;
        assert_eq!(seq3, 3);

        // Set exit code: seq 4.
        table.set_exit_code("proc1", 0);
        let seq4 = table.get("proc1").unwrap().last_modified_seq;
        assert_eq!(seq4, 4);

        // Set signal: seq 5.
        table.set_signal("proc1", "SIGTERM");
        let seq5 = table.get("proc1").unwrap().last_modified_seq;
        assert_eq!(seq5, 5);

        // Set output summary: seq 6.
        table.set_output_summary("proc1", "done");
        let seq6 = table.get("proc1").unwrap().last_modified_seq;
        assert_eq!(seq6, 6);

        // Set error tail: seq 7.
        table.set_error_tail("proc1", "err");
        let seq7 = table.get("proc1").unwrap().last_modified_seq;
        assert_eq!(seq7, 7);
    }

    // ── Watch match recording ────────────────────────────────────────

    #[test]
    fn watch_match_recording_updates_last_match_and_count() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);

        table
            .register("server", "default", 1234, Some("error"))
            .unwrap();

        table.record_watch_match("server", "ERROR: connection refused");
        let entry = table.get("server").unwrap();
        assert_eq!(entry.last_match.as_deref(), Some("ERROR: connection refused"));
        assert_eq!(entry.match_count, 1);

        table.record_watch_match("server", "ERROR: timeout");
        let entry = table.get("server").unwrap();
        assert_eq!(entry.last_match.as_deref(), Some("ERROR: timeout"));
        assert_eq!(entry.match_count, 2);
    }

    // ── Error code mapping ───────────────────────────────────────────

    #[test]
    fn error_code_mapping() {
        assert_eq!(ProcessTableError::AliasNotFound.error_code(), -32003);
        assert_eq!(ProcessTableError::AliasInUse.error_code(), -32004);
        assert_eq!(ProcessTableError::ProcessLimitReached.error_code(), -32007);
        assert_eq!(
            ProcessTableError::InvalidStateTransition.error_code(),
            -32009
        );
    }

    // ── Get and active_count ─────────────────────────────────────────

    #[test]
    fn get_returns_none_for_unknown_alias() {
        let config = test_config();
        let table = ProcessTable::new(&config);
        assert!(table.get("nope").is_none());
    }

    #[test]
    fn active_count_excludes_terminal() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);

        table.register("a", "default", 1, None).unwrap();
        table.register("b", "default", 2, None).unwrap();
        table.register("c", "default", 3, None).unwrap();
        table.update_state("c", ProcessState::Completed).unwrap();

        assert_eq!(table.active_count(), 2);
    }

    // ── Digest entry fields ──────────────────────────────────────────

    #[test]
    fn digest_entry_has_watch_fields_when_matches_exist() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);

        table
            .register("server", "default", 1234, Some("error"))
            .unwrap();
        table.record_watch_match("server", "ERROR: fail");
        table.record_watch_match("server", "ERROR: crash");

        let digest = table.digest(DigestMode::Full);
        assert_eq!(digest.len(), 1);
        assert_eq!(digest[0].last_match.as_deref(), Some("ERROR: crash"));
        assert_eq!(digest[0].match_count, Some(2));
    }

    #[test]
    fn digest_entry_has_no_watch_fields_when_no_matches() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);

        table.register("server", "default", 1234, None).unwrap();

        let digest = table.digest(DigestMode::Full);
        assert_eq!(digest.len(), 1);
        assert!(digest[0].last_match.is_none());
        assert!(digest[0].match_count.is_none());
    }

    #[test]
    fn digest_entry_awaiting_input_has_notify_operator() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);

        table.register("proc", "default", 1234, None).unwrap();
        table
            .update_state("proc", ProcessState::AwaitingInput)
            .unwrap();
        table.set_prompt_tail("proc", "Password: ");

        let digest = table.digest(DigestMode::Full);
        assert_eq!(digest.len(), 1);
        assert_eq!(digest[0].notify_operator, Some(true));
        assert_eq!(digest[0].prompt_tail.as_deref(), Some("Password: "));
    }

    // ── Dismiss frees slot for new registration ──────────────────────

    #[test]
    fn dismiss_frees_slot_for_new_registration() {
        let config = small_config(2);
        let mut table = ProcessTable::new(&config);

        table.register("a", "default", 1, None).unwrap();
        table.register("b", "default", 2, None).unwrap();
        assert_eq!(
            table.register("c", "default", 3, None),
            Err(ProcessTableError::ProcessLimitReached)
        );

        table.dismiss("a").unwrap();
        assert!(table.register("c", "default", 3, None).is_ok());
    }

    // ── Interpreter field ─────────────────────────────────────────────

    #[test]
    fn interpreter_field_defaults_to_none() {
        let config = test_config();
        let mut table = ProcessTable::new(&config);
        table.register("py", "interpreter", 100, None).unwrap();

        let entry = table.get("py").unwrap();
        assert!(entry.interpreter.is_none());
    }
}
