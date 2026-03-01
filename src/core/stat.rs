/// File stat primitives.
///
/// Shared by narrate handler (success narration) and enrich module (failure diagnostics).
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Information gathered about source and destination *before* a command runs.
pub struct PreFlightInfo {
    pub source_size: Option<u64>,
    pub source_mtime: Option<SystemTime>,
    pub source_permissions: Option<u32>,
    pub dest_exists: bool,
    pub dest_size: Option<u64>,
    pub dest_mtime: Option<SystemTime>,
}

/// Information gathered about the destination *after* a command runs,
/// plus comparison results against pre-flight.
pub struct PostFlightInfo {
    pub dest_size: Option<u64>,
    pub dest_mtime: Option<SystemTime>,
    pub size_match: bool,
    pub file_count: Option<u64>,
    pub total_bytes: Option<u64>,
}

/// Summary of a directory tree: file count and total bytes.
pub struct TreeInfo {
    pub file_count: u64,
    pub total_bytes: u64,
}

/// Narrated result from a file operation handler.
pub struct NarratedResult {
    pub success: bool,
    pub message: String,
    pub exit_code: i32,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Format a byte count into a human-readable string.
///
/// - `< 1024` → `"{n} B"`
/// - `< 1 MB` → `"{n:.1} KB"`
/// - `< 1 GB` → `"{n:.1} MB"`
/// - `>= 1 GB` → `"{n:.1} GB"`
pub fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

/// Return a human-readable description of a path: `"name (size, perms)"` or `"name (not found)"`.
pub fn file_info(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());

    match std::fs::metadata(path) {
        Ok(meta) => {
            let size = human_size(meta.len());
            let perms = format_permissions(meta.permissions().mode());
            format!("{name} ({size}, {perms})")
        }
        Err(_) => format!("{name} (not found)"),
    }
}

/// Gather pre-flight stat information for source and destination paths.
pub fn gather_pre_flight(source: &Path, dest: &Path) -> PreFlightInfo {
    let source_meta = std::fs::metadata(source).ok();
    let dest_meta = std::fs::metadata(dest).ok();

    PreFlightInfo {
        source_size: source_meta.as_ref().map(|m| m.len()),
        source_mtime: source_meta
            .as_ref()
            .and_then(|m| m.modified().ok()),
        source_permissions: source_meta
            .as_ref()
            .map(|m| m.permissions().mode()),
        dest_exists: dest_meta.is_some(),
        dest_size: dest_meta.as_ref().map(|m| m.len()),
        dest_mtime: dest_meta.as_ref().and_then(|m| m.modified().ok()),
    }
}

/// Gather post-flight stat information for the destination, comparing with pre-flight.
pub fn gather_post_flight(dest: &Path, pre: &PreFlightInfo) -> PostFlightInfo {
    let dest_meta = std::fs::metadata(dest).ok();
    let dest_size = dest_meta.as_ref().map(|m| m.len());
    let dest_mtime = dest_meta.as_ref().and_then(|m| m.modified().ok());

    let size_match = match (pre.source_size, dest_size) {
        (Some(src), Some(dst)) => src == dst,
        _ => false,
    };

    // If dest is a directory, gather tree info
    let (file_count, total_bytes) = if dest_meta
        .as_ref()
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        let tree = gather_tree_info(dest);
        (Some(tree.file_count), Some(tree.total_bytes))
    } else {
        (None, None)
    };

    PostFlightInfo {
        dest_size,
        dest_mtime,
        size_match,
        file_count,
        total_bytes,
    }
}

/// Recursively count files and total bytes in a directory tree.
pub fn gather_tree_info(path: &Path) -> TreeInfo {
    let mut file_count: u64 = 0;
    let mut total_bytes: u64 = 0;

    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if let Ok(meta) = std::fs::metadata(&entry_path) {
                if meta.is_file() {
                    file_count += 1;
                    total_bytes += meta.len();
                } else if meta.is_dir() {
                    let sub = gather_tree_info(&entry_path);
                    file_count += sub.file_count;
                    total_bytes += sub.total_bytes;
                }
            }
        }
    }

    TreeInfo {
        file_count,
        total_bytes,
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Format a Unix permission mode as `rwxrwxrwx`.
fn format_permissions(mode: u32) -> String {
    let mut s = String::with_capacity(9);
    let flags = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    for (bit, ch) in flags {
        if mode & bit != 0 {
            s.push(ch);
        } else {
            s.push('-');
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    // Test 13: human_size formatting (B, KB, MB, GB)
    #[test]
    fn test_human_size_formatting() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
        assert_eq!(human_size(1024 * 1024 + 512 * 1024), "1.5 MB");
        assert_eq!(human_size(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(
            human_size(1024 * 1024 * 1024 + 512 * 1024 * 1024),
            "1.5 GB"
        );
    }

    // Test 14: file_info for nonexistent path
    #[test]
    fn test_file_info_nonexistent() {
        let result = file_info(Path::new("/tmp/this_file_definitely_does_not_exist_mish_test"));
        assert!(result.contains("not found"));
    }

    // Test 14b: file_info for existing file
    #[test]
    fn test_file_info_existing() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("hello.txt");
        fs::write(&file_path, "hello").unwrap();

        let result = file_info(&file_path);
        assert!(result.starts_with("hello.txt"));
        assert!(result.contains("5 B"));
        // Should contain some permission string like rw-r--r--
        assert!(result.contains("rw"));
    }

    // Test 15: gather_pre_flight for existing file
    #[test]
    fn test_gather_pre_flight_existing_file() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source.txt");
        let dest = dir.path().join("dest.txt");
        fs::write(&source, "source content").unwrap();

        let pre = gather_pre_flight(&source, &dest);

        // Source should have stats
        assert_eq!(pre.source_size, Some(14)); // "source content" = 14 bytes
        assert!(pre.source_mtime.is_some());
        assert!(pre.source_permissions.is_some());

        // Dest should not exist yet
        assert!(!pre.dest_exists);
        assert!(pre.dest_size.is_none());
        assert!(pre.dest_mtime.is_none());
    }

    // Test 15b: gather_pre_flight when dest already exists
    #[test]
    fn test_gather_pre_flight_dest_exists() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source.txt");
        let dest = dir.path().join("dest.txt");
        fs::write(&source, "source").unwrap();
        fs::write(&dest, "existing dest").unwrap();

        let pre = gather_pre_flight(&source, &dest);

        assert!(pre.dest_exists);
        assert_eq!(pre.dest_size, Some(13)); // "existing dest" = 13 bytes
        assert!(pre.dest_mtime.is_some());
    }

    // Test 16: gather_post_flight comparison
    #[test]
    fn test_gather_post_flight_comparison() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source.txt");
        let dest = dir.path().join("dest.txt");
        let content = "test content!!"; // 14 bytes
        fs::write(&source, content).unwrap();

        let pre = gather_pre_flight(&source, &dest);
        assert!(!pre.dest_exists);

        // Simulate a copy
        fs::copy(&source, &dest).unwrap();

        let post = gather_post_flight(&dest, &pre);
        assert_eq!(post.dest_size, Some(14));
        assert!(post.dest_mtime.is_some());
        assert!(post.size_match); // source and dest sizes should match
        // Not a directory, so file_count/total_bytes should be None
        assert!(post.file_count.is_none());
        assert!(post.total_bytes.is_none());
    }

    // Test 16b: gather_post_flight size mismatch
    #[test]
    fn test_gather_post_flight_size_mismatch() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source.txt");
        let dest = dir.path().join("dest.txt");
        fs::write(&source, "long content here").unwrap();

        let pre = gather_pre_flight(&source, &dest);

        // Write different content to dest (simulating partial/different copy)
        fs::write(&dest, "short").unwrap();

        let post = gather_post_flight(&dest, &pre);
        assert!(!post.size_match);
    }

    // Test 17: gather_tree_info
    #[test]
    fn test_gather_tree_info() {
        let dir = TempDir::new().unwrap();

        // Create some files
        let mut f1 = fs::File::create(dir.path().join("a.txt")).unwrap();
        f1.write_all(b"hello").unwrap(); // 5 bytes

        let mut f2 = fs::File::create(dir.path().join("b.txt")).unwrap();
        f2.write_all(b"world!").unwrap(); // 6 bytes

        // Create a subdirectory with a file
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let mut f3 = fs::File::create(sub.join("c.txt")).unwrap();
        f3.write_all(b"nested").unwrap(); // 6 bytes

        let tree = gather_tree_info(dir.path());
        assert_eq!(tree.file_count, 3);
        assert_eq!(tree.total_bytes, 17); // 5 + 6 + 6
    }

    // Test 17b: gather_tree_info on empty directory
    #[test]
    fn test_gather_tree_info_empty() {
        let dir = TempDir::new().unwrap();
        let tree = gather_tree_info(dir.path());
        assert_eq!(tree.file_count, 0);
        assert_eq!(tree.total_bytes, 0);
    }

    // Test: format_permissions
    #[test]
    fn test_format_permissions() {
        assert_eq!(format_permissions(0o755), "rwxr-xr-x");
        assert_eq!(format_permissions(0o644), "rw-r--r--");
        assert_eq!(format_permissions(0o700), "rwx------");
        assert_eq!(format_permissions(0o000), "---------");
        assert_eq!(format_permissions(0o777), "rwxrwxrwx");
    }
}
