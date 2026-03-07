# mish as Agent Microkernel

**Date:** 2026-03-06
**Status:** Design proposal
**Authors:** Scott Meyer, Claude (inner-agent dogfooding session), Gemini (async review pending)

## One-sentence pitch

mish is a microkernel for AI agents — process table, coordinated filesystem, and IPC in a single binary, with the orchestrator as the scheduler.

## Background

mish started as an LLM-native shell: a CLI proxy and MCP server for process supervision. Slipstream started as a separate MCP server for file editing. In practice, agents use both simultaneously — every tool call goes through mish for shell I/O and slipstream for file ops.

Running them as separate systems creates coordination gaps:

- **File clobbering**: parallel agents edit overlapping files with no conflict detection. Discovered during worktree experiments where agents editing `types.rs` clobbered each other on copy-back.
- **No ambient file awareness**: agents re-read files they just edited (to verify), wasting tokens. mish's process table gives ambient compute awareness; no equivalent exists for files.
- **Human-as-clipboard**: in multi-agent sessions, the human manually relays messages between agents running in separate PTYs. No inter-agent communication primitive exists.

This design absorbs slipstream into mish and extends the architecture into a microkernel for multi-agent development.

## Architecture: Unix kernel mapping

```
Unix kernel          →  mish
────────────────────────────────────
Process table        →  [procs] digest (exists today)
Filesystem + inodes  →  [files] digest with generation counters
IPC / signals        →  [msgs] send/receive/broadcast
File descriptors     →  MCP connection IDs
PIDs                 →  agent aliases (cc, gem, ...)
open() / read()      →  ss_session("read <path>")
write() / fsync()    →  ss(path, old_str, new_str) + flush
kill() / signal()    →  sh_interact(action="send_signal")
fork() / exec()      →  sh_spawn
Permission model     →  policy engine (mish.toml)
Scheduler            →  Teams (external orchestrator)
```

**Microkernel, not monolithic.** mish provides primitives — process management, file coordination, message passing. It does not parse code, resolve semantic conflicts, or make scheduling decisions. Agents and the orchestrator (Teams) operate in userspace.

## 1. Absorption model

### Single binary, two tool families

Slipstream's file operations are rewritten in Rust and merged into the mish binary. The API surface stays separated into two tool families:

| Family | Tools | Domain |
|--------|-------|--------|
| `sh_*` | sh_run, sh_spawn, sh_interact, sh_session, sh_help | Process supervision |
| `ss_*` | ss, ss_session, ss_help | File coordination |
| `sh_msg` | sh_msg (new) | Inter-agent messaging |

One process, one MCP connection per agent, shared internal state (process table, file table, message queues).

### Why absorb, not bridge

- **Atomic coordination**: one process owns all state. No IPC between mish and slipstream, no race conditions at the boundary.
- **Shared identity**: a single MCP connection = one agent. File edits and process spawns are attributed to the same identity.
- **Single digest**: process, file, and message awareness in one response. No need to query two servers.

## 2. File coordination: optimistic concurrency control

### str_replace is already a compare-and-swap

Slipstream's edit primitive — `ss(path, old_str="foo", new_str="bar")` — is accidentally an OCC token. If `old_str` no longer exists in the file (because another agent changed it), the edit fails. The match string IS the compare-and-swap.

This catches textual conflicts with zero additional machinery.

### Three-layer conflict model

| Layer | Mechanism | Catches |
|-------|-----------|---------|
| **CAS (str_replace)** | old_str must match current file content | Overlapping edits to same text |
| **Generation counter** | Per-file monotonic counter, bumped on every write | Non-overlapping concurrent edits (same file, different regions) |
| **Tests** | Agent-driven, external to mish | Semantic incompatibility (rename breaks call site) |

mish handles layers 1 and 2. Layer 3 is the agent's responsibility. mish is infrastructure, not a compiler.

### Generation counter semantics

Every file touched through `ss_*` gets a generation counter. On edit:

1. Generation increments
2. Editor identity and timestamp recorded
3. If the editing agent's last-read generation is stale, the response includes a warning:

```
⚠ src/main.rs modified since your read (gen 5→7, by agent-gem 10s ago)
  diff: -fn old_name() +fn new_name()
  Your edit succeeded — verify semantic compatibility.
```

The agent decides whether to re-read, abort, or proceed. mish informs, doesn't block.

### HTTP caching semantics

The generation counter maps directly to well-understood HTTP caching:

```
HTTP                    →  mish
──────────────────────────────────────
ETag: "gen=7"           →  src/main.rs:gen=7
Last-Modified: 5m ago   →  :cc:5m
If-None-Match: gen=5    →  agent reads at gen=5, file now gen=7 → stale
304 Not Modified        →  "you have latest, gen=7" (read elision)
Vary: Agent             →  per-agent staleness tracking
```

**Read elision (304 Not Modified)**: mish tracks each agent's read high-water mark per file. If an agent requests a file it already has at the current generation, mish returns a 5-token confirmation instead of the file contents. Over a session with dozens of reads, this saves thousands of tokens.

## 3. Triple digest

Every tool response includes an ambient status digest. Three channels:

```
[procs] cc:running:85m gem:running:52s build:exit(0):2m
[files] src/main.rs:gen=7:cc:2m src/lib.rs:gen=3:gem:30s
[msgs] 1 unread from gem: "[CLOSED] bd-456 — OCC implementation"
```

### Adaptive suppression (priority tiers)

The digest scales with activity, not total state:

| Tier | Channel | Source | Show when | Rationale |
|------|---------|--------|-----------|-----------|
| 1 (always) | `[procs]` | mish-native | Every response | Compute awareness is always relevant |
| 2 (if stale) | `[files]` | mish-native | File modified since agent's last read | No news = no tokens |
| 3 (if unread) | `[mail]` | agentmail peek | Unread messages exist | Absent if agentmail not configured |

**Token budget**: ~60-100 tokens when active, ~20-40 tokens when quiet, 200-token ceiling. Overflow collapses to summary: `[msgs] 7 unread from gem (oldest: 45s)`.

Cost scales with information density. A quiet system costs the same as today's process-only digest.

## 4. FCP ecosystem: three independent servers

### Topology

```
                    Teams (scheduler)
                        │
              ┌─────────┼─────────┐
              │         │         │
          mish serve  agentmail  fcp-*
        (compute+state) (comms) (code intel)
              │         │         │
              └─────────┼─────────┘
                        │
                  Agent (cc, gem)
```

Each server has exactly one job:

| Server | Concern | Provides |
|--------|---------|----------|
| **mish serve** | Compute + state | Shell, files, OCC, process/file digest |
| **agentmail serve** | Communication | Inbox, outbox, broadcast, catch-up |
| **fcp-*** | Code intelligence | Definitions, references, diagnostics (rust-analyzer, pylsp, etc.) |

No overlap. Agents compose what they need — shell-only agents connect to mish alone, multi-agent teams add agentmail, code-heavy work adds fcp-rust. Slipstream's file editing is absorbed into mish; its FCP plugin system continues as independent `fcp-*` servers.

### Agentmail as standalone peer FCP

Agentmail is NOT a slipstream plugin or a mish subsystem. It's an independent MCP server at the same level as mish. This matters because:

- Agents that don't use mish (e.g., Gemini CLI on PATH) can still connect to agentmail for messaging
- Agentmail can evolve independently — new message types, persistence backends, protocols
- The communication concern is cleanly separated from the compute/state concern

### mish as agentmail client

mish peeks at the agent's agentmail inbox and surfaces unread count in the digest — like a Unix shell showing `You have new mail` by checking `/var/mail`. mish doesn't own messages, it reads the inbox count as a convenience:

```
[procs] cc:running:85m gem:running:52s
[files] src/main.rs:gen=7:cc:2m
[mail] 2 unread (via agentmail)
```

Configuration: mish.toml points to the agentmail endpoint. If agentmail isn't running, tier 3 is simply absent.

### Message semantics (agentmail-owned)

- **Ordering**: latest-N, no sequence numbers. Timestamps provide implicit ordering.
- **Persistence**: messages persist until delivered. Auto-cleared after acknowledgment.
- **Catch-up**: agent reconnects after crash/disconnect/compaction → receives all messages since last seen. Same as Slack scrollback.
- **Compaction broadcast**: when an agent's context is compacted, agentmail broadcasts a compaction event so peers can compensate:
  ```
  [COMPACTED] kern — lost fine-grained context, retaining summary.
    files read: src/squasher/pipeline.rs (gen=8), src/handlers/condense.rs (gen=3)
    last topic: microkernel design doc, FCP ecosystem topology
  ```
  Peers seeing this know to provide richer context in subsequent messages rather than assuming shared history. The orchestrator (Mux) can also trigger a delta recovery via mish's file digest — "here's what changed since your last coherent state."

## 5. Identity model

### MCP connection = agent identity

```
MCP connection (fd=7) → alias "cc" → agent name "claude"
MCP connection (fd=9) → alias "gem" → agent name "gemini"
```

One connection = one agent = one set of:
- File read high-water marks (per-file generation seen)
- Unread message queue
- Process table entries
- Edit attribution history

No registration ceremony, no auth tokens. Connect to mish, get an identity. Disconnect, state persists for catch-up on reconnect. Alias is the human-readable name; connection ID is the internal key.

### Identity survives agent restart

If Gemini's PTY dies and respawns with alias "gem", mish resumes the identity: "welcome back — 4 file changes, 2 messages since your disconnect." Generation counters and message queues are keyed to alias, not connection fd.

## 6. Context recovery (compaction deltas)

Every LLM agent eventually loses context (compaction, crash, token limit). mish can generate a delta:

```
Since your last coherent state (gen=5):
  [files] pipeline.rs: gen 5→8 (3 edits by gem, cc)
  [msgs] 2 unread: gem "[CLOSED] bd-456", cc "design doc ready"
  [procs] build:exit(0) server:running:12m
```

This is the kernel providing a "session resume" primitive. Per-agent high-water marks make it possible — mish knows what each agent last saw and can compute the delta. Persistent memory through infrastructure rather than through the model.

## 7. State persistence

In-memory state needs durability across mish restarts:

| State | Persistence | Rationale |
|-------|-------------|-----------|
| Process table | Reconstructed (processes survive or don't) | Ephemeral by nature |
| File generations | Persisted to `~/.local/share/mish/state.json` | Must survive restart |
| Message queues | Persisted alongside generations | Undelivered messages can't be lost |
| Agent high-water marks | Persisted per-alias | Required for read elision and catch-up |

Lightweight: fsync on generation bump, not on every read. State file is small — one entry per tracked file, one queue per agent.

### Scope boundary

mish tracks only files touched through `ss_*` tools. If an agent bypasses slipstream (`cat > file.py << 'EOF'`), mish doesn't know. This is acceptable — the agent opted out of coordination. Same as a Unix process writing directly to `/dev/` bypassing the filesystem.

## 8. Teams integration

Teams is the scheduler that sits above the microkernel. Clean separation:

| Concern | Owner |
|---------|-------|
| "Who works on what" | Teams (scheduling) |
| "What does the world look like right now" | mish (awareness) |
| "Execute this command / edit this file" | mish (primitives) |
| "Coordinate with other agents" | mish (IPC) |
| "Make design decisions" | Agents (userspace) |

Teams invokes mish primitives. mish doesn't know about tasks, phases, or beads. It provides the substrate; Teams provides the intelligence.

## Open questions

1. **Permission model**: should mish enforce per-agent file permissions (agent-cc can only edit files in track-A), or is that Teams' responsibility? Leaning toward Teams — mish provides the `Vary: Agent` tracking, Teams decides policy.

2. **Digest format**: the current `[procs] cc:running:85m` format is ad-hoc. Should the digest be structured (JSON) for machine parsing, or stay human-readable for token efficiency? Leaning human-readable with a `--json` flag for programmatic consumers.

3. **Slipstream migration path**: the absorption requires rewriting slipstream's Python in Rust. Phased approach? Start with file coordination (gen counters, digest) as a mish-native layer, keep slipstream running alongside for the actual str_replace engine, then absorb fully.

4. **Scale limits**: how many files and agents before the digest becomes unwieldy? Probably fine for 5-10 agents and 50-100 active files. Beyond that, the adaptive suppression tiers become critical.

## Summary

mish evolves from "LLM-native shell" to "agent microkernel":

- **Process table** for compute awareness (exists today)
- **Filesystem with OCC** for state coordination (str_replace CAS + generation counters)
- **Triple digest** for ambient awareness (process + file + agentmail peek, adaptively suppressed)
- **Identity** via MCP connection, surviving reconnects
- **Context recovery** via per-agent deltas
- **Three-server ecosystem**: mish (compute+state), agentmail (communication), fcp-* (code intelligence)
- **Teams as scheduler** sitting cleanly above all three

One kernel for compute and state. One mail server for communication. Composed freely. Zero blocking.
