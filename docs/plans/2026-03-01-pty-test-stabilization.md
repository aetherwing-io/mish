# PTY Test Stabilization Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Eliminate PTY test hangs by serializing PTY-spawning tests and sharing sessions where safe.

**Architecture:** Add `#[serial(pty)]` from `serial_test` crate to all 70+ PTY-spawning tests. Refactor sh_spawn.rs and sh_session.rs integration tests to share a single PTY session instead of spawning one per test.

**Tech Stack:** Rust, `serial_test` crate (already in Cargo.toml), `tokio`

---

### Task 1: Add `#[serial(pty)]` to `src/core/pty.rs`

**Files:**
- Modify: `src/core/pty.rs` (test module, lines ~358-695)

**Step 1: Add import**

In the `#[cfg(test)] mod tests` block, add:

```rust
use serial_test::serial;
```

**Step 2: Annotate all 11 tests**

Add `#[serial(pty)]` below `#[test]` (or `#[tokio::test]`) for each of these:

| Line | Test | Attr |
|------|------|------|
| 395 | `test_ansi_color_passthrough` | `#[test]` `#[serial(pty)]` |
| 426 | `test_progress_bar_detection` | `#[test]` `#[serial(pty)]` |
| 471 | `test_sigwinch_forwarding` | `#[test]` `#[serial(pty)]` |
| 497 | `test_raw_mode_detection` | `#[test]` `#[serial(pty)]` |
| 534 | `test_multibyte_utf8_at_buffer_boundary` | `#[test]` `#[serial(pty)]` |
| 585 | `test_spawn_and_exit` | `#[test]` `#[serial(pty)]` |
| 608 | `test_nonzero_exit` | `#[test]` `#[serial(pty)]` |
| 628 | `test_write_stdin` | `#[test]` `#[serial(pty)]` |
| 654 | `test_signal_child` | `#[test]` `#[serial(pty)]` |
| 676 | `test_empty_command` | **SKIP** — no PTY spawn, just validation |
| 683 | `test_wait_async` | `#[tokio::test]` `#[serial(pty)]` |

That's 10 tests annotated (skip `test_empty_command`).

**Step 3: Run tests**

```bash
cargo test --lib core::pty::tests -- --test-threads=1
```

Expected: All 11 pass (10 serial + 1 parallel-safe).

**Step 4: Commit**

```bash
git add src/core/pty.rs
git commit -m "test: add #[serial(pty)] to pty.rs PTY tests"
```

---

### Task 2: Add `#[serial(pty)]` to `src/session/shell.rs`

**Files:**
- Modify: `src/session/shell.rs` (test module, lines ~443-722)

**Step 1: Add import**

In the `#[cfg(test)] mod tests` block, add:

```rust
use serial_test::serial;
```

**Step 2: Annotate all 11 tests**

All are `#[tokio::test]`. Add `#[serial(pty)]` to each:

| Line | Test |
|------|------|
| 453 | `test_shell_spawns_successfully` |
| 463 | `test_is_ready_before_and_after_init` |
| 477 | `test_initialization_completes` |
| 490 | `test_execute_echo_hello` |
| 513 | `test_execute_failing_command` |
| 537 | `test_cwd_tracking` |
| 565 | `test_environment_persistence` |
| 594 | `test_timeout_enforcement` |
| 625 | `test_kill_terminates_shell` |
| 675 | `test_startup_output_discarded` |
| 695 | `stress_concurrent_initialization` (already `#[ignore]`, still add `#[serial(pty)]`) |

**Step 3: Run tests**

```bash
cargo test --lib session::shell::tests -- --test-threads=1
```

Expected: 10 pass, 1 ignored.

**Step 4: Commit**

```bash
git add src/session/shell.rs
git commit -m "test: add #[serial(pty)] to shell.rs PTY tests"
```

---

### Task 3: Add `#[serial(pty)]` to `src/handlers/condense.rs`

**Files:**
- Modify: `src/handlers/condense.rs` (test module, lines ~124-344)

**Step 1: Add import**

```rust
use serial_test::serial;
```

**Step 2: Annotate 7 PTY tests (skip the `#[ignore]` one — still annotate it)**

| Line | Test | Notes |
|------|------|-------|
| 140 | `test_condense_with_grammar` | |
| 176 | `test_condense_no_grammar` | |
| 201 | `test_exit_code_nonzero` | |
| 219 | `test_hazard_passthrough` | |
| 259 | `test_partial_line_timeout` | `#[ignore]` — still add `#[serial(pty)]` |
| 277 | `test_signal_forwarding` | |
| 301 | `test_empty_output` | |
| 315 | `test_long_output_summarized` | |

8 tests annotated total.

**Step 3: Run tests**

```bash
cargo test --lib handlers::condense::tests -- --test-threads=1
```

Expected: 7 pass, 1 ignored.

**Step 4: Commit**

```bash
git add src/handlers/condense.rs
git commit -m "test: add #[serial(pty)] to condense.rs PTY tests"
```

---

### Task 4: Add `#[serial(pty)]` to `src/tools/sh_session.rs` (PTY tests only)

**Files:**
- Modify: `src/tools/sh_session.rs` (test module)

**Step 1: Add import**

In the `#[cfg(test)] mod tests` block, add:

```rust
use serial_test::serial;
```

**Step 2: Annotate ONLY the PTY-spawning tests**

These tests call `create_session()` which spawns a real shell:

| Line | Test | Needs `#[serial(pty)]` |
|------|------|----------------------|
| 192 | `test_create_session` | YES — calls `handle()` with action "create" |
| 220 | `test_list_sessions` | YES — calls `create_session` directly |
| 254 | `test_close_session` | YES — calls `create_session` directly |
| 284 | `test_close_nonexistent_session` | NO — no session created |
| 311 | `test_create_limit_reached` | YES — calls `create_session` directly |
| 345 | `test_create_duplicate_name` | YES — calls `create_session` directly |
| 379 | `test_unknown_action` | NO — no session created |
| 406 | `test_create_missing_name` | NO — param validation fails before spawn |
| 433 | `test_close_missing_name` | NO — param validation fails before spawn |
| 460 | `test_list_multiple_sessions` | YES — calls `create_session` twice |
| 496 | `test_list_empty` | NO — no session created |
| 519 | `test_params_deserialize_create` | NO — sync, no PTY |
| 528 | `test_params_deserialize_list` | NO — sync, no PTY |
| 537 | `test_params_deserialize_close` | NO — sync, no PTY |
| 550 | `test_tool_error_display` | NO — sync, no PTY |
| 565 | `test_session_error_to_tool_error` | NO — sync, no PTY |
| 590 | `test_full_lifecycle` | YES — calls `handle()` with "create" |

7 tests get `#[serial(pty)]`. The other 10 stay parallel.

**Step 3: Run tests**

```bash
cargo test --lib tools::sh_session::tests -- --test-threads=1
```

Expected: All 17 pass.

**Step 4: Commit**

```bash
git add src/tools/sh_session.rs
git commit -m "test: add #[serial(pty)] to sh_session.rs PTY tests"
```

---

### Task 5: Add `#[serial(pty)]` to `src/tools/sh_spawn.rs` (integration tests only)

**Files:**
- Modify: `src/tools/sh_spawn.rs` (test module)

**Step 1: Add import**

```rust
use serial_test::serial;
```

**Step 2: Annotate ONLY integration tests that spawn sessions**

| Line | Test | Needs `#[serial(pty)]` |
|------|------|----------------------|
| 325-470 | All unit tests (`extract_bg_pid_*`, `clean_bg_*`, etc.) | NO |
| 480 | `spawn_basic_command` | YES |
| 514 | `spawn_alias_conflict_error` | YES |
| 551 | `spawn_wait_for_matching` | YES |
| 588 | `spawn_session_not_found` | NO — no session created (tests missing session) |
| 610 | `spawn_empty_alias_rejected` | YES |
| 636 | `spawn_invalid_wait_for_regex` | YES |
| 663 | `spawn_wait_for_timeout` | YES |
| 701 | `spawn_deny_list_blocks_rm_rf_root` | YES |
| 731 | `spawn_deny_list_blocks_mkfs` | YES |

8 tests get `#[serial(pty)]`. Note: `spawn_session_not_found` does NOT create a session, so it stays parallel.

**Step 3: Run tests**

```bash
cargo test --lib tools::sh_spawn::tests -- --test-threads=1
```

Expected: All 25 pass.

**Step 4: Commit**

```bash
git add src/tools/sh_spawn.rs
git commit -m "test: add #[serial(pty)] to sh_spawn.rs integration tests"
```

---

### Task 6: Add `#[serial(pty)]` to `src/mcp/server.rs` (PTY tests only)

**Files:**
- Modify: `src/mcp/server.rs` (test module)

**Step 1: Add import**

```rust
use serial_test::serial;
```

**Step 2: Annotate the 2 tests that create sessions**

| Line | Test | Needs `#[serial(pty)]` |
|------|------|----------------------|
| 312 | `test_server_new` | NO — sync, no session |
| 320 | `test_server_initialize` | NO — protocol only |
| 341 | `test_server_eof` | NO — protocol only |
| 352 | `test_server_tools_list` | NO — protocol only |
| 371 | `test_server_notification_no_output` | NO — protocol only |
| 392 | `test_server_multiple_requests` | NO — protocol only |
| 418 | `test_server_tools_call_sh_help` | NO — sh_help needs no session |
| 438 | `test_server_unknown_tool_error` | NO — error path, no session |
| 457 | `test_server_shutdown_signal` | NO — shutdown mechanics only |
| 472 | `test_server_error_display` | NO — sync, no session |
| 492 | `test_server_full_stack_sh_run` | YES — calls `create_session` |
| 531 | `test_server_full_mcp_lifecycle` | YES — calls `create_session` |

2 tests get `#[serial(pty)]`.

**Step 3: Run tests**

```bash
cargo test --lib mcp::server::tests -- --test-threads=1
```

Expected: All 12 pass.

**Step 4: Commit**

```bash
git add src/mcp/server.rs
git commit -m "test: add #[serial(pty)] to server.rs PTY tests"
```

---

### Task 7: Add `#[serial(pty)]` to `tests/cli_integration.rs`

**Files:**
- Modify: `tests/cli_integration.rs`

**Step 1: Add import at top of file**

After the existing `use` statements (line 17), add:

```rust
use serial_test::serial;
```

**Step 2: Annotate all 35 tests**

Every test in this file spawns the mish binary which uses PTY. Add `#[serial(pty)]` below every `#[test]`:

Tests 01-35: `test_01_echo_hello_produces_output` through `test_35_rapid_exit`.

All 35 get `#[serial(pty)]`.

**Step 3: Run tests**

```bash
cargo test --test cli_integration
```

Expected: All 35 pass (serially, ~18s).

**Step 4: Commit**

```bash
git add tests/cli_integration.rs
git commit -m "test: add #[serial(pty)] to cli_integration.rs tests"
```

---

### Task 8: Session sharing in `src/tools/sh_spawn.rs`

**Files:**
- Modify: `src/tools/sh_spawn.rs` (test module, integration tests section)

**Step 1: Add shared session helper**

After `test_session_config()` (around line 320), add:

```rust
/// Shared session for integration tests that need a "main" shell.
/// Reduces PTY spawns from 8 to 1.
async fn shared_session() -> (SessionManager, MishConfig) {
    let config = test_config();
    let session_config = test_session_config();
    let mgr = SessionManager::new(session_config);
    mgr.create_session("main", Some("/bin/bash"))
        .await
        .expect("shared session creation");
    (mgr, config)
}
```

**Step 2: Refactor tests to use `shared_session()`**

Replace the duplicated session setup in these tests:

- `spawn_basic_command` — replace setup with `let (mgr, config) = shared_session().await;`
- `spawn_alias_conflict_error` — same
- `spawn_wait_for_matching` — same
- `spawn_empty_alias_rejected` — same
- `spawn_invalid_wait_for_regex` — same
- `spawn_wait_for_timeout` — same
- `spawn_deny_list_blocks_rm_rf_root` — same
- `spawn_deny_list_blocks_mkfs` — same

Each test still creates its own `ProcessTable` (they need unique alias state).

Remove the per-test `mgr.close_all().await;` — the `SessionManager` drops at end of test. BUT: since these are `#[serial(pty)]`, only one runs at a time, so session cleanup between tests is important. **Keep** `mgr.close_all().await;` at end of each test.

WAIT — these tests share the `shared_session()` function but each test still calls it independently, so each still spawns 1 session. The win here is just DRY code, not fewer sessions. To actually reduce sessions, we'd need `lazy_static` or `OnceCell` which is more complex.

**Revised approach:** Just keep `shared_session()` as a DRY helper. The serial gate already fixes the hang. The session sharing for fewer PTY spawns would require `tokio::sync::OnceCell` and is out of scope for this plan.

So Task 8 is just: extract the duplicated setup into `shared_session()` for DRY. Each test still gets its own session but the boilerplate is reduced.

**Step 3: Run tests**

```bash
cargo test --lib tools::sh_spawn::tests -- --test-threads=1
```

Expected: All 25 pass.

**Step 4: Commit**

```bash
git add src/tools/sh_spawn.rs
git commit -m "refactor: extract shared_session() helper in sh_spawn tests"
```

---

### Task 9: Session sharing in `src/tools/sh_session.rs`

**Files:**
- Modify: `src/tools/sh_session.rs` (test module)

Same as Task 8 — extract duplicated `SessionManager::new + create_session` into a helper for DRY. Actual PTY reduction requires OnceCell, which is out of scope.

**Step 1: Add shared helper**

```rust
async fn shared_mgr_with_session(name: &str) -> SessionManager {
    let mgr = SessionManager::new(test_config());
    mgr.create_session(name, Some(bash_path()))
        .await
        .expect("shared session");
    mgr
}
```

**Step 2: Refactor PTY tests to use helper**

Update `test_list_sessions`, `test_close_session`, `test_create_duplicate_name`, `test_list_multiple_sessions` to use the helper. Tests that need special config (`test_create_limit_reached` with `test_config_with_max(1)`) keep their own setup.

**Step 3: Run tests**

```bash
cargo test --lib tools::sh_session::tests -- --test-threads=1
```

Expected: All 17 pass.

**Step 4: Commit**

```bash
git add src/tools/sh_session.rs
git commit -m "refactor: extract shared session helper in sh_session tests"
```

---

### Task 10: Full suite verification

**Step 1: Run the full test suite**

```bash
cargo test 2>&1 | tee /tmp/mish-test-output.txt
```

**What to look for:**
- No hangs (should complete in <60s)
- No `InitTimeout` errors
- If there's a deterministic failure, it should now be clearly visible without timeout noise

**Step 2: If any test fails, investigate**

The serial gate should have eliminated all resource-contention failures. Any remaining failure is a real bug, not a flaky hang.

**Step 3: Commit any fixes from investigation**

---

### Summary

| Task | File | Tests Annotated | PTY Reduction |
|------|------|-----------------|---------------|
| 1 | `src/core/pty.rs` | 10 | none (need isolation) |
| 2 | `src/session/shell.rs` | 11 | none (need isolation) |
| 3 | `src/handlers/condense.rs` | 8 | none (need isolation) |
| 4 | `src/tools/sh_session.rs` | 7 | DRY helper |
| 5 | `src/tools/sh_spawn.rs` | 8 | DRY helper |
| 6 | `src/mcp/server.rs` | 2 | none |
| 7 | `tests/cli_integration.rs` | 35 | none (binary spawn) |
| 8 | `src/tools/sh_spawn.rs` | — | DRY refactor |
| 9 | `src/tools/sh_session.rs` | — | DRY refactor |
| 10 | Full suite | — | Verification |

**Total `#[serial(pty)]` annotations: ~81 tests**
**Expected wall clock: ~45s serial PTY + ~2s parallel unit = ~47s**
