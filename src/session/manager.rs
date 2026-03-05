//! Session manager — owns all sessions and coordinates access.
//!
//! `SessionManager` is the central coordinator for MCP server sessions. Each
//! session wraps an interactive shell process in a PTY and provides serialized
//! command execution via per-session mutexes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use tokio::sync::{Mutex as TokioMutex, RwLock};

use crate::config::MishConfig;
use crate::mcp::types::{
    ERR_SESSION_NOT_FOUND as ERR_NOT_FOUND,
    ERR_SESSION_LIMIT as ERR_LIMIT_REACHED,
    ERR_SESSION_NOT_READY as ERR_NOT_READY,
    ERR_SHELL_ERROR,
};
use crate::session::shell::{CommandResult, ShellError, ShellProcess};

// ---------------------------------------------------------------------------
// SessionError
// ---------------------------------------------------------------------------

/// Error type for session operations.
#[derive(Debug)]
pub enum SessionError {
    /// Session with the given name was not found.
    NotFound(String),
    /// Session limit reached.
    LimitReached { current: usize, max: usize },
    /// Session exists but is not ready for commands.
    NotReady(String),
    /// Session with that name already exists.
    AlreadyExists(String),
    /// Underlying shell error.
    ShellError(ShellError),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::NotFound(name) => write!(f, "session not found: {name}"),
            SessionError::LimitReached { current, max } => {
                write!(f, "session limit reached: {current}/{max}")
            }
            SessionError::NotReady(name) => write!(f, "session not ready: {name}"),
            SessionError::AlreadyExists(name) => write!(f, "session already exists: {name}"),
            SessionError::ShellError(e) => write!(f, "shell error: {e}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<ShellError> for SessionError {
    fn from(e: ShellError) -> Self {
        SessionError::ShellError(e)
    }
}

impl SessionError {
    /// Return the MCP error code for this error.
    pub fn error_code(&self) -> i32 {
        match self {
            SessionError::NotFound(_) => ERR_NOT_FOUND,
            SessionError::LimitReached { .. } => ERR_LIMIT_REACHED,
            SessionError::NotReady(_) => ERR_NOT_READY,
            SessionError::AlreadyExists(_) => ERR_LIMIT_REACHED,
            SessionError::ShellError(_) => ERR_SHELL_ERROR,
        }
    }
}

// ---------------------------------------------------------------------------
// SessionInfo
// ---------------------------------------------------------------------------

/// Summary information about a session (for listing).
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub name: String,
    pub pid: u32,
    pub cwd: String,
    pub created_at: Instant,
    pub last_activity: Instant,
    pub ready: bool,
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// A managed session wrapping an interactive shell process.
///
/// The shell is behind a `tokio::sync::Mutex` to serialize command execution.
/// Metadata (cwd, last_activity) is cached outside the mutex for non-blocking
/// access during `list_sessions`.
pub struct Session {
    name: String,
    shell: TokioMutex<ShellProcess>,
    pid: u32,
    created_at: Instant,
    last_activity: std::sync::Mutex<Instant>,
    cwd: std::sync::Mutex<String>,
    ready: AtomicBool,
    pub hints_shown: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl Session {
    /// Get the session name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the shell PID.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Get the creation time.
    pub fn created_at(&self) -> Instant {
        self.created_at
    }

    /// Get the last activity time.
    pub fn last_activity(&self) -> Instant {
        *self.last_activity.lock().unwrap()
    }

    /// Get the current working directory (cached, non-blocking).
    pub fn cwd(&self) -> String {
        self.cwd.lock().unwrap().clone()
    }

    /// Check if the session is ready for commands.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Get a SessionInfo snapshot.
    pub fn info(&self) -> SessionInfo {
        SessionInfo {
            name: self.name.clone(),
            pid: self.pid,
            cwd: self.cwd(),
            created_at: self.created_at,
            last_activity: self.last_activity(),
            ready: self.is_ready(),
        }
    }

    /// Kill the shell process group (SIGKILL). Does not require shell mutex.
    pub fn kill(&self) {
        let pid = Pid::from_raw(self.pid as i32);
        // Try process group first, fall back to direct kill.
        let _ = killpg(pid, Signal::SIGKILL);
        let _ = nix::sys::signal::kill(pid, Signal::SIGKILL);
    }

    /// Send SIGTERM to the shell process group.
    pub fn terminate(&self) {
        let pid = Pid::from_raw(self.pid as i32);
        let _ = killpg(pid, Signal::SIGTERM);
        let _ = nix::sys::signal::kill(pid, Signal::SIGTERM);
    }

    /// Touch the last_activity timestamp.
    fn touch(&self) {
        *self.last_activity.lock().unwrap() = Instant::now();
    }

    /// Update the cached CWD.
    fn update_cwd(&self, cwd: &str) {
        *self.cwd.lock().unwrap() = cwd.to_string();
    }
}

// ---------------------------------------------------------------------------
// SessionManager
// ---------------------------------------------------------------------------

/// Session manager — creates, tracks, and coordinates access to sessions.
///
/// Thread-safe: uses `RwLock` for the session map and per-session `Mutex`
/// for shell access. Designed to be shared via `Arc<SessionManager>`.
pub struct SessionManager {
    sessions: RwLock<HashMap<String, Arc<Session>>>,
    config: Arc<MishConfig>,
}

impl SessionManager {
    /// Create a new SessionManager with the given configuration.
    pub fn new(config: Arc<MishConfig>) -> Self {
        SessionManager {
            sessions: RwLock::new(HashMap::new()),
            config,
        }
    }

    /// Create a new session with the given name and optional shell path.
    ///
    /// If `shell_path` is `None`, uses `$SHELL` or falls back to `/bin/sh`.
    /// The session is fully initialized (hooks injected, prompt detected)
    /// before being added to the session map.
    pub async fn create_session(
        &self,
        name: &str,
        shell_path: Option<&str>,
    ) -> Result<SessionInfo, SessionError> {
        // Pre-check: name conflict and limit (read lock — fast path).
        {
            let sessions = self.sessions.read().await;
            if sessions.contains_key(name) {
                return Err(SessionError::AlreadyExists(name.to_string()));
            }
            let max = self.config.server.max_sessions;
            if sessions.len() >= max {
                return Err(SessionError::LimitReached {
                    current: sessions.len(),
                    max,
                });
            }
        }

        // Determine shell path.
        let shell = shell_path
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
            });

        // Determine initial CWD.
        let initial_cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "/tmp".to_string());

        // Spawn and initialize the shell process.
        let mut shell_process =
            ShellProcess::spawn(&shell, &initial_cwd).await?;
        let pid = shell_process.pid();
        shell_process.initialize().await?;

        let now = Instant::now();
        let cwd = shell_process.cwd().to_string();

        let session = Arc::new(Session {
            name: name.to_string(),
            shell: TokioMutex::new(shell_process),
            pid,
            created_at: now,
            last_activity: std::sync::Mutex::new(now),
            cwd: std::sync::Mutex::new(cwd),
            ready: AtomicBool::new(true),
            hints_shown: std::sync::Mutex::new(std::collections::HashSet::new()),
        });

        let info = session.info();

        // Insert with write lock. Double-check after acquiring since spawn is async.
        {
            let mut sessions = self.sessions.write().await;
            if sessions.contains_key(name) {
                session.kill();
                return Err(SessionError::AlreadyExists(name.to_string()));
            }
            if sessions.len() >= self.config.server.max_sessions {
                session.kill();
                return Err(SessionError::LimitReached {
                    current: sessions.len(),
                    max: self.config.server.max_sessions,
                });
            }
            sessions.insert(name.to_string(), session);
        }

        Ok(info)
    }

    /// Create the default "main" session.
    pub async fn create_main_session(&self) -> Result<SessionInfo, SessionError> {
        self.create_session("main", None).await
    }

    /// Ensure the default "main" session exists, creating it if needed.
    /// Returns `Ok(true)` if newly created, `Ok(false)` if it already existed.
    pub async fn ensure_default_session(&self) -> Result<bool, SessionError> {
        // Fast path: session exists (read lock only).
        if self.get_session("main").await.is_some() {
            return Ok(false);
        }
        // Cold path: create it.
        match self.create_session("main", None).await {
            Ok(_) => Ok(true),
            Err(SessionError::AlreadyExists(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Get a session by name.
    pub async fn get_session(&self, name: &str) -> Option<Arc<Session>> {
        self.sessions.read().await.get(name).cloned()
    }

    /// List all sessions (sorted by name).
    pub async fn list_sessions(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.read().await;
        let mut infos: Vec<SessionInfo> = sessions.values().map(|s| s.info()).collect();
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        infos
    }

    /// Return the number of active sessions.
    pub async fn session_count(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// Execute a command in a named session. Serialized by per-session mutex.
    pub async fn execute_in_session(
        &self,
        session_name: &str,
        cmd: &str,
        timeout: Duration,
    ) -> Result<CommandResult, SessionError> {
        let session = self
            .get_session(session_name)
            .await
            .ok_or_else(|| SessionError::NotFound(session_name.to_string()))?;

        if !session.is_ready() {
            return Err(SessionError::NotReady(session_name.to_string()));
        }

        // Acquire session's shell mutex — serializes command execution.
        let mut shell = session.shell.lock().await;
        session.touch();

        let result = shell.execute(cmd, timeout).await?;

        // Update cached metadata.
        session.update_cwd(&result.cwd);
        session.touch();

        Ok(result)
    }

    /// Write raw input to a session's shell stdin.
    pub async fn write_to_session(
        &self,
        session_name: &str,
        input: &[u8],
    ) -> Result<usize, SessionError> {
        let session = self
            .get_session(session_name)
            .await
            .ok_or_else(|| SessionError::NotFound(session_name.to_string()))?;

        let mut shell = session.shell.lock().await;
        session.touch();

        let n = shell.write_stdin(input).await?;
        Ok(n)
    }

    /// Read available output from a session's shell (non-blocking).
    pub async fn read_from_session(
        &self,
        session_name: &str,
        buf: &mut [u8],
    ) -> Result<usize, SessionError> {
        let session = self
            .get_session(session_name)
            .await
            .ok_or_else(|| SessionError::NotFound(session_name.to_string()))?;

        let mut shell = session.shell.lock().await;
        let n = shell.read_output(buf).await?;
        Ok(n)
    }

    /// Close a session, killing its shell process.
    pub async fn close_session(&self, name: &str) -> Result<(), SessionError> {
        let session = {
            let mut sessions = self.sessions.write().await;
            sessions
                .remove(name)
                .ok_or_else(|| SessionError::NotFound(name.to_string()))?
        };

        session.kill();
        Ok(())
    }

    /// Close all sessions (for shutdown).
    pub async fn close_all(&self) {
        let sessions: Vec<Arc<Session>> = {
            let mut map = self.sessions.write().await;
            map.drain().map(|(_, s)| s).collect()
        };

        for session in sessions {
            session.kill();
        }
    }

    /// Check for idle sessions and close them.
    ///
    /// Returns the names of sessions that were closed.
    pub async fn cleanup_idle_sessions(&self) -> Vec<String> {
        let timeout = Duration::from_secs(self.config.server.idle_session_timeout_sec);
        let now = Instant::now();

        // Find idle sessions (read lock).
        let to_remove: Vec<String> = {
            let sessions = self.sessions.read().await;
            sessions
                .iter()
                .filter(|(_, session)| now.duration_since(session.last_activity()) > timeout)
                .map(|(name, _)| name.clone())
                .collect()
        };

        // Remove them (write lock).
        if !to_remove.is_empty() {
            let mut sessions = self.sessions.write().await;
            for name in &to_remove {
                if let Some(session) = sessions.remove(name) {
                    session.kill();
                }
            }
        }

        to_remove
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_config;

    /// Use /bin/bash for all tests (avoids zsh/Oh My Zsh ANSI noise).
    fn bash_path() -> &'static str {
        "/bin/bash"
    }

    fn test_config() -> Arc<MishConfig> {
        Arc::new(default_config())
    }

    fn test_config_with_max(max_sessions: usize) -> Arc<MishConfig> {
        let mut config = default_config();
        config.server.max_sessions = max_sessions;
        Arc::new(config)
    }

    // Test 1: Main session created successfully at startup.
    #[tokio::test]
    async fn test_create_main_session() {
        let mgr = SessionManager::new(test_config());
        let info = mgr
            .create_session("main", Some(bash_path()))
            .await
            .expect("should create main session");

        assert_eq!(info.name, "main");
        assert!(info.pid > 0);
        assert!(info.ready);

        mgr.close_all().await;
    }

    // Test 2: get_session("main") returns the main session.
    #[tokio::test]
    async fn test_get_session_main() {
        let mgr = SessionManager::new(test_config());
        mgr.create_session("main", Some(bash_path()))
            .await
            .expect("create");

        let session = mgr.get_session("main").await;
        assert!(session.is_some());
        assert_eq!(session.unwrap().name(), "main");

        mgr.close_all().await;
    }

    // Test 3: get_session("nonexistent") returns None.
    #[tokio::test]
    async fn test_get_session_nonexistent() {
        let mgr = SessionManager::new(test_config());
        assert!(mgr.get_session("nonexistent").await.is_none());
    }

    // Test 4: list_sessions returns ["main"] after startup.
    #[tokio::test]
    async fn test_list_sessions_after_main() {
        let mgr = SessionManager::new(test_config());
        mgr.create_session("main", Some(bash_path()))
            .await
            .expect("create");

        let sessions = mgr.list_sessions().await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "main");
        assert!(sessions[0].ready);

        mgr.close_all().await;
    }

    // Test 5: execute_in_session("main", "echo hello") returns output.
    #[tokio::test]
    async fn test_execute_in_session() {
        let mgr = SessionManager::new(test_config());
        mgr.create_session("main", Some(bash_path()))
            .await
            .expect("create");

        let result = mgr
            .execute_in_session("main", "echo hello", Duration::from_secs(5))
            .await
            .expect("execute");

        assert_eq!(result.exit_code, 0);
        assert!(
            result.output.contains("hello"),
            "output should contain 'hello', got: {:?}",
            result.output
        );

        mgr.close_all().await;
    }

    // Test 6: Session not found returns error code -32002.
    #[tokio::test]
    async fn test_session_not_found() {
        let mgr = SessionManager::new(test_config());

        let result = mgr
            .execute_in_session("ghost", "echo hi", Duration::from_secs(5))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.error_code(), ERR_NOT_FOUND);
        match err {
            SessionError::NotFound(name) => assert_eq!(name, "ghost"),
            other => panic!("expected NotFound, got: {other:?}"),
        }
    }

    // Test 7: Session limit enforcement returns -32006.
    #[tokio::test]
    async fn test_session_limit_reached() {
        let mgr = SessionManager::new(test_config_with_max(1));
        mgr.create_session("first", Some("/bin/bash"))
            .await
            .expect("create first");

        let result = mgr.create_session("second", Some("/bin/bash")).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.error_code(), ERR_LIMIT_REACHED);
        match err {
            SessionError::LimitReached { current: 1, max: 1 } => {}
            other => panic!("expected LimitReached {{1, 1}}, got: {other:?}"),
        }

        mgr.close_all().await;
    }

    // Test 8: Duplicate session name rejected.
    #[tokio::test]
    async fn test_duplicate_session_name() {
        let mgr = SessionManager::new(test_config());
        mgr.create_session("main", Some(bash_path()))
            .await
            .expect("create");

        let result = mgr.create_session("main", Some(bash_path())).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SessionError::AlreadyExists(name) => assert_eq!(name, "main"),
            other => panic!("expected AlreadyExists, got: {other:?}"),
        }

        mgr.close_all().await;
    }

    // Test 9: close_session removes it from the map.
    #[tokio::test]
    async fn test_close_session() {
        let mgr = SessionManager::new(test_config());
        mgr.create_session("main", Some(bash_path()))
            .await
            .expect("create");

        mgr.close_session("main").await.expect("close");

        assert!(mgr.get_session("main").await.is_none());
        assert_eq!(mgr.session_count().await, 0);
    }

    // Test 10: close_all kills all sessions.
    #[tokio::test]
    async fn test_close_all() {
        let mgr = SessionManager::new(test_config());
        mgr.create_session("a", Some("/bin/bash"))
            .await
            .expect("create a");
        mgr.create_session("b", Some("/bin/bash"))
            .await
            .expect("create b");

        assert_eq!(mgr.session_count().await, 2);

        mgr.close_all().await;

        assert_eq!(mgr.session_count().await, 0);
    }

    // Test 11: Idle session cleanup closes sessions past timeout.
    #[tokio::test]
    async fn test_idle_session_cleanup() {
        let mut config = default_config();
        config.server.idle_session_timeout_sec = 0; // immediate timeout
        let mgr = SessionManager::new(Arc::new(config));

        mgr.create_session("main", Some(bash_path()))
            .await
            .expect("create");
        assert_eq!(mgr.session_count().await, 1);

        // Wait a tiny bit so the session is "idle".
        tokio::time::sleep(Duration::from_millis(10)).await;

        let removed = mgr.cleanup_idle_sessions().await;
        assert_eq!(removed, vec!["main"]);
        assert_eq!(mgr.session_count().await, 0);
    }

    // Test 12: Error codes map correctly.
    #[test]
    fn test_error_codes() {
        assert_eq!(
            SessionError::NotFound("x".into()).error_code(),
            ERR_NOT_FOUND
        );
        assert_eq!(
            SessionError::LimitReached {
                current: 5,
                max: 5,
            }
            .error_code(),
            ERR_LIMIT_REACHED
        );
        assert_eq!(
            SessionError::NotReady("x".into()).error_code(),
            ERR_NOT_READY
        );
        assert_eq!(
            SessionError::AlreadyExists("x".into()).error_code(),
            ERR_LIMIT_REACHED
        );
    }

    // Test 13: CWD tracking through execute_in_session.
    #[tokio::test]
    async fn test_cwd_tracking_through_manager() {
        let mgr = SessionManager::new(test_config());
        mgr.create_session("main", Some(bash_path()))
            .await
            .expect("create");

        let result = mgr
            .execute_in_session("main", "cd /tmp", Duration::from_secs(5))
            .await
            .expect("cd");

        let cwd = result.cwd.clone();
        assert!(
            cwd == "/tmp" || cwd == "/private/tmp",
            "CWD should be /tmp or /private/tmp, got: {cwd}"
        );

        // Verify cached CWD is updated in session info.
        let sessions = mgr.list_sessions().await;
        let main_info = &sessions[0];
        assert!(
            main_info.cwd == "/tmp" || main_info.cwd == "/private/tmp",
            "cached CWD should match, got: {}",
            main_info.cwd
        );

        mgr.close_all().await;
    }

    // Test 14: close_session on nonexistent returns NotFound.
    #[tokio::test]
    async fn test_close_nonexistent_session() {
        let mgr = SessionManager::new(test_config());

        let result = mgr.close_session("ghost").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SessionError::NotFound(name) => assert_eq!(name, "ghost"),
            other => panic!("expected NotFound, got: {other:?}"),
        }
    }

    // Test 15: SessionError Display impls.
    #[test]
    fn test_error_display() {
        let err = SessionError::NotFound("test".into());
        assert!(format!("{err}").contains("not found"));
        assert!(format!("{err}").contains("test"));

        let err = SessionError::LimitReached {
            current: 5,
            max: 5,
        };
        assert!(format!("{err}").contains("limit"));

        let err = SessionError::NotReady("test".into());
        assert!(format!("{err}").contains("not ready"));

        let err = SessionError::AlreadyExists("test".into());
        assert!(format!("{err}").contains("already exists"));
    }

    // Test 16: Multiple commands in same session preserve state.
    #[tokio::test]
    async fn test_environment_persistence_through_manager() {
        let mgr = SessionManager::new(test_config());
        mgr.create_session("main", Some(bash_path()))
            .await
            .expect("create");

        let result = mgr
            .execute_in_session(
                "main",
                "export MISH_MGR_TEST=footest",
                Duration::from_secs(5),
            )
            .await
            .expect("export");
        assert_eq!(result.exit_code, 0);

        let result = mgr
            .execute_in_session("main", "echo $MISH_MGR_TEST", Duration::from_secs(5))
            .await
            .expect("echo");
        assert_eq!(result.exit_code, 0);
        assert!(
            result.output.contains("footest"),
            "output should contain 'footest', got: {:?}",
            result.output
        );

        mgr.close_all().await;
    }

    // Test 17: Session with explicit shell path.
    #[tokio::test]
    async fn test_create_session_with_explicit_shell() {
        let mgr = SessionManager::new(test_config());
        let info = mgr
            .create_session("custom", Some("/bin/bash"))
            .await
            .expect("create with explicit shell");

        assert_eq!(info.name, "custom");
        assert!(info.pid > 0);
        assert!(info.ready);

        mgr.close_all().await;
    }

    // Test 18: session_count tracks correctly.
    #[tokio::test]
    async fn test_session_count() {
        let mgr = SessionManager::new(test_config());

        assert_eq!(mgr.session_count().await, 0);

        mgr.create_session("a", Some("/bin/bash"))
            .await
            .expect("create a");
        assert_eq!(mgr.session_count().await, 1);

        mgr.create_session("b", Some("/bin/bash"))
            .await
            .expect("create b");
        assert_eq!(mgr.session_count().await, 2);

        mgr.close_session("a").await.expect("close a");
        assert_eq!(mgr.session_count().await, 1);

        mgr.close_all().await;
        assert_eq!(mgr.session_count().await, 0);
    }

    // Test 19: ensure_default_session creates "main" when missing.
    #[tokio::test]
    async fn test_ensure_default_session_creates() {
        let mgr = SessionManager::new(test_config());
        assert_eq!(mgr.session_count().await, 0);

        let created = mgr.ensure_default_session().await.expect("ensure");
        assert!(created, "should report newly created");
        assert_eq!(mgr.session_count().await, 1);
        assert!(mgr.get_session("main").await.is_some());

        mgr.close_all().await;
    }

    // Test 20: ensure_default_session returns false when "main" already exists.
    #[tokio::test]
    async fn test_ensure_default_session_already_exists() {
        let mgr = SessionManager::new(test_config());
        mgr.create_session("main", Some(bash_path()))
            .await
            .expect("create");

        let created = mgr.ensure_default_session().await.expect("ensure");
        assert!(!created, "should report already existed");
        assert_eq!(mgr.session_count().await, 1);

        mgr.close_all().await;
    }
}
