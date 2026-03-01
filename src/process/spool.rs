use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Per-process circular buffer for raw output.
///
/// Stores the most recent `capacity` bytes of a process's output.
/// When the buffer is full, new writes overwrite the oldest data.
/// All operations are thread-safe via an internal `Mutex`.
#[derive(Debug)]
pub struct OutputSpool {
    inner: Mutex<SpoolInner>,
    capacity: usize,
}

#[derive(Debug)]
struct SpoolInner {
    buffer: Vec<u8>,
    write_pos: usize,
    total_written: usize,
    /// Current fill level (saturates at capacity).
    len: usize,
}

impl OutputSpool {
    /// Create a new spool with the given byte capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(SpoolInner {
                buffer: vec![0u8; capacity],
                write_pos: 0,
                total_written: 0,
                len: 0,
            }),
            capacity,
        }
    }

    /// Write bytes to the spool. Overwrites oldest data when full.
    pub fn write(&self, data: &[u8]) {
        let mut inner = self.inner.lock().unwrap();

        if data.len() >= self.capacity {
            // Data larger than or equal to the buffer — keep only the tail.
            let start = data.len() - self.capacity;
            inner.buffer[..self.capacity].copy_from_slice(&data[start..]);
            inner.write_pos = 0;
            inner.len = self.capacity;
            inner.total_written += data.len();
            return;
        }

        let cap = self.capacity;
        let mut src_offset = 0;
        let mut remaining = data.len();

        while remaining > 0 {
            let wp = inner.write_pos;
            let chunk = remaining.min(cap - wp);
            inner.buffer[wp..wp + chunk]
                .copy_from_slice(&data[src_offset..src_offset + chunk]);
            inner.write_pos = (wp + chunk) % cap;
            src_offset += chunk;
            remaining -= chunk;
        }

        inner.len = (inner.len + data.len()).min(cap);
        inner.total_written += data.len();
    }

    /// Read the last `max_bytes` bytes from the spool.
    ///
    /// Returns fewer bytes if the spool contains less data than requested.
    pub fn read_tail(&self, max_bytes: usize) -> Vec<u8> {
        let inner = self.inner.lock().unwrap();
        let n = max_bytes.min(inner.len);
        if n == 0 {
            return Vec::new();
        }

        let cap = self.capacity;
        // Start position: n bytes before write_pos (wrapped).
        let start = (inner.write_pos + cap - n) % cap;

        let mut result = Vec::with_capacity(n);
        if start + n <= cap {
            result.extend_from_slice(&inner.buffer[start..start + n]);
        } else {
            // Wraps around the end.
            result.extend_from_slice(&inner.buffer[start..cap]);
            let remainder = n - (cap - start);
            result.extend_from_slice(&inner.buffer[..remainder]);
        }
        result
    }

    /// Read all bytes currently in the spool, in chronological order.
    pub fn read_all(&self) -> Vec<u8> {
        let inner = self.inner.lock().unwrap();
        if inner.len == 0 {
            return Vec::new();
        }

        let cap = self.capacity;
        let n = inner.len;
        let start = (inner.write_pos + cap - n) % cap;

        let mut result = Vec::with_capacity(n);
        if start + n <= cap {
            result.extend_from_slice(&inner.buffer[start..start + n]);
        } else {
            result.extend_from_slice(&inner.buffer[start..cap]);
            let remainder = n - (cap - start);
            result.extend_from_slice(&inner.buffer[..remainder]);
        }
        result
    }

    /// Get total bytes written (including overwritten).
    pub fn total_written(&self) -> usize {
        self.inner.lock().unwrap().total_written
    }

    /// Get current buffer fill level.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().len == 0
    }
}

/// Errors from spool management operations.
#[derive(Debug)]
pub enum SpoolError {
    /// Aggregate memory limit would be exceeded.
    AggregateLimitExceeded { requested: usize, available: usize },
    /// The requested alias is already in use.
    AliasInUse(String),
}

impl std::fmt::Display for SpoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpoolError::AggregateLimitExceeded {
                requested,
                available,
            } => write!(
                f,
                "aggregate spool limit exceeded: requested {} bytes, {} available",
                requested, available
            ),
            SpoolError::AliasInUse(alias) => write!(f, "spool alias already in use: {}", alias),
        }
    }
}

impl std::error::Error for SpoolError {}

/// Aggregate spool manager to enforce total memory limits.
///
/// Tracks all per-process spools and ensures their combined capacity
/// does not exceed a configured ceiling.
pub struct SpoolManager {
    spools: HashMap<String, Arc<OutputSpool>>,
    total_capacity: usize,
    allocated: usize,
}

impl SpoolManager {
    /// Create a new manager with the given total capacity ceiling.
    pub fn new(total_capacity: usize) -> Self {
        Self {
            spools: HashMap::new(),
            total_capacity,
            allocated: 0,
        }
    }

    /// Create a spool for a process. Returns error if aggregate limit exceeded
    /// or the alias is already in use.
    pub fn create_spool(
        &mut self,
        alias: &str,
        capacity: usize,
    ) -> Result<Arc<OutputSpool>, SpoolError> {
        if self.spools.contains_key(alias) {
            return Err(SpoolError::AliasInUse(alias.to_string()));
        }

        let available = self.total_capacity - self.allocated;
        if capacity > available {
            return Err(SpoolError::AggregateLimitExceeded {
                requested: capacity,
                available,
            });
        }

        let spool = Arc::new(OutputSpool::new(capacity));
        self.spools.insert(alias.to_string(), Arc::clone(&spool));
        self.allocated += capacity;
        Ok(spool)
    }

    /// Remove a spool when process is dismissed. Frees the allocated capacity.
    pub fn remove_spool(&mut self, alias: &str) {
        if let Some(spool) = self.spools.remove(alias) {
            self.allocated -= spool.capacity;
        }
    }

    /// Get a spool by alias.
    pub fn get_spool(&self, alias: &str) -> Option<Arc<OutputSpool>> {
        self.spools.get(alias).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── OutputSpool basic ──────────────────────────────────────────

    #[test]
    fn write_and_read_small_data() {
        let spool = OutputSpool::new(1024);
        spool.write(b"hello world");
        let data = spool.read_all();
        assert_eq!(data, b"hello world");
    }

    #[test]
    fn write_more_than_capacity_overwrites_oldest() {
        let spool = OutputSpool::new(8);
        // Write 12 bytes into an 8-byte buffer.
        spool.write(b"abcdefghijkl");
        let data = spool.read_all();
        // Should keep only the last 8 bytes.
        assert_eq!(data, b"efghijkl");
    }

    #[test]
    fn incremental_writes_wrap_correctly() {
        let spool = OutputSpool::new(8);
        spool.write(b"abcdef"); // 6 bytes, write_pos=6
        spool.write(b"ghij"); // 4 more bytes, total 10, wraps around
        let data = spool.read_all();
        // Buffer has 8 bytes: the last 8 of "abcdefghij" = "cdefghij"
        assert_eq!(data, b"cdefghij");
    }

    #[test]
    fn read_tail_returns_last_n_bytes() {
        let spool = OutputSpool::new(1024);
        spool.write(b"0123456789");
        let tail = spool.read_tail(4);
        assert_eq!(tail, b"6789");
    }

    #[test]
    fn read_tail_after_wrap() {
        let spool = OutputSpool::new(8);
        spool.write(b"abcdefghij"); // wraps, buffer has "cdefghij"
        let tail = spool.read_tail(3);
        assert_eq!(tail, b"hij");
    }

    #[test]
    fn read_tail_more_than_available() {
        let spool = OutputSpool::new(1024);
        spool.write(b"abc");
        let tail = spool.read_tail(100);
        assert_eq!(tail, b"abc");
    }

    #[test]
    fn read_all_returns_chronological_order_after_wrap() {
        let spool = OutputSpool::new(4);
        spool.write(b"ab");
        spool.write(b"cd");
        spool.write(b"ef"); // wraps: buffer is "efcd" with write_pos=2, len=4
        let data = spool.read_all();
        assert_eq!(data, b"cdef");
    }

    // ── Concurrency ────────────────────────────────────────────────

    #[test]
    fn concurrent_write_and_read_no_panic() {
        use std::thread;

        let spool = Arc::new(OutputSpool::new(256));
        let mut handles = vec![];

        // Spawn writers.
        for i in 0..4 {
            let s = Arc::clone(&spool);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    let data = format!("writer-{}-data\n", i);
                    s.write(data.as_bytes());
                }
            }));
        }

        // Spawn readers.
        for _ in 0..4 {
            let s = Arc::clone(&spool);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    let _ = s.read_tail(64);
                    let _ = s.read_all();
                }
            }));
        }

        for h in handles {
            h.join().expect("thread panicked");
        }

        // If we get here, no panics occurred.
    }

    // ── total_written / len / is_empty ─────────────────────────────

    #[test]
    fn total_written_tracks_cumulative_bytes() {
        let spool = OutputSpool::new(8);
        spool.write(b"abcdef"); // 6
        spool.write(b"ghij"); // 4 more = 10 total, but buffer only holds 8
        assert_eq!(spool.total_written(), 10);
    }

    #[test]
    fn len_and_is_empty_after_operations() {
        let spool = OutputSpool::new(16);
        assert!(spool.is_empty());
        assert_eq!(spool.len(), 0);

        spool.write(b"hello");
        assert!(!spool.is_empty());
        assert_eq!(spool.len(), 5);

        // Fill and overflow.
        spool.write(b"0123456789ab"); // 12 more, total 17, cap=16
        assert_eq!(spool.len(), 16);
    }

    #[test]
    fn len_saturates_at_capacity() {
        let spool = OutputSpool::new(4);
        spool.write(b"abcdef");
        assert_eq!(spool.len(), 4);
        spool.write(b"gh");
        assert_eq!(spool.len(), 4);
    }

    // ── SpoolManager ───────────────────────────────────────────────

    #[test]
    fn manager_creates_and_gets_spool() {
        let mut mgr = SpoolManager::new(1024);
        let spool = mgr.create_spool("proc1", 256).unwrap();
        spool.write(b"data");

        let fetched = mgr.get_spool("proc1").unwrap();
        assert_eq!(fetched.read_all(), b"data");
    }

    #[test]
    fn manager_enforces_aggregate_limit() {
        let mut mgr = SpoolManager::new(512);
        mgr.create_spool("a", 256).unwrap();
        mgr.create_spool("b", 200).unwrap();
        // Only 56 bytes left, requesting 100 should fail.
        let result = mgr.create_spool("c", 100);
        assert!(result.is_err());
        match result.unwrap_err() {
            SpoolError::AggregateLimitExceeded {
                requested,
                available,
            } => {
                assert_eq!(requested, 100);
                assert_eq!(available, 56);
            }
            other => panic!("expected AggregateLimitExceeded, got {:?}", other),
        }
    }

    #[test]
    fn manager_refuses_duplicate_alias() {
        let mut mgr = SpoolManager::new(1024);
        mgr.create_spool("dup", 128).unwrap();
        let result = mgr.create_spool("dup", 128);
        assert!(result.is_err());
        match result.unwrap_err() {
            SpoolError::AliasInUse(alias) => assert_eq!(alias, "dup"),
            other => panic!("expected AliasInUse, got {:?}", other),
        }
    }

    #[test]
    fn manager_remove_frees_capacity() {
        let mut mgr = SpoolManager::new(512);
        mgr.create_spool("a", 300).unwrap();
        mgr.create_spool("b", 200).unwrap();
        // 12 bytes free — can't allocate 100.
        assert!(mgr.create_spool("c", 100).is_err());
        // Remove "a", freeing 300 bytes (now 312 free).
        mgr.remove_spool("a");
        assert!(mgr.get_spool("a").is_none());
        // Now 100 should fit.
        mgr.create_spool("c", 100).unwrap();
    }

    #[test]
    fn manager_remove_nonexistent_is_noop() {
        let mut mgr = SpoolManager::new(1024);
        mgr.remove_spool("does_not_exist"); // should not panic
    }

    #[test]
    fn empty_read_tail_returns_empty() {
        let spool = OutputSpool::new(64);
        assert!(spool.read_tail(10).is_empty());
    }

    #[test]
    fn empty_read_all_returns_empty() {
        let spool = OutputSpool::new(64);
        assert!(spool.read_all().is_empty());
    }

    #[test]
    fn single_large_write_equals_capacity() {
        let spool = OutputSpool::new(4);
        spool.write(b"abcd");
        assert_eq!(spool.read_all(), b"abcd");
        assert_eq!(spool.len(), 4);
    }
}
