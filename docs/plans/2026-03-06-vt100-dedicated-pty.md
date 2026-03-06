# Virtual Terminal Screen Buffer for Dedicated PTY

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the append-only spool with a vt100 virtual terminal screen buffer for dedicated PTY processes, so `read_tail` returns the current screen state instead of accumulated TUI frame redraws.

**Architecture:** Add `vt100::Parser` to `DedicatedPtyProcess`. Raw PTY bytes feed into the parser instead of being VTE-stripped and appended to spool. `read_tail` reads the parser's screen buffer (`screen().contents()`), returning clean text representing what's currently displayed. The spool is kept but receives only extracted content lines (non-empty rows), not raw bytes.

**Tech Stack:** `vt100` crate v0.16.x (pure Rust terminal emulator, MIT, 4.3M downloads)

---

### Task 1: Add vt100 dependency

**Files:**
- Modify: `Cargo.toml`

**Step 1: Add the dependency**

In `Cargo.toml`, add to `[dependencies]`:
```toml
vt100 = "0.16"
```

**Step 2: Verify it compiles**

Run: `cargo build 2>&1`
Expected: compiles cleanly

**Step 3: Commit**

```
git add Cargo.toml Cargo.lock
git commit -m "chore: add vt100 dependency for virtual terminal screen buffer"
```

---

### Task 2: Add vt100 parser to DedicatedPtyProcess

**Files:**
- Modify: `src/interpreter/dedicated.rs`

**Step 1: Add parser field and update constructor**

Replace the spool-only approach with a vt100 parser. The parser is the primary state — spool receives extracted content.

```rust
use crate::core::pty::PtyCapture;
use crate::process::spool::OutputSpool;

use std::sync::{Arc, Mutex};

pub struct DedicatedPtyProcess {
    pty: Arc<Mutex<PtyCapture>>,
    parser: Arc<Mutex<vt100::Parser>>,
    spool: Arc<OutputSpool>,
}

impl DedicatedPtyProcess {
    pub fn new(pty: PtyCapture, spool: Arc<OutputSpool>) -> Self {
        Self {
            pty: Arc::new(Mutex::new(pty)),
            parser: Arc::new(Mutex::new(vt100::Parser::new(24, 80, 1000))),
            spool,
        }
    }
```

`Parser::new(24, 80, 1000)` — 24 rows, 80 cols, 1000 lines of scrollback so we capture conversation history, not just the visible screen.

**Step 2: Update `drain_to_spool` to feed the parser**

Instead of VTE-stripping and writing to spool, feed raw bytes into the vt100 parser. Then extract screen contents and write to spool (so `read_full` and the spool size accounting still work).

```rust
    pub async fn drain_to_spool(&self) -> Result<(), String> {
        let pty = self.pty.clone();
        let parser = self.parser.clone();

        let drained = tokio::task::spawn_blocking(move || {
            let pty = pty.lock().map_err(|e| format!("lock poisoned: {e}"))?;
            let mut accumulated = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match pty.read_output(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => accumulated.extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }

            if !accumulated.is_empty() {
                let mut parser = parser.lock().map_err(|e| format!("lock poisoned: {e}"))?;
                parser.process(&accumulated);
            }

            Ok::<Vec<u8>, String>(accumulated)
        })
        .await
        .map_err(|e| format!("spawn_blocking join error: {e}"))??;

        // Write screen contents to spool (for read_full compatibility and size accounting)
        if !drained.is_empty() {
            let parser = self.parser.lock().map_err(|e| format!("lock poisoned: {e}"))?;
            let contents = parser.screen().contents();
            if !contents.is_empty() {
                self.spool.clear_and_write(contents.as_bytes());
            }
        }

        Ok(())
    }
```

**Step 3: Add `read_screen` method**

New method that returns the current screen contents from the vt100 parser:

```rust
    /// Read current screen contents from the virtual terminal.
    /// Returns clean text with TUI chrome resolved — cursor movement,
    /// partial redraws, etc. are all handled by the vt100 parser.
    pub fn read_screen(&self) -> Result<String, String> {
        let parser = self.parser.lock().map_err(|e| format!("lock poisoned: {e}"))?;
        // contents() returns rows × cols text with trailing whitespace trimmed
        Ok(parser.screen().contents())
    }

    /// Read screen contents including scrollback buffer.
    pub fn read_screen_full(&self) -> Result<String, String> {
        let parser = self.parser.lock().map_err(|e| format!("lock poisoned: {e}"))?;
        let screen = parser.screen();
        // scrollback_contents() includes scrolled-off lines + visible screen
        let mut full = screen.scrollback_contents();
        full.push_str(&screen.contents());
        Ok(full)
    }
```

**Step 4: Verify it compiles**

Run: `cargo build 2>&1`

**Step 5: Commit**

```
git add src/interpreter/dedicated.rs
git commit -m "feat: add vt100 screen buffer to DedicatedPtyProcess"
```

---

### Task 3: Add `clear_and_write` to OutputSpool

**Files:**
- Modify: `src/process/spool.rs`

The spool needs a method to replace its contents (not append) since the screen buffer is a snapshot, not a stream.

**Step 1: Check spool API**

Read `src/process/spool.rs` to find the `OutputSpool` struct and its methods.

**Step 2: Add `clear_and_write` method**

```rust
    /// Replace all spool contents with new data.
    /// Used by dedicated PTY processes where the screen buffer
    /// is a snapshot, not an append stream.
    pub fn clear_and_write(&self, data: &[u8]) {
        let mut buf = self.buffer.lock().unwrap();
        buf.clear();
        buf.extend_from_slice(data);
    }
```

**Step 3: Verify it compiles**

Run: `cargo build 2>&1`

**Step 4: Commit**

```
git add src/process/spool.rs
git commit -m "feat: add clear_and_write to OutputSpool for screen buffer snapshots"
```

---

### Task 4: Expose `read_screen` through ManagedProcess

**Files:**
- Modify: `src/interpreter/managed_process.rs`

**Step 1: Add delegating methods**

```rust
    /// Read current screen contents (dedicated PTY only).
    /// Returns None for interpreter processes (they use spool).
    pub fn read_screen(&self) -> Option<Result<String, String>> {
        match self {
            ManagedProcess::Dedicated(d) => Some(d.read_screen()),
            ManagedProcess::Interpreter(_) => None,
        }
    }

    /// Read full screen contents including scrollback (dedicated PTY only).
    pub fn read_screen_full(&self) -> Option<Result<String, String>> {
        match self {
            ManagedProcess::Dedicated(d) => Some(d.read_screen_full()),
            ManagedProcess::Interpreter(_) => None,
        }
    }
```

**Step 2: Verify it compiles**

Run: `cargo build 2>&1`

**Step 3: Commit**

```
git add src/interpreter/managed_process.rs
git commit -m "feat: expose read_screen through ManagedProcess enum"
```

---

### Task 5: Wire `read_tail` to use screen buffer for dedicated PTYs

**Files:**
- Modify: `src/tools/sh_interact.rs`

**Step 1: Update `handle_read_tail`**

In `handle_read_tail`, after getting the entry, check if the managed process supports screen reading. If so, drain first, then read screen contents instead of spool.

Find the block (around line 89-112):
```rust
    // Drain interpreter PTY output to spool before reading
    if let Some(ref interpreter) = entry.interpreter {
        interpreter.drain_to_spool().await.ok();
    }

    let lines_requested = params.lines.unwrap_or(50);

    // Read raw bytes from spool, strip ANSI, then extract last N lines.
    let raw = entry.spool.read_all();
```

Replace with:
```rust
    // Drain PTY output before reading
    if let Some(ref managed) = entry.interpreter {
        managed.drain_to_spool().await.ok();
    }

    let lines_requested = params.lines.unwrap_or(50);

    // For dedicated PTY processes, read from virtual terminal screen buffer.
    // For everything else, read from spool with VTE stripping.
    let stripped = if let Some(ref managed) = entry.interpreter {
        if let Some(Ok(screen)) = managed.read_screen() {
            screen
        } else {
            let raw = entry.spool.read_all();
            let text = String::from_utf8_lossy(&raw);
            vte_strip::strip_ansi(&text)
        }
    } else {
        let raw = entry.spool.read_all();
        let text = String::from_utf8_lossy(&raw);
        vte_strip::strip_ansi(&text)
    };

    let all_lines: Vec<&str> = stripped.lines().collect();
```

Then remove the old `stripped` variable and the duplicate spool read that follows.

**Step 2: Update `handle_read_full` similarly**

Same pattern but use `read_screen_full()` for the dedicated PTY path.

**Step 3: Verify it compiles**

Run: `cargo build 2>&1`

**Step 4: Run tests**

Run: `make test 2>&1`
Expected: all existing tests pass unchanged

**Step 5: Commit**

```
git add src/tools/sh_interact.rs
git commit -m "feat: read_tail uses vt100 screen buffer for dedicated PTY processes"
```

---

### Task 6: Tests

**Files:**
- Modify: `src/interpreter/dedicated.rs` (add tests)

**Step 1: Test that parser processes output correctly**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::spool::SpoolManager;

    #[test]
    fn dedicated_pty_process_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Arc<DedicatedPtyProcess>>();
    }

    #[test]
    fn read_screen_returns_parser_contents() {
        // Directly test the parser integration without a real PTY
        let parser = vt100::Parser::new(24, 80, 0);
        let parser = Arc::new(Mutex::new(parser));

        // Feed some data
        {
            let mut p = parser.lock().unwrap();
            p.process(b"hello world\r\nline two");
        }

        let p = parser.lock().unwrap();
        let contents = p.screen().contents();
        assert!(contents.contains("hello world"), "contents: {contents}");
        assert!(contents.contains("line two"), "contents: {contents}");
    }

    #[test]
    fn parser_handles_cursor_movement() {
        let mut parser = vt100::Parser::new(24, 80, 0);

        // Write "ABCDE", move cursor back 3, overwrite with "XY"
        // Result should be "ABXYE" (not "ABCDEXY")
        parser.process(b"ABCDE\x1b[3DXY");

        let contents = parser.screen().contents();
        let first_line = contents.lines().next().unwrap_or("");
        assert_eq!(first_line.trim(), "ABXYE", "cursor overwrite: {contents}");
    }

    #[test]
    fn parser_handles_screen_clear() {
        let mut parser = vt100::Parser::new(24, 80, 0);

        parser.process(b"old content");
        parser.process(b"\x1b[2J\x1b[H");  // clear screen + home
        parser.process(b"new content");

        let contents = parser.screen().contents();
        assert!(!contents.contains("old content"), "should be cleared: {contents}");
        assert!(contents.contains("new content"), "should have new: {contents}");
    }
}
```

**Step 2: Run tests**

Run: `make test 2>&1`
Expected: all tests pass including new ones

**Step 3: Commit**

```
git add src/interpreter/dedicated.rs
git commit -m "test: vt100 parser integration tests for DedicatedPtyProcess"
```

---

### Task 7: Version bump and final verification

**Files:**
- Modify: `Cargo.toml`

**Step 1: Bump version**

```toml
version = "0.4.12"
```

**Step 2: Run full test suite**

Run: `make test 2>&1`
Expected: all tests pass

**Step 3: Manual smoke test**

```
sh_spawn(alias="test", cmd="python3 -c \"import time; print('ready'); time.sleep(60)\"", dedicated_pty=true, wait_for="ready")
sh_interact(alias="test", action="read_tail")
sh_interact(alias="test", action="kill")
```

Verify `read_tail` returns clean screen contents, not accumulated redraws.

**Step 4: Commit and tag**

```
git add -A
git commit -m "feat: vt100 virtual terminal for dedicated PTY, bump to v0.4.12"
make release
```

---

## Files Changed Summary

| File | Change |
|------|--------|
| `Cargo.toml` | Add `vt100 = "0.16"`, version bump |
| `src/interpreter/dedicated.rs` | Add `vt100::Parser`, `read_screen`, update `drain_to_spool` |
| `src/interpreter/managed_process.rs` | Delegate `read_screen` / `read_screen_full` |
| `src/process/spool.rs` | Add `clear_and_write` method |
| `src/tools/sh_interact.rs` | `read_tail` / `read_full` use screen buffer for dedicated PTY |

## Future: Chrome Stripping Grammar

Once the screen buffer is working, a follow-up grammar can strip TUI chrome (borders, status bars) from specific apps. This is a separate concern — the vt100 buffer gives us clean text, the grammar filters out known UI patterns. Not in scope for this plan.
