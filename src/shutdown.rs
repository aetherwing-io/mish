//! Graceful shutdown manager for the mish MCP server.
//!
//! Handles:
//! - Signal handling: SIGTERM and SIGINT trigger graceful shutdown
//! - Drain timeout: wait up to N seconds for in-flight requests to complete
//! - Session cleanup: SIGTERM all process groups, wait, then SIGKILL survivors
//! - Audit log flush
//! - PID file lifecycle (write, remove, stale detection)
//!
//! Crash recovery on startup:
//! - Detect stale PID file at the configured location
//! - Clean up orphaned PTY file descriptors (by removing stale PID file)
//! - Remove stale PID file

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::session::manager::SessionManager;

// ---------------------------------------------------------------------------
// Default drain timeout
// ---------------------------------------------------------------------------

/// Default drain timeout: 5 seconds for processes to exit after SIGTERM
/// before sending SIGKILL.
const DEFAULT_DRAIN_TIMEOUT_SECS: u64 = 5;

// ---------------------------------------------------------------------------
// ShutdownManager
// ---------------------------------------------------------------------------

/// Coordinates graceful server shutdown.
///
/// Listens for SIGTERM / SIGINT, then runs the shutdown sequence:
/// 1. Signal via `watch` channel (stop accepting new tool calls)
/// 2. SIGTERM all session process groups
/// 3. Wait up to `drain_timeout` for processes to exit
/// 4. SIGKILL any remaining process groups
/// 5. Close all PTYs
/// 6. Remove PID file
/// 7. Flush and close audit log (caller responsibility — we signal completion)
pub struct ShutdownManager {
    session_manager: Arc<SessionManager>,
    drain_timeout: Duration,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
}

impl ShutdownManager {
    /// Create a new `ShutdownManager`.
    ///
    /// # Arguments
    /// - `session_manager` — shared handle to the session manager for cleanup.
    /// - `drain_timeout` — how long to wait after SIGTERM before sending SIGKILL.
    pub fn new(session_manager: Arc<SessionManager>, drain_timeout: Duration) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Self {
            session_manager,
            drain_timeout,
            shutdown_tx,
            shutdown_rx,
        }
    }

    /// Create a `ShutdownManager` with the default 5-second drain timeout.
    pub fn with_defaults(session_manager: Arc<SessionManager>) -> Self {
        Self::new(
            session_manager,
            Duration::from_secs(DEFAULT_DRAIN_TIMEOUT_SECS),
        )
    }

    /// Get a clone of the shutdown receiver.
    ///
    /// Other components (e.g. MCP transport) can watch this to stop accepting
    /// new tool calls when shutdown is triggered.
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.shutdown_rx.clone()
    }

    /// Check if shutdown has been triggered.
    pub fn is_shutdown(&self) -> bool {
        *self.shutdown_rx.borrow()
    }

    /// Trigger shutdown programmatically (e.g. on stdin EOF).
    pub fn trigger_shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Install signal handlers and wait for a shutdown signal.
    ///
    /// This listens for SIGTERM and SIGINT. When either arrives (or
    /// `trigger_shutdown()` is called), the shutdown sequence executes.
    ///
    /// This method runs until shutdown is complete. Call it as a spawned task.
    pub async fn wait_for_shutdown(&self) {
        // Set up signal listeners.
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};

            let mut sigterm =
                signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
            let mut sigint =
                signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");

            let mut rx = self.shutdown_rx.clone();

            tokio::select! {
                _ = sigterm.recv() => {
                    eprintln!("mish: received SIGTERM, initiating graceful shutdown");
                }
                _ = sigint.recv() => {
                    eprintln!("mish: received SIGINT, initiating graceful shutdown");
                }
                _ = rx.changed() => {
                    // Programmatic trigger (e.g. stdin EOF).
                    if *rx.borrow() {
                        eprintln!("mish: shutdown triggered programmatically");
                    }
                }
            }
        }

        // For non-Unix platforms, just wait for programmatic trigger.
        #[cfg(not(unix))]
        {
            let mut rx = self.shutdown_rx.clone();
            let _ = rx.changed().await;
        }

        // Ensure the signal is broadcast to all subscribers.
        let _ = self.shutdown_tx.send(true);

        // Execute the shutdown sequence.
        self.shutdown_sequence().await;
    }

    /// Execute the shutdown sequence.
    ///
    /// 1. SIGTERM all session process groups
    /// 2. Wait up to `drain_timeout` for processes to exit
    /// 3. SIGKILL remaining via `close_all()`
    /// 4. (PID file removal and audit flush are caller responsibilities,
    ///    since those resources aren't owned by ShutdownManager)
    async fn shutdown_sequence(&self) {
        eprintln!("mish: shutting down...");

        // Step 1: Send SIGTERM to all session process groups.
        self.terminate_all_sessions().await;

        // Step 2: Wait for drain_timeout, giving processes a chance to exit.
        tokio::time::sleep(self.drain_timeout).await;

        // Step 3: SIGKILL any remaining processes and close all PTYs.
        self.session_manager.close_all().await;

        eprintln!("mish: shutdown complete");
    }

    /// Send SIGTERM to all sessions' process groups.
    async fn terminate_all_sessions(&self) {
        let sessions = self.session_manager.list_sessions().await;
        for info in &sessions {
            if let Some(session) = self.session_manager.get_session(&info.name).await {
                session.terminate();
            }
        }
    }

    // -----------------------------------------------------------------------
    // PID file management
    // -----------------------------------------------------------------------

    /// Determine the PID file directory.
    ///
    /// Uses `$XDG_RUNTIME_DIR/mish/` if set, otherwise `/tmp/mish/`.
    pub fn pid_dir() -> String {
        match std::env::var("XDG_RUNTIME_DIR") {
            Ok(dir) if !dir.is_empty() => format!("{dir}/mish"),
            _ => "/tmp/mish".to_string(),
        }
    }

    /// Return the PID file path for a given PID.
    pub fn pid_file_path(pid: u32) -> String {
        format!("{}/{pid}.pid", Self::pid_dir())
    }

    /// Return the PID file path for the current process.
    pub fn current_pid_file_path() -> String {
        Self::pid_file_path(std::process::id())
    }

    /// Write a PID file for the current process.
    ///
    /// Creates parent directories if they don't exist. The PID file contains
    /// just the PID as a decimal string followed by a newline.
    pub fn write_pid_file(pid_path: &str) -> std::io::Result<()> {
        let path = Path::new(pid_path);

        // Create parent directories.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let pid = std::process::id();
        std::fs::write(path, format!("{pid}\n"))
    }

    /// Remove a PID file.
    ///
    /// Returns `Ok(())` even if the file doesn't exist (idempotent).
    pub fn remove_pid_file(pid_path: &str) -> std::io::Result<()> {
        match std::fs::remove_file(pid_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Check for a stale PID file at the given path.
    ///
    /// A PID file is considered stale if:
    /// - It exists and contains a PID
    /// - The process with that PID is no longer running
    ///
    /// Returns `true` if the PID file is stale (and should be cleaned up).
    /// Returns `false` if the PID file doesn't exist, or if the process
    /// is still running (which means another mish instance is active).
    pub fn check_stale_pid(pid_path: &str) -> bool {
        let content = match std::fs::read_to_string(pid_path) {
            Ok(c) => c,
            Err(_) => return false, // File doesn't exist or unreadable.
        };

        let pid_str = content.trim();
        let pid: i32 = match pid_str.parse() {
            Ok(p) => p,
            Err(_) => {
                // Corrupt PID file — treat as stale.
                eprintln!("mish: warning: corrupt PID file at {pid_path}");
                return true;
            }
        };

        // Check if the process is still running using kill(pid, 0).
        // This doesn't send a signal — it just checks if the process exists.
        #[cfg(unix)]
        {
            use nix::sys::signal::kill;
            use nix::unistd::Pid;

            match kill(Pid::from_raw(pid), None) {
                Ok(()) => {
                    // Process exists — not stale.
                    false
                }
                Err(nix::errno::Errno::ESRCH) => {
                    // No such process — stale.
                    true
                }
                Err(nix::errno::Errno::EPERM) => {
                    // Process exists but we don't have permission to signal it.
                    // Conservative: not stale (another mish might be running as
                    // a different user).
                    false
                }
                Err(_) => {
                    // Unknown error — treat as not stale to be safe.
                    false
                }
            }
        }

        #[cfg(not(unix))]
        {
            // On non-Unix, we can't check — assume not stale.
            let _ = pid;
            false
        }
    }

    /// Scan the PID directory for any stale PID files and clean them up.
    ///
    /// Returns a list of stale PIDs that were cleaned up.
    pub fn cleanup_stale_pid_files() -> Vec<u32> {
        let dir = Self::pid_dir();
        let dir_path = Path::new(&dir);

        if !dir_path.exists() {
            return Vec::new();
        }

        let mut cleaned = Vec::new();

        if let Ok(entries) = std::fs::read_dir(dir_path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("pid") {
                    let path_str = path.to_string_lossy().to_string();
                    if Self::check_stale_pid(&path_str) {
                        // Read the PID for reporting.
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            if let Ok(pid) = content.trim().parse::<u32>() {
                                eprintln!(
                                    "mish: warning: cleaning up stale PID file for process {pid}"
                                );
                                cleaned.push(pid);
                            }
                        }
                        // Remove the stale file.
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }

        cleaned
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;

    use crate::config::default_config;
    use serial_test::serial;

    fn test_session_manager() -> Arc<SessionManager> {
        Arc::new(SessionManager::new(Arc::new(default_config())))
    }

    // -- PID file tests --

    // Test 1: Write and read PID file.
    #[test]
    fn test_write_pid_file() {
        let dir = TempDir::new().unwrap();
        let pid_path = dir
            .path()
            .join("test.pid")
            .to_string_lossy()
            .to_string();

        ShutdownManager::write_pid_file(&pid_path).expect("should write PID file");

        let content = std::fs::read_to_string(&pid_path).expect("should read PID file");
        let pid: u32 = content.trim().parse().expect("should parse PID");
        assert_eq!(pid, std::process::id());
    }

    // Test 2: Remove PID file.
    #[test]
    fn test_remove_pid_file() {
        let dir = TempDir::new().unwrap();
        let pid_path = dir
            .path()
            .join("test.pid")
            .to_string_lossy()
            .to_string();

        ShutdownManager::write_pid_file(&pid_path).expect("write");
        assert!(Path::new(&pid_path).exists());

        ShutdownManager::remove_pid_file(&pid_path).expect("remove");
        assert!(!Path::new(&pid_path).exists());
    }

    // Test 3: Remove nonexistent PID file is idempotent (no error).
    #[test]
    fn test_remove_nonexistent_pid_file() {
        let result = ShutdownManager::remove_pid_file("/tmp/mish_test_nonexistent_12345.pid");
        assert!(result.is_ok());
    }

    // Test 4: Write PID file creates parent directories.
    #[test]
    fn test_write_pid_file_creates_dirs() {
        let dir = TempDir::new().unwrap();
        let pid_path = dir
            .path()
            .join("a")
            .join("b")
            .join("c")
            .join("test.pid")
            .to_string_lossy()
            .to_string();

        ShutdownManager::write_pid_file(&pid_path).expect("should create dirs and write");
        assert!(Path::new(&pid_path).exists());

        let content = std::fs::read_to_string(&pid_path).unwrap();
        let pid: u32 = content.trim().parse().unwrap();
        assert_eq!(pid, std::process::id());
    }

    // Test 5: Stale PID detection — non-existent file returns false.
    #[test]
    fn test_check_stale_pid_no_file() {
        assert!(!ShutdownManager::check_stale_pid(
            "/tmp/mish_test_no_such_file_999999.pid"
        ));
    }

    // Test 6: Stale PID detection — current process PID is not stale.
    #[test]
    fn test_check_stale_pid_current_process() {
        let dir = TempDir::new().unwrap();
        let pid_path = dir
            .path()
            .join("current.pid")
            .to_string_lossy()
            .to_string();

        // Write our own PID — should NOT be stale.
        ShutdownManager::write_pid_file(&pid_path).unwrap();
        assert!(
            !ShutdownManager::check_stale_pid(&pid_path),
            "current process PID should not be stale"
        );
    }

    // Test 7: Stale PID detection — dead process PID is stale.
    #[test]
    fn test_check_stale_pid_dead_process() {
        let dir = TempDir::new().unwrap();
        let pid_path = dir
            .path()
            .join("dead.pid")
            .to_string_lossy()
            .to_string();

        // Use a PID that almost certainly doesn't exist.
        // PID 4_000_000 is well above typical PID ranges.
        std::fs::write(&pid_path, "4000000\n").unwrap();

        assert!(
            ShutdownManager::check_stale_pid(&pid_path),
            "dead process PID should be stale"
        );
    }

    // Test 8: Stale PID detection — corrupt PID file is stale.
    #[test]
    fn test_check_stale_pid_corrupt_file() {
        let dir = TempDir::new().unwrap();
        let pid_path = dir
            .path()
            .join("corrupt.pid")
            .to_string_lossy()
            .to_string();

        std::fs::write(&pid_path, "not_a_number\n").unwrap();

        assert!(
            ShutdownManager::check_stale_pid(&pid_path),
            "corrupt PID file should be treated as stale"
        );
    }

    // Test 9: PID directory uses XDG_RUNTIME_DIR when set.
    #[test]
    #[serial]
    fn test_pid_dir_xdg() {
        // Save and restore XDG_RUNTIME_DIR.
        let original = std::env::var("XDG_RUNTIME_DIR").ok();

        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        assert_eq!(ShutdownManager::pid_dir(), "/run/user/1000/mish");

        // Restore.
        match original {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    // Test 10: PID file path format is correct.
    #[test]
    fn test_pid_file_path_format() {
        let path = ShutdownManager::pid_file_path(12345);
        assert!(
            path.ends_with("/12345.pid"),
            "PID file path should end with /<pid>.pid, got: {path}"
        );
        assert!(
            path.contains("mish"),
            "PID file path should contain 'mish', got: {path}"
        );
    }

    // -- ShutdownManager construction and state tests --

    // Test 11: ShutdownManager starts in non-shutdown state.
    #[test]
    fn test_initial_state_not_shutdown() {
        let mgr = ShutdownManager::new(
            test_session_manager(),
            Duration::from_secs(5),
        );
        assert!(!mgr.is_shutdown());
    }

    // Test 12: trigger_shutdown sets the shutdown flag.
    #[test]
    fn test_trigger_shutdown_sets_flag() {
        let mgr = ShutdownManager::new(
            test_session_manager(),
            Duration::from_secs(5),
        );
        assert!(!mgr.is_shutdown());

        mgr.trigger_shutdown();
        assert!(mgr.is_shutdown());
    }

    // Test 13: Subscribers receive shutdown signal.
    #[tokio::test]
    async fn test_subscriber_receives_shutdown() {
        let mgr = ShutdownManager::new(
            test_session_manager(),
            Duration::from_secs(5),
        );

        let mut rx = mgr.subscribe();
        assert!(!*rx.borrow());

        mgr.trigger_shutdown();

        // Wait for the signal to propagate.
        rx.changed().await.expect("should receive signal");
        assert!(*rx.borrow());
    }

    // Test 14: Multiple subscribers all receive the signal.
    #[tokio::test]
    async fn test_multiple_subscribers() {
        let mgr = ShutdownManager::new(
            test_session_manager(),
            Duration::from_secs(5),
        );

        let mut rx1 = mgr.subscribe();
        let mut rx2 = mgr.subscribe();

        mgr.trigger_shutdown();

        rx1.changed().await.expect("rx1 should receive");
        rx2.changed().await.expect("rx2 should receive");
        assert!(*rx1.borrow());
        assert!(*rx2.borrow());
    }

    // Test 15: with_defaults creates manager with 5-second drain timeout.
    #[test]
    fn test_with_defaults() {
        let mgr = ShutdownManager::with_defaults(test_session_manager());
        assert_eq!(mgr.drain_timeout, Duration::from_secs(5));
        assert!(!mgr.is_shutdown());
    }

    // Test 16: Shutdown sequence calls close_all on session manager.
    #[tokio::test]
    async fn test_shutdown_sequence_closes_sessions() {
        let sm = test_session_manager();

        // Create a session so there's something to close.
        sm.create_session("test_shutdown", Some("/bin/bash"))
            .await
            .expect("create session");
        assert_eq!(sm.session_count().await, 1);

        let mgr = ShutdownManager::new(
            sm.clone(),
            Duration::from_millis(100), // Short drain for test speed.
        );

        // Run shutdown_sequence directly.
        mgr.shutdown_sequence().await;

        // All sessions should be closed.
        assert_eq!(sm.session_count().await, 0);
    }

    // Test 17: Shutdown sequence is idempotent (no sessions to close).
    #[tokio::test]
    async fn test_shutdown_sequence_no_sessions() {
        let sm = test_session_manager();
        assert_eq!(sm.session_count().await, 0);

        let mgr = ShutdownManager::new(sm.clone(), Duration::from_millis(50));

        // Should not panic when there are no sessions.
        mgr.shutdown_sequence().await;
        assert_eq!(sm.session_count().await, 0);
    }

    // Test 18: Cleanup stale PID files in a temp directory.
    #[test]
    fn test_cleanup_stale_pid_files() {
        let dir = TempDir::new().unwrap();

        // Write a stale PID file (dead process).
        let stale_path = dir.path().join("4000000.pid");
        std::fs::write(&stale_path, "4000000\n").unwrap();

        // Write a non-stale PID file (our own process).
        let live_path = dir.path().join(format!("{}.pid", std::process::id()));
        std::fs::write(&live_path, format!("{}\n", std::process::id())).unwrap();

        // We can't use cleanup_stale_pid_files() directly because it uses
        // the real PID dir. Instead, verify check_stale_pid for each.
        assert!(ShutdownManager::check_stale_pid(
            &stale_path.to_string_lossy()
        ));
        assert!(!ShutdownManager::check_stale_pid(
            &live_path.to_string_lossy()
        ));
    }

    // Test 19: trigger_shutdown is idempotent — calling it twice is fine.
    #[test]
    fn test_trigger_shutdown_idempotent() {
        let mgr = ShutdownManager::new(
            test_session_manager(),
            Duration::from_secs(5),
        );

        mgr.trigger_shutdown();
        assert!(mgr.is_shutdown());

        // Second call should not panic.
        mgr.trigger_shutdown();
        assert!(mgr.is_shutdown());
    }

    // Test 20: Drain timeout is configurable.
    #[test]
    fn test_custom_drain_timeout() {
        let mgr = ShutdownManager::new(
            test_session_manager(),
            Duration::from_secs(10),
        );
        assert_eq!(mgr.drain_timeout, Duration::from_secs(10));

        let mgr2 = ShutdownManager::new(
            test_session_manager(),
            Duration::from_millis(500),
        );
        assert_eq!(mgr2.drain_timeout, Duration::from_millis(500));
    }
}
