# PTY Test Stabilization Design

**Date:** 2026-03-01
**Problem:** ~70 tests spawn real PTYs. Under parallel cargo test threads, kernel PTY pool / FD pressure causes `InitTimeout` hangs. One deterministic failure is hidden in the noise.
**Goal:** Eliminate hangs while maintaining comprehensive coverage. Speed improvement is secondary.

## Root Cause

`cargo test` runs all `#[test]` and `#[tokio::test]` in parallel threads. When 70+ tests each call `PtyCapture::spawn` or `SessionManager::create_session`, the system hits:
- Kernel PTY device limits (`/dev/ptmx` allocation)
- FD exhaustion (each PTY = 2 FDs minimum)
- Shell init timeout (`INIT_TIMEOUT = 10s`) exceeded under load

The `InitTimeout` error in `ShellProcess::initialize` is the symptom, not the bug.

## Design

### Part 1: Serial Gate (`#[serial(pty)]`)

All PTY-spawning tests get `#[serial(pty)]` from the `serial_test` crate (already a dependency). This serializes PTY tests against each other while allowing non-PTY tests to run in full parallel.

Named group `pty` ensures these don't block unrelated serial tests (e.g., `shutdown.rs` uses default `#[serial]`).

**Files to annotate:**

| File | PTY Tests | Annotation |
|------|-----------|------------|
| `src/core/pty.rs` | 10 | `#[serial(pty)]` on each |
| `src/session/shell.rs` | 11 | `#[serial(pty)]` on each |
| `src/handlers/condense.rs` | 8 | `#[serial(pty)]` on each |
| `src/tools/sh_session.rs` | 6 (PTY ones only) | `#[serial(pty)]` on each |
| `src/tools/sh_spawn.rs` | 8 (integration only) | `#[serial(pty)]` on each |
| `src/mcp/server.rs` | any with SessionManager | `#[serial(pty)]` on each |
| `tests/cli_integration.rs` | 35 | `#[serial(pty)]` on each |

Non-PTY tests (param validation, JSON parsing, grammar matching, error code checks) are **not** annotated and continue running in parallel.

### Part 2: Session Sharing in sh_spawn.rs

**Current:** 8 integration tests each create SessionManager + session (8 PTY spawns).

**After:** One shared `SessionManager` with pre-initialized "main" session, reused across tests that only need to run commands.

```rust
async fn shared_spawn_session() -> (SessionManager, MishConfig) {
    let config = test_config();
    let mgr = SessionManager::new(Arc::new(MishConfig::default()));
    mgr.create_session("main", Some("/bin/bash")).await.unwrap();
    (mgr, config)
}
```

**Sharing plan:**
- `spawn_basic_command`, `spawn_wait_for_matching`, `spawn_wait_for_timeout` — share session, unique aliases
- `spawn_alias_conflict_error` — share session, fresh ProcessTable
- `spawn_empty_alias_rejected`, `spawn_invalid_wait_for_regex` — share session (validation happens before session use)
- `spawn_deny_list_blocks_*` — share session (deny-list check happens before session use)
- `spawn_session_not_found` — NO session (tests missing session error)

**Reduction:** 8 sessions -> 1.

### Part 3: Session Sharing in sh_session.rs

**Current:** 6 PTY-spawning tests each create their own session.

**After:** Group into:
1. **Read-only tests** (list, create, duplicate checks) — share one pre-built session
2. **Destructive tests** (close) — get their own session

**Reduction:** 6 sessions -> 2.

### Part 4: cli_integration.rs Serialization

These 35 tests each spawn the full `mish` binary via `assert_cmd`. They can't share sessions across process boundaries, but `#[serial(pty)]` ensures they don't stampede the PTY pool.

Expected wall clock: ~18s (35 tests x ~0.5s each).

## What Changes, What Doesn't

**Changes:**
- PTY tests run serially within their group
- sh_spawn and sh_session share sessions where safe
- Total PTY spawns per suite: ~70 -> ~50 (session sharing) running serially

**Doesn't change:**
- ~120+ non-PTY tests still run in full parallel
- Test coverage — same assertions, same error paths
- No mocking introduced — these are real integration tests
- pty.rs, shell.rs, condense.rs tests keep per-test isolation (they need it)

## Expected Outcome

- **Hangs eliminated:** Serial execution prevents PTY resource contention
- **Deterministic failures exposed:** Without noise from timeouts, the real bug becomes visible
- **Wall clock:** PTY tests ~35s serial + non-PTY tests ~2s parallel = ~37s total (vs current: hangs indefinitely)
