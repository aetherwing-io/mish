# mish Audit Log Specification

mish emits a structured, append-only audit log for every command execution and
session lifecycle event. The log serves integration debugging, evaluation
metrics, squasher quality review, and safety auditing from a single data
source.

Audit logging is always on. Generation is unconditional; retention and access
are configurable.

## Storage Layout

```
$MISH_AUDIT_DIR/               # default: ~/.local/share/mish/audit/
├── s_7a3f.jsonl               # one file per session, append-only
├── s_7a3f/                    # raw output sidecar directory
│   ├── 001.raw.zst            # zstd-compressed raw stdout+stderr, per seq
│   ├── 002.raw.zst
│   └── ...
├── s_b2e1.jsonl
└── s_b2e1/
    └── ...
```

- `$MISH_AUDIT_DIR` defaults to `~/.local/share/mish/audit/`
- One JSONL file per session: `{session_id}.jsonl`
- Raw output stored in `{session_id}/{seq:03}.raw.zst` (zstd-compressed)
- Raw sidecar referenced by SHA-256 hash from the JSONL record

## Record Types

### `session_start`

Emitted once when a session is created.

```jsonl
{
  "type": "session_start",
  "ts": "2026-03-03T02:14:30.000Z",
  "sid": "s_7a3f",
  "shell": "/bin/bash",
  "cwd": "/home/scott/project",
  "env_baseline": {
    "PATH": "/usr/local/bin:/usr/bin:...",
    "HOME": "/home/scott",
    "SHELL": "/bin/bash"
  },
  "env_baseline_sha256": "d4e5f6...",
  "pid": 12345,
  "mode": "mcp",
  "mish_version": "0.1.0"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `shell` | string | Shell binary path |
| `cwd` | string | Initial working directory |
| `env_baseline` | object | Full environment at session creation |
| `env_baseline_sha256` | string | SHA-256 of serialized baseline (for integrity) |
| `pid` | u32 | Shell process PID |
| `mode` | string | `"mcp"` or `"cli"` |
| `mish_version` | string | mish binary version |

### `command`

Emitted on every `sh_run` and `sh_spawn` execution.

```jsonl
{
  "type": "command",
  "ts": "2026-03-03T02:14:33.012Z",
  "sid": "s_7a3f",
  "seq": 14,
  "tool": "sh_run",
  "cmd": "pytest tests/ -v",
  "cwd": "/home/scott/project",
  "env_delta": {
    "VIRTUAL_ENV": "/home/scott/.venv"
  },
  "category": "Condense",
  "grammar": "pytest",
  "exit_code": 1,
  "wall_ms": 4230,
  "raw": {
    "stdout_lines": 347,
    "stderr_lines": 8,
    "bytes": 15252,
    "sha256": "a1b2c3..."
  },
  "squash": {
    "output": "FAILED 2/47 tests\n  test_auth.py::test_login - AssertionError\n  test_auth.py::test_refresh - KeyError: 'token'\n45 passed",
    "lines": 4,
    "bytes": 142,
    "ratio": 0.009
  },
  "rules": {
    "noise": 312,
    "signal": 27,
    "outcome": 3,
    "unclassified": 5,
    "dedup_groups": 4,
    "oreo_suppressed": 0,
    "conflicts": []
  },
  "safety": {
    "dangerous_match": null,
    "action": "allow"
  },
  "digest": "procs: 1 running (pid 8842 npm start) | cwd: /home/scott/project"
}
```

#### Field Reference

**Identity & ordering:**

| Field | Type | Description |
|-------|------|-------------|
| `seq` | u32 | Monotonic counter per session. Reconstructs exact command order regardless of timestamp precision. |
| `tool` | string | `"sh_run"`, `"sh_spawn"`, or `"sh_interact"` |
| `cmd` | string | Exact command string as received |
| `cwd` | string | Working directory at time of execution |

**Environment:**

| Field | Type | Description |
|-------|------|-------------|
| `env_delta` | object | Environment variables that differ from session baseline. Not full env — only what changed (activated venvs, modified PATHs, user-set vars). |

**Classification:**

| Field | Type | Description |
|-------|------|-------------|
| `category` | string | Router classification: `"Dangerous"`, `"Interactive"`, `"Condense"`, `"Passthrough"` |
| `grammar` | string \| null | Matched TOML grammar name (e.g., `"pytest"`, `"pip"`, `"npm"`), or `null` if default heuristics applied |

**Execution:**

| Field | Type | Description |
|-------|------|-------------|
| `exit_code` | i32 \| null | Process exit code. `null` if killed by signal or timeout. |
| `wall_ms` | u64 | Wall-clock execution time in milliseconds |

**Raw output:**

| Field | Type | Description |
|-------|------|-------------|
| `raw.stdout_lines` | u32 | Line count of raw stdout |
| `raw.stderr_lines` | u32 | Line count of raw stderr |
| `raw.bytes` | u64 | Total raw output bytes (stdout + stderr) |
| `raw.sha256` | string | SHA-256 of raw output. Proves the squashed output corresponds to a specific raw output. Sidecar file `{sid}/{seq:03}.raw.zst` contains the full content. |

**Squashed output:**

| Field | Type | Description |
|-------|------|-------------|
| `squash.output` | string | The actual text the agent received. Inlined — not a reference — because this is the primary debugging and review target. |
| `squash.lines` | u32 | Line count of squashed output |
| `squash.bytes` | u64 | Byte count of squashed output |
| `squash.ratio` | f64 | `squash.bytes / raw.bytes`. Lower = more aggressive compression. |

**Rule application (squasher transparency):**

| Field | Type | Description |
|-------|------|-------------|
| `rules.noise` | u32 | Lines matched by noise/strip patterns |
| `rules.signal` | u32 | Lines matched by signal/keep patterns |
| `rules.outcome` | u32 | Lines matched by outcome/promote patterns |
| `rules.unclassified` | u32 | Lines that fell through to default heuristic |
| `rules.dedup_groups` | u32 | Number of template deduplication groups (e.g., "Downloading x10") |
| `rules.oreo_suppressed` | u32 | Lines removed by oreo truncation (budget exceeded) |
| `rules.conflicts` | array | Lines matching both keep and strip patterns. See [Conflict Records](#conflict-records). |

**Safety:**

| Field | Type | Description |
|-------|------|-------------|
| `safety.dangerous_match` | string \| null | Pattern that triggered dangerous check, or `null` |
| `safety.action` | string | `"allow"`, `"block"`, or `"prompt"` (CLI mode only) |

**Ambient state:**

| Field | Type | Description |
|-------|------|-------------|
| `digest` | string | Process table digest appended to the agent response |

### `session_end`

Emitted when a session is closed or the mish process exits.

```jsonl
{
  "type": "session_end",
  "ts": "2026-03-03T03:41:12.000Z",
  "sid": "s_7a3f",
  "seq_final": 47,
  "commands_run": 47,
  "total_raw_bytes": 892331,
  "total_squashed_bytes": 14220,
  "aggregate_ratio": 0.016,
  "dangerous_hits": 0,
  "dangerous_blocks": 0,
  "grammars_used": ["pytest", "pip", "git"],
  "duration_s": 5202
}
```

The `session_end` record provides per-session aggregates suitable for paper
metrics and pitch decks without post-processing.

## Conflict Records

When a line matches both a keep pattern and a strip pattern, log the
resolution:

```json
{
  "line": 47,
  "text": "PASSED tests/test_utils.py::test_edge_case",
  "keep_rule": "outcome.pass",
  "strip_rule": "noise.test_line",
  "resolution": "keep"
}
```

These are almost always empty. When they aren't, they're the squasher
credibility data — proof that ambiguous classification was resolved correctly.

## Configuration

```toml
# ~/.config/mish/config.toml

[audit]
# Directory for audit logs and raw sidecar files
dir = "~/.local/share/mish/audit"

# Raw output retention. Compressed sidecar files older than this are deleted.
# The JSONL records (with hashes) are retained indefinitely.
# Values: "none" (no raw storage), "24h", "7d", "30d", "forever"
raw_retention = "7d"

# Whether to inline squash.output in the JSONL record.
# true  = squashed text in every record (default, best for debugging)
# false = squashed text omitted, only metrics retained (lower disk usage)
inline_squash_output = true
```

## Querying

The JSONL format is designed for `jq` workflows:

```bash
# Compression ratios for all commands in a session
jq '.squash.ratio' s_7a3f.jsonl

# All pytest executions across all sessions
jq 'select(.grammar == "pytest")' *.jsonl

# Aggressive squashes to manually review (ratio < 2%)
jq 'select(.type == "command" and .squash.ratio < 0.02)' *.jsonl

# Every dangerous-check hit
jq 'select(.safety.dangerous_match != null)' *.jsonl

# Commands where signal may have been lost (conflicts present)
jq 'select(.rules.conflicts | length > 0)' *.jsonl

# Session-level aggregates (paper metrics)
jq 'select(.type == "session_end")' *.jsonl

# Token-level analysis (requires tokenizer, but bytes are a proxy)
jq '[select(.type == "command") | .raw.bytes] | add' s_7a3f.jsonl
jq '[select(.type == "command") | .squash.bytes] | add' s_7a3f.jsonl

# Per-grammar compression stats
jq -s 'map(select(.type == "command")) | group_by(.grammar) |
  map({grammar: .[0].grammar, count: length,
       avg_ratio: (map(.squash.ratio) | add / length)})' s_7a3f.jsonl
```

## Programmatic Access

Audit data is also accessible via MCP tools:

```
sh_session("audit")                    # Full JSONL for current session
sh_session("audit", last=5)            # Last 5 command records
sh_session("audit", format="summary")  # Session-end-style aggregate for in-progress session
```

## Implementation Notes

- **Always-on.** The audit log is written unconditionally. Every field in the
  `command` record is already computed during normal execution (category
  classification, grammar matching, squasher pipeline counts, safety check).
  The only added cost is serialization and a file write.

- **Append-only.** Records are appended via `O_APPEND` writes. No locking
  required for single-writer (one session = one writer). Safe across
  `sh_run` and `sh_spawn` because each has its own `seq`.

- **Raw sidecar is optional.** If `raw_retention = "none"`, the `raw.sha256`
  field still proves what output existed. The hash chain is the audit trail;
  the sidecar is the evidence locker.

- **`env_delta` not full env.** Full environment is 200+ variables of noise.
  Delta from `env_baseline` captures what actually changed — activated venvs,
  modified PATHs, user-set vars. Baseline is logged once at `session_start`.

- **`squash.output` is inlined.** During debugging you'll diff this against
  raw output constantly. During paper writing you'll use it for quality review.
  The convenience of having it in the record outweighs the disk cost. Can be
  disabled via config.

- **`seq` for ordering.** Monotonic counter per session. Reconstructs exact
  command sequence without relying on timestamp precision. Timestamps are for
  wall-clock correlation; `seq` is for causal ordering.
