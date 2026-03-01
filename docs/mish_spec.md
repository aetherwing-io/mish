# mish — Technical Specification v2.0

**LLM-Native Shell: A process supervisor that speaks MCP, returns structured observations with global state on every call, and delegates decisions through a policy → LLM → operator escalation chain.**

This is the definitive specification. Companion docs cover specific subsystems:

- [SPEC.md](SPEC.md) — Project overview and quick reference
- [ARCHITECTURE.md](ARCHITECTURE.md) — System design and pipeline
- [PROXY.md](PROXY.md) — Universal command proxy and category system
- [GRAMMARS.md](GRAMMARS.md) — Grammar schema and tool grammar specs
- [CLASSIFIER.md](CLASSIFIER.md) — Classification engine and heuristics
- [STREAMING.md](STREAMING.md) — Streaming buffer and PTY capture
- [DEDUP.md](DEDUP.md) — Deduplication engine
- [PREFLIGHT.md](PREFLIGHT.md) — Argument injection (quiet and verbose flags)
- [VERBOSITY.md](VERBOSITY.md) — Verbose flag injection and file stat enrichment
- [ENRICH.md](ENRICH.md) — Error enrichment (failure diagnostics)

---

## 1. Overview

mish is a Rust binary with two interfaces: an **MCP server** (`mish serve`) for LLM process supervision, and a **CLI proxy** (`mish <command>`) for context-efficient shell output. Both interfaces share a common core: the category router, squasher pipeline, grammar system, and shared primitives. See [ARCHITECTURE.md](ARCHITECTURE.md) for the unified execution model.

**MCP server mode** manages one or more pseudo-terminal (PTY) sessions, intercepts the byte stream between processes and the LLM, and exposes OS interaction through strictly typed JSON-RPC tools. It replaces the raw `subprocess.run()` pattern used by every existing LLM agent with structured, context-aware process supervision.

**CLI proxy mode** wraps individual shell commands with category-aware structured output. Every command is categorized (condense, narrate, passthrough, structured, interactive, dangerous) and routed to the appropriate handler. See [PROXY.md](PROXY.md) for the full category system.

**Shared core:** The category router, squasher pipeline (VTE parsing, progress removal, dedup, truncation), grammar system, file stat primitives, and error enrichment are used by both modes. `sh_run` in MCP mode invokes the same category router that the CLI proxy uses — the category system is shared infrastructure, not a CLI-only feature.

### Design Principles

1. **Structured observation over raw text.** The LLM specifies what it cares about. mish filters everything else.
2. **Global state on every response.** Every tool call returns a process table digest — the LLM never needs to poll.
3. **Three-tier decision escalation.** Policy → LLM → Operator. Each tier is progressively rarer.
4. **Concurrent by default.** Multiple named sessions with independent process lifecycles.
5. **The LLM has temporal control.** Wait conditions, watch patterns, timeouts, and background execution are first-class primitives.
6. **Honest security posture.** The policy engine is an operational safety net, not a security boundary. Defense-in-depth requires OS-level sandboxing.

### Tech Stack

| Component | Choice | Rationale |
|-----------|--------|-----------|
| Language | Rust | Memory safety for PTY byte stream handling, `tokio` for async |
| Async runtime | tokio | Goroutine-equivalent async for PTY multiplexing |
| PTY | Raw `forkpty` via `nix` crate | Direct fd control needed for tokio integration, ioctl, and operator handoff |
| MCP transport | stdio | stdio for Claude Code/Cursor/Cline compatibility |
| Config | TOML | Policy files, server config |
| ANSI parsing | `vte` crate | Proper DEC/ANSI state machine handles all sequence types (CSI, OSC, DCS, APC) |

### Non-Goals (Explicitly Out of Scope)

These features are intentionally excluded. They may be revisited with evidence of user demand:

- **TUI snapshot mode** — LLM agents do not interact with TUIs. An LLM that needs system stats runs `ps aux`, not `htop`.
- **WebSocket terminal** — Security risk (localhost WebSocket accessible from any browser tab). CLI attach is sufficient.
- **SSE transport** — stdio works for every MCP client that matters. The digest-on-every-response pattern already solves the notification problem.
- **Multi-host / remote mish** — Speculative architecture. Build for local first.

---

## 2. Architecture

This section describes the MCP server mode architecture. For the unified execution model showing both CLI proxy and MCP server modes, see [ARCHITECTURE.md](ARCHITECTURE.md).

```
┌─────────────────────────────────────────────────────┐
│                    MCP Client                        │
│            (Claude Code, Cursor, Goose, etc.)        │
└──────────────────────┬──────────────────────────────┘
                       │ JSON-RPC over stdio
┌──────────────────────▼──────────────────────────────┐
│                     mish serve                      │
│  ┌─────────────┐ ┌──────────┐ ┌──────────────────┐  │
│  │  MCP Server  │ │ Policy   │ │ Process Table    │  │
│  │  (tool       │ │ Engine   │ │ (global state,   │  │
│  │   dispatch)  │ │          │ │  digest on every │  │
│  │             │ │          │ │  response)       │  │
│  └──────┬──────┘ └────┬─────┘ └────────┬─────────┘  │
│         │             │                │             │
│  ┌──────▼─────────────▼────────────────▼─────────┐  │
│  │              Category Router (shared core)     │  │
│  │  sh_run/sh_spawn → categorize → handler        │  │
│  │  condense·narrate·passthrough·structured·      │  │
│  │  interactive(error)·dangerous(policy)          │  │
│  └───────────────────────────────────────────────┘  │
│  ┌───────────────────────────────────────────────┐  │
│  │              Session Manager                   │  │
│  │  ┌──────────┐ ┌──────────┐ ┌──────────┐       │  │
│  │  │Session A │ │Session B │ │Session C │  ...  │  │
│  │  │(PTY+proc)│ │(PTY+proc)│ │(PTY+proc)│       │  │
│  │  └──────────┘ └──────────┘ └──────────┘       │  │
│  └───────────────────────────────────────────────┘  │
│  ┌───────────────────────────────────────────────┐  │
│  │         Shared Primitives (used by both modes) │  │
│  │  Squasher · Grammar · Classifier · Dedup ·    │  │
│  │  Stat · Enrichment · Preflight · Format       │  │
│  └───────────────────────────────────────────────┘  │
│  ┌───────────────────────────────────────────────┐  │
│  │           Yield / Heuristic Engine            │  │
│  │  Silence detection → prompt pattern match →   │  │
│  │  policy route → yield to LLM or operator      │  │
│  └───────────────────────────────────────────────┘  │
│  ┌───────────────────────────────────────────────┐  │
│  │           Operator Handoff Service            │  │
│  │  CLI attach (authenticated, single-use IDs)   │  │
│  └───────────────────────────────────────────────┘  │
│  ┌───────────────────────────────────────────────┐  │
│  │              Audit Log                        │  │
│  │  Every command, policy decision, handoff      │  │
│  │  event → append-only log, not LLM-writable    │  │
│  └───────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────┘
```

---

## 3. MCP Tools

mish exposes 5 MCP tools. All responses include the process table digest.

### 3.1 `sh_run` — Synchronous Execution

Execute a command and block until exit or timeout. The primary tool for short-lived commands.

**Parameters (MVP):**

| Param | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `cmd` | string | yes | — | Command to execute (passed to shell) |
| `timeout` | integer | no | 300 | Seconds before kill |
| `watch` | string | no | null | Regex patterns to capture (pipe-separated, case-insensitive) |
| `unmatched` | string | no | "keep" | What to do with non-matching lines when `watch` is set: "keep" or "drop" |

**Command execution model:** Commands are written to the session shell's stdin, wrapped with boundary detection sequences (see §10). This means CWD and environment variables persist between `sh_run` calls — the shell is a stateful REPL, not an isolated `bash -c` invocation. The LLM already knows shell. Use `cd /path && cmd` for working directory, `FOO=bar cmd` for environment variables, `cmd | head -50` for truncation. Don't fight the shell — extend it.

**Deferred parameters (documented for future implementation):**

| Param | Type | Default | Description | Why deferred |
|-------|------|---------|-------------|--------------|
| `session` | string | "main" | Named session to execute in | Single session sufficient for MVP |
| `cwd` | string | session cwd | Working directory override | `cd && cmd` works |
| `env` | object | {} | Additional environment variables | `FOO=bar cmd` works |
| `head` | integer | null | Keep first N lines | `cmd \| head -N` works; squasher Oreo handles most cases |
| `tail` | integer | null | Keep last N lines | `cmd \| tail -N` works; squasher Oreo handles most cases |
| `on_error` | string | "tail" | Output policy if exit != 0 | Squasher defaults are good enough initially |
| `digest` | string | "changed" | Process table detail level | Implement digest modes when context pressure is validated |

**Response:**

```json
{
  "result": {
    "exit_code": 0,
    "duration_ms": 14500,
    "cwd": "/home/user/project",
    "output": "added 142 packages, and audited 143 packages in 14s\nfound 0 vulnerabilities",
    "matched_lines": ["added 142 packages, and audited 143 packages in 14s"],
    "lines": { "total": 1243, "shown": 2 }
  },
  "processes": [ ... ]
}
```

**Notes on the response schema:**

- `output` contains the merged PTY stream (stdout and stderr combined). PTYs merge both streams into a single output — there is no way to separate them. This is the same behavior as a real terminal.
- `cmd` is not echoed back — the LLM already knows what it sent.
- `cwd` is always included — it tracks the session's working directory after command execution.
- `lines.total` vs `lines.shown` replaces the verbose `lines_total` / `lines_captured` / `lines_discarded` / `truncation_applied` fields.

**Category-aware execution:** `sh_run` uses the shared category router (see [ARCHITECTURE.md](ARCHITECTURE.md) and [PROXY.md](PROXY.md)). The command is categorized (condense, narrate, passthrough, structured, interactive, dangerous) and dispatched to the appropriate handler. This means `sh_run("cp file backup/")` returns a narrated response (`"→ cp: file → backup/ (4.2KB)"`) rather than running the squasher on a command that produces no output. The squasher pipeline (below) is invoked by the condense handler for verbose-output commands.

**Squasher behavior (condense-category commands, no watch/head/tail specified):**

1. Parse all terminal sequences via VTE state machine, emit only printable text
2. Drop progress bar frames (lines ending with `\r` without `\n`, cursor-up sequences `\033[<N>A` for any N)
3. Deduplicate consecutive identical lines (collapse with count)
4. Apply "Oreo" truncation: if output exceeds 200 lines, keep first 50 + last 150 with enriched truncation marker
5. If `watch` is set: only capture lines matching the regex (case-insensitive), handle the rest per `unmatched`
6. If `head`/`tail` is set: override Oreo with explicit limits
7. If `on_error: "full"` and exit code != 0: return full output (still VTE-cleaned) up to configurable byte limit

**Known limitation — process group escapes:** If a command spawns a daemon that calls `setsid()`, the daemon creates a new session and escapes the process group. Timeout-triggered `killpg()` will not reach it, resulting in an orphan process reparented to PID 1. This is inherent to Unix process management. Future mitigation: Linux cgroups can track all descendant processes regardless of session/group changes.

**Enriched truncation markers:**

When lines are truncated, the marker includes a statistical summary of the removed content:

```
--- 800 lines truncated (3 matching "warning", 0 matching "error") ---
```

The marker scans truncated lines against common diagnostic patterns (`error`, `warning`, `fail`, `panic`, plus any active `watch` pattern). This helps the LLM decide whether to request full output without wasting a tool call.

**Watch presets:**

In addition to raw regex, the LLM can reference named presets:

| Preset | Pattern |
|--------|---------|
| `@errors` | `error\|ERR!\|fatal\|panic\|exception\|traceback` |
| `@warnings` | `warn\|deprecat` |
| `@test_results` | `passed\|failed\|error\|skip\|===.*===\|TOTAL` |
| `@npm` | `added\|ERR!\|WARN\|vulnerability\|audit` |

The `watch` parameter is treated as a preset name if and only if it exactly matches `@[a-z_]+`. All other values are treated as regex patterns.

Example: `sh_run(cmd="npm install", watch="@npm", unmatched="drop")`

### 3.2 `sh_spawn` — Asynchronous / Background Execution

Start a process, optionally wait for a regex match, then return control. The process continues in the background.

**Parameters (MVP):**

| Param | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `alias` | string | yes | — | Unique name for this background process |
| `cmd` | string | yes | — | Command to execute |
| `wait_for` | string | no | null | Regex to match before returning success (case-insensitive) |
| `timeout` | integer | no | 300 | Seconds to wait for `wait_for` (or total if no wait_for) |

**Deferred parameters:**

| Param | Type | Default | Description | Why deferred |
|-------|------|---------|-------------|--------------|
| `session` | string | "main" | Named session | Single session for MVP |
| `cwd` | string | session cwd | Working directory override | `cd && cmd` works |
| `env` | object | {} | Additional environment variables | `FOO=bar cmd` works |
| `watch` | string | null | Ongoing regex to surface in process table | Add when background monitoring patterns emerge |
| `watch_mode` | string | "first" | "first" or "continuous" match accumulation | Depends on `watch` |
| `notify_operator` | boolean | false | Route yields to operator instead of LLM | Phase 3 (handoff) |

**Response (after wait_for matched):**

```json
{
  "result": {
    "alias": "devserver",
    "pid": 8492,
    "session": "main",
    "state": "running",
    "wait_matched": true,
    "match_line": "Ready on http://localhost:3000",
    "duration_to_match_ms": 4200
  },
  "processes": [ ... ]
}
```

**Response (wait_for timeout):**

```json
{
  "result": {
    "alias": "devserver",
    "pid": 8492,
    "session": "main",
    "state": "running",
    "wait_matched": false,
    "output_tail": "... last 20 lines of output ...",
    "reason": "wait_for regex did not match within 300s timeout"
  },
  "processes": [ ... ]
}
```

### 3.3 `sh_interact` — Process Interaction

Send input to a process, read output, signal, or kill it.

**Parameters (MVP):**

| Param | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `alias` | string | yes | — | Target process |
| `action` | string | yes | — | See action table below |
| `input` | string | cond. | — | For "send" action: string to write to stdin |
| `lines` | integer | no | 50 | For "read_tail": number of lines |

**Actions (MVP):**

| Action | Description |
|--------|-------------|
| `send` | Write `input` to the process stdin (include `\n` if enter needed) |
| `read_tail` | Return last N lines of squashed output buffer |
| `signal` | Send SIGINT by writing `\x03` to the PTY master (correct Ctrl-C simulation) |
| `kill` | SIGKILL the process group via `killpg()` |
| `status` | Return detailed status of this specific process |

**Deferred actions:**

| Action | Description | Why deferred |
|--------|-------------|--------------|
| `read_full` | Return raw unsquashed output from spool | Requires raw output spool (Phase 2) |
| `handoff` | Yield process to operator | Phase 3 (handoff) |
| `dismiss` | Remove completed process from digest | Implement when context pressure is validated |
| `signal` (SIGTERM/SIGKILL) | Direct signal via `killpg()` | MVP `signal` sends SIGINT only; `kill` handles the rest |

**Signal delivery notes:**

- `SIGINT`: Written as `\x03` to the PTY master fd. The kernel's line discipline translates this to SIGINT to the foreground process group. This correctly handles pipelines and process groups.
- `SIGTERM`/`SIGKILL`: Sent via `killpg(pgrp, signal)` to kill the entire process group, not just the leader. This prevents orphaned child processes.
- `SIGTSTP`/`SIGCONT` are intentionally not supported. Job control through a PTY proxy introduces complex state management that is not worth the complexity.

**Response (send):**

```json
{
  "result": {
    "alias": "deploy",
    "action": "send",
    "bytes_written": 4,
    "state": "running"
  },
  "processes": [ ... ]
}
```

**Response (handoff) — Phase 3:**

```json
{
  "result": {
    "alias": "deploy",
    "action": "handoff",
    "state": "handed_off",
    "handoff_id": "hf_<crypto-random-128bit>",
    "attach_cmd": "mish attach hf_...",
    "reason": "Terraform requesting MFA credentials"
  },
  "processes": [ ... ]
}
```

**Error (process is handed off) — Phase 3:**

```json
{
  "error": {
    "code": -32001,
    "message": "Process 'deploy' is under operator control. Use sh_interact(alias='deploy', action='status') to check."
  },
  "processes": [ ... ]
}
```

### 3.4 `sh_session` — Session Lifecycle

Manage shell sessions. MVP uses a single "main" session created at server startup.

**Parameters (MVP):**

| Param | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `action` | string | yes | — | "list" |

MVP exposes only `list` — the "main" session is created automatically at startup. The LLM uses shell builtins for everything else (`cd`, `export`, `env`).

**Deferred actions:**

| Action | Description | Why deferred |
|--------|-------------|--------------|
| `create` | Create a named session with custom shell/cwd | Multi-session is Phase 2 |
| `close` | Close a named session | Multi-session is Phase 2 |
| `env_set` | Set environment variables | `export FOO=bar` via sh_run works |
| `env_get` | Read environment variables | `echo $FOO` or `printenv FOO` via sh_run works |

**Response (list):**

```json
{
  "result": {
    "sessions": [
      { "session": "main", "shell": "/bin/bash", "cwd": "/home/user/project", "active_processes": 0 }
    ]
  },
  "processes": [ ... ]
}
```

### 3.5 `sh_help` — Self-Documenting Reference

Returns a compact reference card (under 500 tokens) including:

- All tools, their parameters, and defaults
- Available watch presets and their patterns
- Active policy summary (what auto-confirm rules exist, what's forbidden)
- Squasher defaults (Oreo split, max lines, etc.)
- Current resource limits and usage (sessions, processes)

No parameters. The response is a structured object, not free-form text.

This tool is trivial to implement (static data, no PTY interaction) and essential for:
- **Discovering watch presets** (`@errors`, `@npm`, etc.) which are not in the tool schema
- **Understanding active policies** so the LLM doesn't duplicate what the policy engine handles
- **Context recovery** after conversation compaction where tool schemas may be summarized

---

## 4. Error Codes

All errors use JSON-RPC error codes. mish defines the following application-specific codes:

| Code | Meaning |
|------|---------|
| -32001 | Process is under operator control (handed off) |
| -32002 | Session not found |
| -32003 | Alias not found |
| -32004 | Alias already in use |
| -32005 | Command blocked by policy |
| -32006 | Session limit reached |
| -32007 | Process limit reached |
| -32008 | Session closed / not ready |
| -32009 | Invalid action for current process state |

---

## 5. Process Table Digest

**Every response from every tool includes a `processes` field reflecting the state of tracked processes.** This is the LLM's ambient awareness mechanism.

### Process States

| State | Description |
|-------|-------------|
| `running` | Process is executing, output is being captured |
| `completed` | Process exited normally |
| `failed` | Process exited with non-zero code |
| `awaiting_input` | Yield engine detected process is waiting for input |
| `handed_off` | Process is under operator control |
| `killed` | Process was killed by signal |
| `timed_out` | Process exceeded timeout and was killed |

**Note:** The state is named `awaiting_input` (not `suspended`) to avoid confusion with POSIX job-control stopped state.

### Digest Entry Schema

```json
{
  "alias": "string",
  "session": "string",
  "state": "string (enum above)",
  "pid": 12345,
  "exit_code": null | 0 | 1,
  "signal": "SIGKILL",
  "elapsed_ms": 34000,
  "duration_ms": null | 14500,

  // Present only in specific states:
  "prompt_tail": "Proceed? [y/N]",           // state: awaiting_input
  "last_match": "47 passed, 2 failed",       // if watch is active
  "match_count": 3,                          // if watch_mode: continuous
  "handoff_id": "hf_...",                    // state: handed_off
  "output_summary": "added 142 packages",    // state: completed (compact)
  "error_tail": "Error: ENOENT ...",         // state: failed (last line)
  "notify_operator": true                    // if operator notification is pending
}
```

### Digest Modes

The `digest` parameter on `sh_run` (and future tools) controls verbosity:

| Mode | Behavior |
|------|----------|
| `"full"` | Return all tracked processes |
| `"changed"` (default) | Return only entries modified since the last response (tracked via monotonic sequence counter — see below). First call returns full table. |
| `"none"` | Omit the process table entirely (for trivial commands where the LLM doesn't need it) |

### Change Tracking

The server maintains a monotonic sequence counter, incremented on every process table mutation (state change, new process, exit, watch match). Each tool call response records the current sequence number. In `"changed"` mode, the response includes only entries modified since the sequence recorded on the previous response to that client. If no prior response exists (first call or client reconnect), the full table is returned.

### Lifecycle Rules

- Completed/failed/killed processes remain in the digest for **5 minutes** or until explicitly dismissed via `sh_interact(action="dismiss")`.
- The digest is ordered by: `awaiting_input` first (needs attention), then `running`, then `completed`/`failed`. Within each category, entries are sorted by `elapsed_ms` descending (longest-running first).
- If the digest exceeds 20 entries, only the 10 most recent completed/failed entries are retained (running/awaiting_input are always included).
- After the LLM has seen a completed/failed entry at least once (via a full or changed digest), subsequent digests reduce it to a compact stub: `{"alias": "build", "state": "completed", "exit_code": 0}`.

---

## 6. Squasher Pipeline

The squasher pipeline processes output from **condense-category commands** (the default for unknown commands). It is the condense handler in the category routing system — see [ARCHITECTURE.md](ARCHITECTURE.md). Other categories (narrate, passthrough, structured) use different handlers that may invoke individual squasher components (e.g., VTE stripping) without running the full pipeline.

The pipeline is:

```
Raw PTY bytes
  → UTF-8 decode (with streaming carryover for partial multi-byte sequences)
  → VTE state machine parse (emit only printable text, strip all escape sequences)
  → Progress bar annihilation (operates on VTE-parsed logical line buffer — see below)
  → Duplicate line deduplication (consecutive identical lines collapsed with count)
  → Pattern matching (watch regex, case-insensitive)
  → Truncation (head/tail/Oreo with enriched markers)
  → Structured output construction
```

### ANSI / Terminal Sequence Handling

mish uses the `vte` crate to implement a proper DEC/ANSI state machine parser. A custom `Perform` implementation emits only `print()` characters, correctly handling:

- CSI sequences (colors, cursor movement, clearing)
- OSC sequences (hyperlinks, terminal titles, iTerm2 image protocol, OSC 133 shell integration)
- DCS sequences (sixel graphics, tmux passthrough)
- APC sequences (kitty graphics protocol)
- Bracketed paste mode markers

This approach handles every sequence type by construction, rather than pattern-matching known sequences and missing edge cases.

### Progress Bar Detection

Progress bar annihilation runs **after VTE parsing**, on the logical line buffer that tracks CR vs LF positions. The VTE state machine has already processed cursor-control sequences (`\033[K` erase line, `\033[2K` erase entire line, `\033[<N>A` cursor up) before progress detection examines the result. Detection criteria:

- **CR without LF:** A line overwritten by carriage return without a newline — the hallmark of progress bars and spinners. Only the final overwrite is retained.
- **Cursor-up overwrites:** Lines rewritten via cursor-up sequences (cargo/rustc-style multi-line progress). The VTE `Perform` implementation tracks cursor position; when cursor-up moves above the current line, the overwritten lines are marked as progress frames and dropped.

This two-stage approach (VTE parse → progress detection) correctly handles all terminal-based progress displays regardless of which escape sequences they use.

### UTF-8 Streaming Decode

PTY reads may split multi-byte UTF-8 sequences at buffer boundaries. The decoder:

1. Maintains a 4-byte carryover buffer between reads
2. Uses `std::str::from_utf8()` — if `error_len()` is `None`, stashes incomplete trailing bytes for the next read
3. If `error_len()` is `Some(n)`, replaces invalid bytes with U+FFFD (lossy decode)
4. If more than 10% of bytes in a buffer are non-UTF-8, switches to binary mode: `"<binary output, N bytes>"` instead of attempting decode

### Output Merging

**PTYs merge stdout and stderr into a single stream.** This is inherent to how pseudo-terminals work — both streams go to the same PTY slave fd. The response field is named `output` (not `stdout`/`stderr`) to reflect this honestly.

If the LLM needs stderr separated, it should use shell redirection: `sh_run(cmd="my_command 2>/tmp/err.log")` and read the file separately.

### JSON Encoding

All output strings in MCP responses are properly JSON-escaped. Control characters (NUL, tabs, raw newlines) are escaped per RFC 8259. Binary detection (see UTF-8 Streaming Decode above) prevents non-text data from reaching JSON serialization — binary output is replaced with a `"<binary output, N bytes>"` placeholder before encoding.

### Squasher Defaults

| Setting | Default | Override |
|---------|---------|---------|
| Max output lines | 200 | `head`/`tail` params |
| Oreo split | 50 head / 150 tail | `head`/`tail` params |
| Max output bytes | 64KB | Server config |
| Progress bar detection | CR without LF, `\033[<N>A` (any N) | Not overridable |
| Sequence stripping | Always on (VTE state machine) | Not overridable |
| Deduplication | On | Server config |
| Watch case sensitivity | Case-insensitive | Not overridable |

### Raw Output Spool

mish maintains a circular buffer (default 1MB) of **raw unsquashed output** per process. This is the escape hatch — if the squasher discarded something important, the LLM can request it via `sh_interact(action="read_full")`. The spool is also used for operator handoff (the operator sees full session history).

The spool uses a mutex-guarded ring buffer with defined behavior under contention: concurrent writes and reads are serialized, and reads never block writes (reads get a snapshot of the current buffer state).

---

## 7. Policy Engine

The operator (deployer of mish) defines policies that auto-resolve known situations without LLM involvement. Policy is the first tier in the decision escalation chain.

### Security Disclaimer

**The policy engine is an operational safety net, not a security boundary.** It catches common mistakes by a well-meaning LLM. It does not protect against a prompt-injected LLM that is actively trying to bypass it. Specifically:

- `policy.forbidden` matches command strings, which is trivially bypassed via indirection (`bash -c "..."`, variable expansion, base64 encoding, aliasing, heredocs, etc.).
- `auto_confirm` rules can be triggered by malicious process output that prints matching prompt text.
- Scope matching by command name is easily defeated by wrappers (`env`, `sudo`, `nice`, `command`).

For actual security, deploy mish inside an OS-level sandbox (see §11).

### Policy Configuration (TOML)

```toml
# mish.toml — loaded at server startup (immutable at runtime)

[server]
max_sessions = 5
max_processes = 20
max_spool_bytes_total = 52428800  # 50MB aggregate
idle_session_timeout_sec = 3600   # Clean up leaked sessions after 1 hour
config_path = "~/.config/mish/mish.toml"  # Default location, outside project dir

[squasher]
max_lines = 200
max_bytes = 65536
oreo_head = 50
oreo_tail = 150
spool_bytes = 1048576

[yield]
silence_timeout_ms = 2500
prompt_patterns = ['[?]$', ':$', '>$', 'Password', 'passphrase', '[y/N]', '[Y/n]']

[timeout_defaults]
default = 300

# Per-scope timeout overrides
[timeout_defaults.scope]
terraform = 1800
docker = 600
cargo = 600
npm = 300
pip = 300

# Watch presets
[watch_presets]
errors = "error|ERR!|fatal|panic|exception|traceback"
warnings = "warn|deprecat"
test_results = "passed|failed|error|skip|===.*===|TOTAL"
npm = "added|ERR!|WARN|vulnerability|audit"

# Auto-confirm rules: matched top-to-bottom, first match wins
# IMPORTANT: Always use scope to limit blast radius
[[policy.auto_confirm]]
match = 'Do you want to continue'
respond = "Y\n"
scope = ["apt", "apt-get"]

[[policy.auto_confirm]]
match = 'Proceed\?'
respond = "y\n"
scope = ["npm"]

[[policy.auto_confirm]]
match = 'Is this ok \[y/d/N\]'
respond = "y\n"
scope = ["dnf", "yum"]

# Operator yield rules: bypass LLM, go straight to operator
[[policy.yield_to_operator]]
match = '[Pp]assword|MFA|OTP|token|passphrase|[Aa]uthenticat'
notify = true

# Forbidden commands: block before execution (speed bump, not security)
[[policy.forbidden]]
pattern = 'rm -rf /'
action = "block"
message = "Command blocked by policy"

[handoff]
timeout_sec = 600
fallback = "yield_to_llm"  # "yield_to_llm" | "kill" | "wait"

[audit]
log_path = "~/.local/share/mish/audit.log"  # Append-only, outside project dir
log_level = "info"  # "debug" | "info" | "warn" | "error"
log_commands = true
log_policy_decisions = true
log_handoff_events = true
```

### Policy Scope Matching

The `scope` field on auto_confirm rules matches against the **command binary name** (first token of the command string, basename only). For example, `scope = ["npm"]` matches `npm install`, `/usr/bin/npm run build`, etc.

If `scope` is omitted, the rule applies to all commands. **Unscoped auto_confirm rules are strongly discouraged** — they allow any process to trigger automated responses by printing matching text.

Known limitations of scope matching:
- `env npm install` → scope is `env`, not `npm`
- `sudo apt install` → scope is `sudo`, not `apt`
- `bash -c "rm ..."` → scope is `bash`

These are documented limitations, not bugs. The policy engine is a convenience layer. See §11 for real security.

### Policy Precedence

1. `policy.forbidden` — checked before execution, blocks the command entirely
2. `policy.yield_to_operator` — checked when yield engine detects a prompt
3. `policy.auto_confirm` — checked when yield engine detects a prompt (scope must match)
4. Fall through to LLM — if no policy matches, the yield is surfaced in the process table digest

**Note:** `auto_reject` has been removed. If a process needs to be killed, the LLM can do it via `sh_interact(action="kill")`, or the operator can define a `policy.forbidden` rule to prevent the command from running in the first place.

---

## 8. Operator Handoff Protocol

### 8.1 Handoff Initiation

Three paths to handoff:

**A. LLM-initiated:** The LLM calls `sh_interact(alias="deploy", action="handoff", reason="MFA required")`. mish transitions the process to `handed_off` state and returns a `handoff_id` + `attach_cmd`.

**B. Policy-initiated:** A `yield_to_operator` rule matches a detected prompt. mish transitions the process to `handed_off` automatically. The process table digest shows `state: "handed_off"` and `notify_operator: true`.

**C. Preemptive (notify_operator flag):** The LLM spawned the process with `notify_operator: true`. Any yield that doesn't match an `auto_confirm` policy is routed directly to operator.

### 8.2 Operator Attachment

```bash
# Operator runs this in a separate terminal
mish attach <handoff_id>

# Or list active handoffs
mish handoffs
```

`mish attach` connects the operator's terminal directly to the process's PTY master fd. The operator sees the full terminal (including the prompt that triggered the yield) and can type normally.

**Handoff ID requirements:**
- Cryptographically random, minimum 128 bits of entropy
- Single-use: invalidated after first successful attachment
- Communicated to the operator out-of-band (printed to mish's stderr, shown via `mish handoffs`)
- **Not** returned to the LLM in the `handoff_id` field — the LLM gets a reference ID for status checking, but not the attachment credential

**Authentication:**
- `mish attach` communicates with the server via a Unix domain socket at a well-known path (`$XDG_RUNTIME_DIR/mish/<server-pid>/control.sock`)
- The socket has mode `0600` (owner-only access)
- Only one operator can attach per handoff. Concurrent attach attempts return an error.

**Operator notification:** Notification is passive — mish logs the handoff event to stderr and the audit log, and `mish handoffs` lists active handoffs. For near-real-time awareness, operators can run `mish handoffs --watch`, which polls every 5 seconds and prints new handoffs as they appear. Push notification (desktop notifications, webhooks) is a backlog item — add with evidence of operator demand.

### 8.3 Handoff State Transitions

```
                   ┌─────────────┐
                   │   running   │
                   └──────┬──────┘
                          │ yield detected
                   ┌──────▼──────┐
                   │awaiting_input│
                   └──────┬──────┘
            ┌─────────────┼──────────────┐
            │ LLM handles │ policy routes │ LLM handoff
            │ via send    │ to operator   │ request
            ▼             ▼               ▼
       ┌─────────┐  ┌──────────┐  ┌──────────┐
       │ running │  │handed_off│  │handed_off│
       └─────────┘  └────┬─────┘  └────┬─────┘
                         │              │
                    operator        operator
                    attaches        attaches
                         │              │
                    operator        operator
                    resolves        resolves
                         │              │
                    operator        operator
                    detaches        detaches
                         │              │
                    ┌────▼──────────────▼────┐
                    │      running           │
                    │  (LLM recaptures)      │
                    └────────────────────────┘
```

### 8.4 Handoff-Return Payload

When the operator detaches (or the yield condition resolves while operator is attached), mish transitions back to `running` and includes a handoff summary in the next digest:

```json
{
  "alias": "deploy",
  "state": "running",
  "handoff_resolved": {
    "duration_ms": 45000,
    "lines_during_handoff": 12,
    "outcome": "resolved"
  }
}
```

**Credential-blind mode (default):** The `handoff_resolved` payload does NOT include process output from during the handoff period. This prevents credential leakage via process echo — many programs echo typed input, and passwords/tokens could appear in stdout even when the operator's raw keystrokes are not captured. The LLM gets only the fact that the handoff resolved, the duration, and the line count.

If the operator explicitly opts in (via a flag during `mish attach --share-output`), process output during the handoff can be included. This is off by default.

### 8.5 Edge Cases

| Scenario | Behavior |
|----------|----------|
| LLM calls sh_interact on handed-off process | Error: "Process under operator control" (code -32001) |
| Operator never responds | After `handoff.timeout_sec`, execute `handoff.fallback` |
| Operator Ctrl-C in attached terminal | SIGINT sent to process, reflected in process table |
| Operator detaches, new prompt appears | Re-yield: check policy first, then surface in digest to LLM |
| Process exits while handed off | Operator sees exit, handoff ends, digest shows `completed`/`failed` |
| Second operator tries to attach | Error: "Handoff already attached" |

---

## 9. Yield / Heuristic Engine

The yield engine detects when a running process is waiting for interactive input.

### Detection Algorithm

```
loop (every 100ms while process is running):
  1. Read available bytes from PTY into buffer
  2. If bytes received: reset silence timer
  3. If silence timer > yield.silence_timeout_ms (default 2500ms):
     a. Check last 256 bytes of buffer against yield.prompt_patterns
     b. If match found → YIELD (prompt detected)
     c. If no match → continue waiting (do not yield on silence alone)
  4. On YIELD:
     a. Check policy.yield_to_operator rules → if match, initiate handoff
     b. Check policy.auto_confirm rules → if match AND scope matches, send response
     c. No policy match → set state to "awaiting_input", surface in digest
```

**Silence timer edge case:** The silence timer only activates after the process has produced at least one byte of output. A freshly launched process with no output yet (e.g., `cargo build` resolving dependencies) is not a yield candidate — it's still starting up. This prevents false yields on commands with slow startup.

**Concurrency note:** The yield engine and tool calls must not race. Each session has a mutex that serializes command execution. The yield engine uses atomic compare-and-swap for state transitions (e.g., `running → awaiting_input` only succeeds if the process is still in `running` state). If the LLM sends `sh_interact(action="send")` at the same time the yield engine fires `auto_confirm`, the mutex ensures only one input is delivered.

**Linux-only enhancement (optional, gated):**

On Linux, if `check_proc_syscall` is enabled in config, the yield engine can additionally read `/proc/{pid}/syscall` to detect if the process is blocked on `read(0, ...)` — a silent stdin wait with no prompt. This is gated behind `#[cfg(target_os = "linux")]` and is disabled by default. On macOS, the silence + prompt pattern heuristic is the only detection mechanism.

### Yield Output

When a process enters `awaiting_input` state, the digest entry includes:

```json
{
  "alias": "install",
  "state": "awaiting_input",
  "prompt_tail": "Are you sure you want to continue connecting (yes/no/[fingerprint])? ",
  "awaiting_since_ms": 34000,
  "action_hint": "Use sh_interact(alias='install', action='send', input='yes\\n') to respond, or action='kill' to abort"
}
```

---

## 10. Session Management

### Session Model

Each session is an independent shell process with its own PTY. Sessions have:

- A name (string, unique)
- A shell process (bash/zsh/sh)
- A current working directory (tracked after every command)
- Environment variables (tracked)
- Zero or more child processes spawned within the session
- A raw output spool (circular buffer)
- A mutex for serialized command execution

### Shell Initialization

Sessions spawn the shell as an **interactive non-login shell** (`bash -i` / `zsh -i`). This means:

- `.bashrc` / `.zshrc` are sourced (aliases and functions available)
- `.profile` / `.bash_profile` are NOT sourced (avoids re-running login scripts)
- The shell detects a TTY and enables job control, prompt, etc.

**Startup sequence:** After spawning the shell, mish injects the PROMPT_COMMAND/precmd hook and then waits for the initial hook to fire, confirming the shell is at a prompt. All startup output (motd, `.bashrc` messages, PS1 rendering) is discarded. The session is not marked "ready" — and tool calls are not accepted — until this initial prompt is detected. This prevents the yield engine from treating shell startup output as an `awaiting_input` state.

### Session Limits

| Limit | Default | Config Key |
|-------|---------|------------|
| Max concurrent sessions | 5 | `server.max_sessions` |
| Max concurrent processes (across all sessions) | 20 | `server.max_processes` |
| Idle session timeout | 1 hour | `server.idle_session_timeout_sec` |

When limits are reached, new `sh_session(action="create")` or `sh_spawn` calls return error code -32006 or -32007. Idle sessions are automatically closed after the timeout, and their processes are killed.

### Command Boundary Detection

**Primary mechanism: Shell integration via PROMPT_COMMAND / precmd**

mish injects shell hooks at session creation to get reliable command boundary notification:

- **bash:** Sets `PROMPT_COMMAND` to emit an OSC 133 sequence with exit code: `printf '\033]133;D;%d\033\\' $?`
- **zsh:** Sets `precmd` to emit the same sequence.

This fires after every command completes, giving mish:
- Exact command boundary (the shell itself signals completion)
- Exit code (embedded in the sequence)
- Resilience against `set -x`, interactive subprocesses, and other sentinel-breaking scenarios

**Fallback mechanism: UUID sentinels**

For shells that don't support `PROMPT_COMMAND` (sh, dash), mish falls back to the sentinel approach:

1. Before the command: `echo __LLMSH_START_<uuid>__`
2. After the command: `echo __LLMSH_END_<uuid>__ $?`
3. Output between sentinels is captured; sentinel lines are stripped

Sentinel UUIDs are generated with `uuid::Uuid::new_v4()` (cryptographically random). Sentinels are validated with exact format matching (exact prefix, exact length, no partial matches).

### CWD Tracking

After every command, mish queries the session's current working directory by appending `; printf '\033]133;P;%s\033\\' \"$PWD\"` to the PROMPT_COMMAND sequence. `sh_run` blocks until the PROMPT_COMMAND/precmd hook fires and the CWD sequence is parsed — the returned `cwd` always reflects the post-command state, never a stale value from a previous command. The cwd is included in every `sh_run` response and in `sh_session(action="list")` output.

### Session Isolation

Sessions provide **logical isolation** (independent shells, environments, working directories) but NOT **security isolation**. All sessions run as the same OS user and share the same filesystem, network, and process namespace. A command in one session can affect another session's files or processes. This is documented, not a bug.

---

## 11. Security Model

### Threat Model

mish treats the LLM as a **semi-trusted, potentially compromised execution agent**. Prompt injection attacks can turn a benign LLM into an adversary with full shell access. The threat model accounts for:

1. **Compromised LLM (prompt injection):** An adversary who can issue arbitrary `sh_run`/`sh_spawn`/`sh_interact` calls.
2. **Malicious process output:** A process whose output is designed to manipulate the yield engine, policy engine, or the LLM's subsequent reasoning (indirect prompt injection).
3. **Local attacker:** Another user on the same machine attempting to interact with handoff mechanisms.

### Defense Layers

| Layer | What It Provides | What It Does NOT Provide |
|-------|-----------------|------------------------|
| **Policy engine** | Catches accidental misuse, auto-resolves known prompts | Security against adversarial bypass (trivially circumvented) |
| **Audit logging** | Post-incident forensics, compliance | Prevention |
| **Resource limits** | Prevents accidental fork bombs, runaway processes | Protection against determined DoS |
| **Handoff authentication** | Prevents unauthorized operator attachment | Protection if attacker has same-user access |
| **OS-level sandboxing** (optional) | Hard security boundary, filesystem/network restriction | — (this is the real security mechanism) |

### Optional Sandboxing

mish supports an optional `[sandbox]` configuration section for OS-level isolation:

```toml
[sandbox]
enabled = false  # Opt-in

# Filesystem restrictions (Linux: Landlock, macOS: sandbox-exec)
allowed_paths = ["/home/user/project", "/tmp"]
readonly_paths = ["/usr", "/etc"]

# Network restrictions
network = "none"  # "none" | "localhost" | "unrestricted"

# Process limits (Linux: cgroups)
max_pids = 100
max_memory_mb = 2048
```

When sandboxing is not enabled (the default), mish provides no meaningful security boundary beyond what the host OS provides. This is acceptable for local development but NOT for production deployments or multi-tenant environments.

### Audit Logging

Every tool call is logged to an append-only audit log at `~/.local/share/mish/audit.log`. The log includes:

- Timestamp, session name, tool name, full command string
- Policy decisions (allowed, blocked, auto-confirmed, yielded)
- Handoff events (initiated, attached, resolved)
- Process lifecycle events (started, exited, killed, timed out)

The audit log path is outside the project directory and is not writable by commands executed through mish (the log fd is not inherited by child processes).

### Config File Protection

The config file should be stored outside the project working directory (default: `~/.config/mish/mish.toml`). The `--config` flag is a CLI argument to the operator, not exposed through the MCP interface. mish does NOT support hot-reload of config — the config is read once at startup and is immutable for the lifetime of the server process.

---

## 12. Lifecycle and Failure Modes

### Server Startup

1. Read and validate config file
2. Register signal handlers (SIGTERM, SIGINT) for graceful shutdown
3. Create Unix domain socket for operator handoff communication
4. Create "main" session (default PTY + shell)
5. Start MCP stdio transport
6. Write PID file to `$XDG_RUNTIME_DIR/mish/<pid>.pid`

### Graceful Shutdown (SIGTERM, SIGINT, stdin EOF)

1. Stop accepting new tool calls
2. Send SIGTERM to all process groups in all sessions
3. Wait up to 5 seconds for processes to exit
4. Send SIGKILL to any remaining process groups
5. Close all PTYs
6. Close Unix domain socket
7. Remove PID file
8. Flush and close audit log
9. Exit

### Client Disconnect (stdin EOF)

When the MCP client disconnects (stdin reaches EOF), mish executes graceful shutdown. Long-running spawned processes do NOT survive server exit — they are children of the session shells, which are children of mish. This is a known limitation.

### Server Crash (unclean exit)

If mish crashes:
- PTY master fds are closed by the kernel, sending SIGHUP to session shells
- Most shell processes will exit on SIGHUP, which kills their children
- Processes that trap SIGHUP may survive as orphans (reparented to PID 1)
- The PID file remains stale; on next startup, mish checks for stale PID files and warns

### PTY Gotchas

- **Master fd must be `O_CLOEXEC`:** After `forkpty()`, the master fd is set to `O_CLOEXEC` so child processes don't inherit it. Without this, the master fd's refcount stays nonzero when the shell exits, preventing proper EIO/SIGHUP detection.
- **EIO on master read is normal:** When the PTY slave side closes, reads from the master return `EIO`. This is not an error — it signals "child exited / slave hung up."
- **PTY window size:** Set via `TIOCSWINSZ` ioctl at session creation. Default: 120x40 (provides more information than classic 80x24 for LLM consumption). Configurable per-session.

---

## 13. CLI Interface

mish has three modes: MCP server, CLI proxy, and CLI management.

### MCP Server Mode

```bash
# Start as MCP server (stdio transport)
mish serve

# Start with custom config
mish serve --config ~/.config/mish/mish.toml
```

### CLI Proxy Mode

```bash
# Every command goes through mish — category-aware structured output
mish npm install lodash
mish cp src/main.rs backup/
mish git status

# Output modes
mish --json npm test            # structured JSON for tool-use
mish --passthrough cargo build  # full output + summary at end
```

See [PROXY.md](PROXY.md) for the full category system (condense, narrate, passthrough, structured, interactive, dangerous) and routing logic. The CLI proxy uses the same category router and shared primitives as MCP server mode.

### CLI Management Mode

```bash
# List active handoffs
mish handoffs

# Attach to a handoff
mish attach <handoff_id>

# List all sessions and processes
mish ps

# View raw output spool for a process
mish logs <alias> [--lines N] [--raw]

# Validate config
mish config check [--config ./mish.toml]
```

---

## 14. MCP Server Registration

```json
{
  "mcpServers": {
    "mish": {
      "command": "mish",
      "args": ["serve", "--config", "~/.config/mish/mish.toml"]
    }
  }
}
```

---

## 15. Build Plan / Implementation Order

The build plan follows the four-layer architecture (see [ARCHITECTURE.md](ARCHITECTURE.md)). Phase 1 builds the shared core bottom-up and ships the CLI proxy. Phase 2 adds the MCP server on top of the proven core. Phase 3 adds LLM-native intelligence features.

**Design principle: build CLI first, design for MCP.** Internal interfaces anticipate concurrent processes and ambient state from the start, but only the CLI entry point ships in Phase 1. This front-loads the PTY platform risk and gets the shared core battle-tested against real tool output before MCP adds complexity.

### Phase 1: Shared Core + CLI Proxy

**Goal:** `mish <command>` works end-to-end with all 6 categories. 5 grammars with test fixtures.

**Step 1 is a validation gate.** If PTY proxying fails on macOS, stop and escalate. See ARCHITECTURE.md "Platform Risk: PTY Fidelity" for the pivot directive.

1. **Project scaffold** — Rust workspace, Cargo.toml, tokio runtime, clap CLI parsing (`mish serve | mish <command> | mish attach/ps/handoffs`)
2. **`core/pty.rs` + `core/line_buffer.rs`** — PTY allocation via `nix::pty::forkpty()`, byte → line assembly, overwrite detection. **PTY stress test on macOS: ANSI color passthrough, progress bar detection (CR without LF), SIGWINCH forwarding, raw mode detection, multi-byte UTF-8 at buffer boundaries. GATE: all tests must pass before proceeding.**
3. **`core/grammar.rs` + `router/categories.rs`** — TOML grammar loading, grammar front matter `category` field, `categories.toml` fallback mapping, unknown → condense default. Category resolution order: grammar front matter wins → categories.toml → default condense.
4. **`squasher/`** — Full pipeline: VTE-based ANSI parsing (`vte_strip.rs`), progress bar removal (`progress.rs`), deduplication (`dedup.rs`), Oreo truncation with enriched markers (`truncate.rs`), watch regex matching + presets (`pattern.rs`), UTF-8 streaming decode (`utf8.rs`), pipeline orchestration (`pipeline.rs`)
5. **`core/classifier.rs` + `core/emit.rs`** — Three-tier classification engine, emit buffer with flush triggers
6. **`handlers/condense.rs`** — Condense handler invoking squasher pipeline. Wire up: command → grammar → squasher → condensed output
7. **`core/stat.rs` + `handlers/narrate.rs`** — File stat primitives (shared by narration and enrichment), narration engine for file operations (cp, mv, mkdir, rm, chmod, ln, chown). Pre-flight stat → execute → post-flight stat → narrate
8. **`handlers/passthrough.rs` + `handlers/structured.rs`** — Passthrough (execute + metadata footer), structured handlers (git status, docker ps — inject `--porcelain`/`--format json`, parse, format)
9. **`handlers/interactive.rs` + `handlers/dangerous.rs`** — Interactive detection (raw mode on PTY → transparent passthrough, session summary on exit). Dangerous detection (pattern matching from `dangerous.toml` → terminal warning → maybe execute). CLI-mode behavior only for Phase 1.
10. **`core/enrich.rs`** — Error enrichment on failure. Path resolution, source/target existence, permission checks, command-not-found hints. Narrate handler runs first, enrichment adds diagnostics below on failure. Budget: <100ms total, read-only.
11. **`core/preflight.rs`** — Bidirectional argument injection. Quiet flag injection (reduce noise at source), verbose flag injection (enrich terse commands). Grammar-declared, never inject behavior-changing flags.
12. **`router/mod.rs`** — Top-level category routing: command → grammar lookup → categorize → dispatch to handler
13. **`cli/proxy.rs` + `core/format.rs`** — CLI entry point wiring it all together. Output formatting (human mode default, `--json`, `--passthrough`, `--context`). Compound command splitting (`&&`, `||`, `;`).
14. **5 grammars with test fixtures** — npm.toml, cargo.toml, git.toml, docker.toml, make.toml. Each grammar must have captured real output in `tests/fixtures/` as test fixtures before the grammar is declared complete.
15. **Integration tests** — Run real commands (ls, echo, npm install, cargo build, git status) through full CLI proxy pipeline, verify structured output matches expected format across all 6 categories.

### Phase 2: MCP Server

**Goal:** `mish serve` works as an MCP server. sh_run, sh_spawn, sh_interact are functional. Process table digest on every response.

16. **MCP stdio transport** — JSON-RPC request/response over stdin/stdout (`mcp/transport.rs`, `mcp/types.rs`, `mcp/dispatch.rs`)
17. **Session manager** — Single "main" session (`session/manager.rs`, `session/shell.rs`), shell spawn as interactive non-login shell, session limits
18. **Command boundary detection** — PROMPT_COMMAND/precmd shell integration (primary), UUID sentinel fallback, CWD tracking via shell hook (`session/boundary.rs`)
19. **sh_run tool** — Synchronous execution → category router → structured JSON response with `output`/`cwd`/`lines`. Watch patterns, `unmatched` parameter, watch presets (`@errors`, `@npm`, etc.)
20. **Process table** — Global state (`process/table.rs`, `process/state.rs`), digest with `changed`/`full`/`none` modes, compact stubs for seen entries, lifecycle rules (5min retention, 20-entry cap)
21. **sh_spawn tool** — Background execution, `wait_for` regex matching, alias management
22. **sh_interact tool** — send (stdin write via `\x03`), read_tail, signal (SIGINT via PTY), kill (SIGKILL via `killpg`), status
23. **Raw output spool** — Circular buffer per process (`process/spool.rs`), mutex-guarded, `read_full` action
24. **Multiple sessions** — sh_session tool (create, list, close), named sessions, per-session mutex
25. **sh_help tool** — Reference card with watch presets, active policy summary, squasher defaults, resource usage
26. **Basic safety** — Hardcoded deny-list for catastrophic commands (compiled in), resource limits (max sessions, max processes), timeout enforcement with process group kill
27. **Audit logging** — Append-only log (`audit/logger.rs`), fd non-inheritance, all commands and policy decisions
28. **Graceful shutdown** — Signal handlers (SIGTERM, SIGINT, stdin EOF), process group cleanup, PID file
29. **MCP integration tests** — Full MCP round-trip: JSON-RPC → sh_run → structured response → verify process table digest

### Phase 3: Intelligence & Operator Handoff

30. **Yield engine** — Silence detection, prompt pattern matching, atomic state transitions (`yield_engine/detector.rs`)
31. **Policy engine** — TOML config (`policy/config.rs`), auto_confirm (with mandatory scope), yield_to_operator, forbidden (`policy/matcher.rs`, `policy/scope.rs`). Policy precedence: forbidden → yield_to_operator → auto_confirm → fall through to LLM
32. **Handoff state machine** — State transitions, crypto-random handoff IDs (128-bit), single-use (`handoff/state.rs`)
33. **CLI attach** — Unix domain socket (0600 permissions), `mish attach` command, raw PTY forwarding, single-attachment lock (`handoff/attach.rs`)
34. **Credential-blind handoff return** — No process output shared by default, operator opt-in via `--share-output` (`handoff/summary.rs`)
35. **Handoff edge cases** — Timeout, re-yield, process exit during handoff, concurrent attach rejection
36. **Per-scope timeout configuration** — TOML-based timeout defaults per command scope
37. **Interactive/Dangerous MCP behavior** — Update `handlers/interactive.rs` to return structured error in MCP mode. Update `handlers/dangerous.rs` to route through policy engine in MCP mode.

### Backlog (evidence-driven, not scheduled)

- `/proc/pid/syscall` detection (Linux-only, `#[cfg]` gated) — add if silence heuristic proves insufficient
- CLI proxy watch patterns (`mish --watch="@errors" cargo build`) — add if CLI users request filtering
- TUI snapshot mode — add if LLM agents demonstrate a real need for TUI interaction
- WebSocket terminal — add with proper authentication if browser-based handoff is needed
- SSE transport — add if server-push notifications prove necessary
- Command allowlisting (`policy.allowed`) — add for production/multi-tenant deployments
- Optional OS-level sandboxing (`[sandbox]` config) — add for high-security environments
- Repeat-command diff mode — show what changed vs last run of the same command

---

## 16. Testing Strategy

| Layer | Approach |
|-------|----------|
| Squasher | Unit tests: VTE-based stripping vs raw ANSI, progress detection (CR, cursor-up), Oreo truncation, enriched markers, watch regex + presets, deduplication |
| Boundary detection | Unit tests: PROMPT_COMMAND parsing, OSC 133 sequences, sentinel fallback, exit code extraction, CWD capture |
| UTF-8 decode | Unit tests: partial multi-byte sequences at buffer boundaries, binary detection, BOM handling |
| Yield engine | Integration tests: mock PTY with timed silence + prompt patterns, concurrent tool call + yield race |
| Policy engine | Unit tests: TOML parsing, rule matching, scope filtering (including edge cases: env, sudo, paths), precedence |
| Process table | Unit tests: state transitions, digest modes (full/changed/none), compact stubs, lifecycle, dismiss |
| Resource limits | Unit tests: max sessions, max processes, rejection behavior |
| MCP transport | Integration tests: JSON-RPC over stdio, tool dispatch, response format, error codes |
| Signal handling | Integration tests: SIGINT via PTY `\x03`, SIGTERM/SIGKILL via `killpg`, process group cleanup |
| Handoff | Integration tests: Unix socket communication, single-use IDs, concurrent attach rejection, credential-blind return |
| Audit logging | Unit tests: log format, append-only semantics, fd non-inheritance |
| End-to-end | Integration tests: real commands (ls, echo, npm, pytest) through full pipeline |
| Shutdown | Integration tests: graceful shutdown on SIGTERM, stdin EOF, orphan prevention |

### Test Fixtures

Create a `tests/fixtures/` directory with:
- `ansi_output.txt` — Raw output with ANSI codes, progress bars, color
- `osc_sequences.txt` — OSC hyperlinks, terminal titles, kitty graphics protocol sequences
- `npm_install.txt` — Realistic npm install output (1000+ lines)
- `pytest_output.txt` — Test suite output with failures
- `interactive_prompt.txt` — Output ending with Y/n prompt
- `binary_output.bin` — Non-UTF-8 binary data for binary detection tests
- `cursor_up_progress.txt` — Multi-line progress using cursor-up sequences (cargo-style)

---

## 17. Dependencies (Cargo.toml)

```toml
[package]
name = "mish"
version = "0.1.0"
edition = "2021"

[dependencies]
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
regex = "1"
uuid = { version = "1", features = ["v4"] }
nix = { version = "0.29", features = ["process", "signal", "pty", "fs"] }
vte = "0.13"                       # DEC/ANSI state machine for terminal sequence parsing
clap = { version = "4", features = ["derive"] }  # CLI args
tracing = "0.1"                    # Structured logging
tracing-subscriber = "0.3"
rand = "0.8"                       # Handoff IDs (thread_rng() uses OsRng — crypto-quality)

[dev-dependencies]
assert_cmd = "2"
predicates = "3"
tempfile = "3"
```

---

## 18. File Structure

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full module structure showing both CLI proxy and MCP server code organization with the four-layer architecture (entry points → category router → handlers → shared primitives).

```
mish/
├── Cargo.toml
├── src/
│   ├── main.rs                 # CLI entrypoint: mish serve | mish <command> | mish attach/ps/handoffs
│   ├── lib.rs
│   │
│   ├── cli/                    # Layer 1: CLI proxy entry point
│   │   └── proxy.rs            # Parse command, invoke category router, format terminal output
│   │
│   ├── mcp/                    # Layer 1: MCP server entry point
│   │   ├── transport.rs        # stdio JSON-RPC transport
│   │   ├── types.rs            # MCP request/response types
│   │   └── dispatch.rs         # Tool routing → sh_run/sh_spawn/sh_interact/sh_session/sh_help
│   │
│   ├── tools/                  # MCP tool implementations (call into category router)
│   │   ├── sh_run.rs           # Synchronous execution → category router → response
│   │   ├── sh_spawn.rs         # Background execution → category router → wait_for
│   │   ├── sh_interact.rs      # Process interaction (send, read_tail, signal, kill, status)
│   │   ├── sh_session.rs       # Session lifecycle
│   │   └── sh_help.rs          # Reference card
│   │
│   ├── router/                 # Layer 2: Category router (shared by CLI + MCP)
│   │   ├── mod.rs              # Top-level routing by category
│   │   └── categories.rs       # Command categorization (grammar + TOML mapping + fallback)
│   │
│   ├── handlers/               # Layer 3: Category handlers (mode-aware)
│   │   ├── condense.rs         # Invoke squasher pipeline
│   │   ├── narrate.rs          # File operation narration
│   │   ├── passthrough.rs      # Execute + metadata footer
│   │   ├── structured.rs       # Structured handlers (git, docker, kubectl)
│   │   ├── interactive.rs      # Mode-aware: passthrough (CLI) or error (MCP)
│   │   └── dangerous.rs        # Mode-aware: prompt (CLI) or policy engine (MCP)
│   │
│   ├── squasher/               # Layer 4: Shared primitives — squasher pipeline
│   │   ├── pipeline.rs         # Pipeline orchestration
│   │   ├── vte_strip.rs        # VTE state machine → printable text
│   │   ├── utf8.rs             # Streaming UTF-8 decode with carryover buffer
│   │   ├── progress.rs         # Progress bar detection/removal (CR, cursor-up)
│   │   ├── truncate.rs         # Oreo truncation with enriched markers
│   │   ├── pattern.rs          # Watch regex matching + presets
│   │   └── dedup.rs            # Consecutive line deduplication
│   │
│   ├── core/                   # Layer 4: Shared primitives — other
│   │   ├── pty.rs              # PTY allocation via nix::pty::forkpty(), O_CLOEXEC, TIOCSWINSZ
│   │   ├── line_buffer.rs      # Byte → line assembly
│   │   ├── classifier.rs       # Three-tier classification engine
│   │   ├── emit.rs             # Emit buffer and summary
│   │   ├── grammar.rs          # Grammar loading and matching
│   │   ├── stat.rs             # File stat primitives (shared by narrate + enrich)
│   │   ├── enrich.rs           # Error enrichment on failure
│   │   ├── preflight.rs        # Argument injection (quiet + verbose flags)
│   │   └── format.rs           # Output formatting (human, json, context modes)
│   │
│   ├── session/                # MCP-only: session management
│   │   ├── manager.rs          # Session lifecycle, limits
│   │   ├── shell.rs            # Shell process lifecycle (spawn, PTY wrapper, env tracking)
│   │   └── boundary.rs         # Command boundary detection (PROMPT_COMMAND, sentinel fallback)
│   │
│   ├── process/                # MCP-only: process table
│   │   ├── table.rs            # Global process table + digest (full/changed/none modes)
│   │   ├── state.rs            # Process state machine (atomic transitions)
│   │   └── spool.rs            # Raw output circular buffer (mutex-guarded)
│   │
│   ├── yield_engine/           # MCP-only: yield detection
│   │   ├── detector.rs         # Silence + prompt pattern detection
│   │   └── syscall.rs          # /proc/pid/syscall reader (#[cfg(target_os = "linux")])
│   │
│   ├── policy/                 # MCP-only: policy engine
│   │   ├── config.rs           # TOML parsing + validation
│   │   ├── matcher.rs          # Rule matching engine
│   │   └── scope.rs            # Command scope resolution
│   │
│   ├── handoff/                # MCP-only: operator handoff
│   │   ├── state.rs            # Handoff state machine (single-use IDs, single-attachment)
│   │   ├── attach.rs           # CLI attach via Unix domain socket
│   │   └── summary.rs          # Credential-blind handoff return
│   │
│   ├── audit/                  # MCP-only: audit logging
│   │   └── logger.rs           # Append-only audit log, fd non-inheritance
│   │
│   └── shutdown.rs             # Graceful shutdown, process group cleanup, PID file
│
├── grammars/
│   ├── _meta/
│   │   ├── categories.toml     # command → category mapping
│   │   └── dangerous.toml      # dangerous patterns
│   ├── _shared/
│   │   ├── ansi-progress.toml
│   │   ├── node-stacktrace.toml
│   │   ├── python-traceback.toml
│   │   └── c-compiler-output.toml
│   ├── npm.toml
│   ├── cargo.toml
│   ├── git.toml
│   ├── docker.toml
│   └── make.toml
│
├── tests/
│   ├── fixtures/
│   │   ├── ansi_output.txt
│   │   ├── osc_sequences.txt
│   │   ├── npm_install.txt
│   │   ├── pytest_output.txt
│   │   ├── interactive_prompt.txt
│   │   ├── binary_output.bin
│   │   └── cursor_up_progress.txt
│   ├── squasher_test.rs
│   ├── boundary_test.rs
│   ├── utf8_test.rs
│   ├── yield_test.rs
│   ├── policy_test.rs
│   ├── process_table_test.rs
│   ├── mcp_transport_test.rs
│   ├── handoff_test.rs
│   ├── audit_test.rs
│   └── e2e_test.rs
│
└── docs/
    ├── mish_spec.md            # This document (MCP server mode definitive spec)
    ├── SPEC.md                 # Project overview and quick reference
    ├── ARCHITECTURE.md         # Unified execution model (both modes)
    ├── PROXY.md                # CLI proxy mode and category system
    └── ...                     # Companion subsystem docs
```

---

## 19. Prior Art & Differentiation

| Project | What It Does | What mish Adds |
|---------|-------------|-----------------|
| lightos/interactive-shell-mcp | Multi-session PTY over MCP, snapshot mode | Squasher, watch/wait, policy, handoff, process table digest |
| rtk | CLI proxy, rule-based output filtering | LLM-specified per-command filtering, async, handoff |
| Claude Code bash tool | subprocess.run() with raw output | Structured output, 50-90% token reduction, concurrent processes, operator handoff |
| Goose shell extension | Agent-level command execution | Structured output, concurrency, temporal control, handoff |
| butterfish / shell_gpt | LLM assists human in shell | Inverse: shell assists LLM in OS interaction |

**Positioning risk:** ANSI stripping and output truncation are easy wins that agent frameworks could add in a day. The moat is the complete system: squasher + background process management + yield detection + operator handoff. Speed to Phase 3 matters — the full system is hard to replicate piecemeal.

**The pitch:** mish cuts 50-90% of shell output tokens by squashing noise before it reaches your LLM, while giving it structured process state and human-in-the-loop for authentication.

---

## 20. Success Criteria

**Phase 1 (CLI proxy) is successful when:**
- `mish npm install` returns ~3 lines instead of ~1400 (condense category)
- `mish cp file backup/` returns `"→ cp: file → backup/ (4.2KB)"` (narrate category)
- `mish cat config.json` passes through verbatim with metadata footer (passthrough category)
- `mish git status` returns structured summary (structured category)
- `mish vim file` detects interactivity and passes through transparently (interactive category)
- `mish rm -rf node_modules` warns with size/count before executing (dangerous category)
- Error enrichment on failure provides diagnostic context (path walks, permissions, similar branches)
- All 5 MVP grammars (npm, cargo, git, docker, make) have test fixtures from real captured output
- PTY stress test passes on macOS (ANSI, progress bars, SIGWINCH, raw mode, UTF-8)

**Phase 2 (MCP server) is successful when:**
- An LLM agent (Claude Code, Cursor, etc.) can install mish as an MCP server
- `sh_run(cmd="npm install", watch:"@npm", unmatched:"drop")` returns ~200 tokens instead of ~5000
- The process table digest appears on every response (with `changed` mode as default)
- sh_spawn + sh_interact allow background process management with alias tracking

**The project is successful when:**
- The end-to-end terraform deployment scenario works without human intervention except for MFA
- Operator handoff allows a human to enter MFA credentials and the LLM seamlessly resumes
- An agent can run 5 concurrent processes and reason about their state via the digest
- Token consumption for typical agent workflows drops 50-90% vs raw shell output
