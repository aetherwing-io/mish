# mish Decomposition Prompt

> Use this prompt to start the decomposition session. Run `/decompose` after reading this.

---

## What You're Building

**mish** is a Rust binary — an LLM-native shell that wraps every command with structured, context-efficient output. One binary, two interfaces:

- **CLI proxy** (`mish <command>`) — wraps individual commands with category-aware output
- **MCP server** (`mish serve`) — full process supervisor over JSON-RPC with PTY session management

Both share a common core: category router, squasher pipeline, grammar system, and shared primitives.

**Status:** Design/spec phase complete. No source code exists. All work is in `docs/`.

---

## The Spec Files (Read These)

| Doc | What It Owns | Read Order |
|-----|-------------|------------|
| `docs/ARCHITECTURE.md` | **Start here.** Authoritative unified execution model. Four-layer ASCII diagram, module structure, all file paths. | 1st |
| `docs/mish_spec.md` **§15** | **The build plan.** 37 steps across 3 phases. This is what you're decomposing. | 2nd |
| `docs/mish_spec.md` **§1-6** | MCP server overview, design principles, tech stack, squasher pipeline detail | 3rd |
| `docs/PROXY.md` | Category system (condense, narrate, passthrough, structured, interactive, dangerous), category resolution order | As needed |
| `docs/GRAMMARS.md` | Grammar schema, TOML format, dialect system, tool grammar specs | As needed |
| `docs/CLASSIFIER.md` | Three-tier classification engine (grammar rules → universal patterns → structural heuristics) | As needed |
| `docs/STREAMING.md` | Streaming buffer, PTY capture, line buffer byte-level rules | As needed |
| `docs/DEDUP.md` | Deduplication engine (edit-distance, template extraction) | As needed |
| `docs/PREFLIGHT.md` | Argument injection — quiet flag injection, verbose flag injection | As needed |
| `docs/VERBOSITY.md` | Verbose flag injection detail, file stat enrichment (success path) | As needed |
| `docs/ENRICH.md` | Error enrichment on failure — path resolution, permissions, command-not-found | As needed |
| `docs/SPEC.md` | Quick reference / overview (lightweight, references others) | Optional |
| `docs/mish_spec.md` full | Remaining MCP sections: process table, yield engine, policy, handoff, etc. | Phase 2-3 |

### How the docs relate

```
ARCHITECTURE.md ← authoritative execution model (4 layers, module map)
     │
     ├── PROXY.md ← category system detail (shared by both modes)
     │     ├── GRAMMARS.md ← grammar schema (TOML rules for known tools)
     │     ├── CLASSIFIER.md ← classification engine (3 tiers)
     │     ├── DEDUP.md ← dedup engine
     │     ├── PREFLIGHT.md ← argument injection
     │     ├── VERBOSITY.md ← verbose injection + stat enrichment
     │     └── ENRICH.md ← error enrichment on failure
     │
     ├── STREAMING.md ← PTY capture + line buffer
     │
     └── mish_spec.md ← MCP server (process table, yield, policy, handoff)
           └── §15 Build Plan ← THE PLAN TO DECOMPOSE
```

---

## The Build Plan (What You're Decomposing)

### Phase 1: Shared Core + CLI Proxy (Steps 1-15)

**Goal:** `mish <command>` works end-to-end with all 6 categories. 5 grammars with test fixtures.

**Critical gate at Step 2:** PTY stress test on macOS. If PTY proxying fails, STOP. Do not heroically fix. Escalate. The project may pivot to a real shell implementation.

| Step | What | Key Files | Depends On |
|------|------|-----------|------------|
| 1 | Project scaffold | `Cargo.toml`, `main.rs`, `lib.rs` | — |
| 2 | **PTY + Line Buffer (GATE)** | `core/pty.rs`, `core/line_buffer.rs` | 1 |
| 3 | Grammar system + categories | `core/grammar.rs`, `router/categories.rs` | 1 |
| 4 | Squasher pipeline (7 files) | `squasher/*.rs` | 2 |
| 5 | Classifier + emit buffer | `core/classifier.rs`, `core/emit.rs` | 3, 4 |
| 6 | Condense handler | `handlers/condense.rs` | 5 |
| 7 | Stat primitives + narrate handler | `core/stat.rs`, `handlers/narrate.rs` | 2, 3 |
| 8 | Passthrough + structured handlers | `handlers/passthrough.rs`, `handlers/structured.rs` | 2, 3 |
| 9 | Interactive + dangerous handlers | `handlers/interactive.rs`, `handlers/dangerous.rs` | 2, 3 |
| 10 | Error enrichment | `core/enrich.rs` | 7 (stat.rs) |
| 11 | Preflight (argument injection) | `core/preflight.rs` | 3 (grammars) |
| 12 | Category router | `router/mod.rs` | 6, 7, 8, 9 |
| 13 | CLI proxy + format | `cli/proxy.rs`, `core/format.rs` | 12 |
| 14 | 5 grammars + test fixtures | `grammars/*.toml`, `tests/fixtures/*` | 3, 5 |
| 15 | Integration tests | `tests/` | 13, 14 |

**Dependency highlights:**
- Steps 2 and 3 can start in parallel after step 1
- Steps 4 depends on 2 (PTY); steps 5 depends on 3+4
- Steps 7, 8, 9 can be parallel (all need 2+3)
- Step 12 (router) is the convergence point — needs all handlers
- Step 13 (CLI proxy) is the final wiring

### Phase 2: MCP Server (Steps 16-29)

**Goal:** `mish serve` works as MCP server. sh_run, sh_spawn, sh_interact functional. Process table digest on every response.

| Step | What | Key Files | Depends On |
|------|------|-----------|------------|
| 16 | MCP stdio transport | `mcp/transport.rs`, `mcp/types.rs`, `mcp/dispatch.rs` | Phase 1 |
| 17 | Session manager | `session/manager.rs`, `session/shell.rs` | 16 |
| 18 | Command boundary detection | `session/boundary.rs` | 17 |
| 19 | sh_run tool | `tools/sh_run.rs` | 16, 12 (router) |
| 20 | Process table | `process/table.rs`, `process/state.rs` | 17 |
| 21 | sh_spawn tool | `tools/sh_spawn.rs` | 19, 20 |
| 22 | sh_interact tool | `tools/sh_interact.rs` | 20 |
| 23 | Raw output spool | `process/spool.rs` | 20 |
| 24 | Multiple sessions | `tools/sh_session.rs` | 17, 20 |
| 25 | sh_help tool | `tools/sh_help.rs` | 19 |
| 26 | Basic safety | (compiled deny-list, limits) | 16 |
| 27 | Audit logging | `audit/logger.rs` | 16 |
| 28 | Graceful shutdown | `shutdown.rs` | 17, 20 |
| 29 | MCP integration tests | `tests/` | 19-28 |

### Phase 3: Intelligence & Operator Handoff (Steps 30-37)

**Goal:** Yield detection, policy engine, operator handoff, MCP-mode behavior for interactive/dangerous.

| Step | What | Key Files | Depends On |
|------|------|-----------|------------|
| 30 | Yield engine | `yield_engine/detector.rs` | Phase 2 |
| 31 | Policy engine | `policy/config.rs`, `policy/matcher.rs`, `policy/scope.rs` | Phase 2 |
| 32 | Handoff state machine | `handoff/state.rs` | 30, 31 |
| 33 | CLI attach | `handoff/attach.rs` | 32 |
| 34 | Credential-blind return | `handoff/summary.rs` | 32 |
| 35 | Handoff edge cases | (tests for 32-34) | 33, 34 |
| 36 | Per-scope timeout config | (policy extension) | 31 |
| 37 | Interactive/Dangerous MCP mode | `handlers/interactive.rs`, `handlers/dangerous.rs` | 31 |

---

## Execution Model: Agent Teams

This implementation will be executed by **agent teams**, not a human developer. Structure the decomposition accordingly:

1. **Maximize parallelism.** Identify which steps/sub-beads can be worked on concurrently by independent agents. The dependency table above shows the critical path — everything else is parallelizable.

2. **Each bead must be standalone.** An agent picks up a bead and implements it without needing to ask questions or read other beads. Include full context: file paths, struct signatures from upstream dependencies, spec references, test requirements.

3. **Phase gates matter.** Phase 1 must be complete and tested before Phase 2 starts. Phase 2 before Phase 3. Within each phase, respect the dependency ordering.

4. **PTY gate is non-negotiable.** Step 2 is a validation gate. If it fails, the entire project may pivot. Make this the very first bead executed after scaffolding.

5. **Grammars are data, not code.** The 5 grammar TOML files (step 14) can be authored in parallel with the code that consumes them, as long as the grammar schema (step 3) is settled first.

6. **Tests live with their code.** Each implementation bead should include unit tests. Step 15 (integration tests) is a separate bead that runs the full pipeline.

---

## PTY Platform Risk — Pivot Directive

From ARCHITECTURE.md:

> If PTY proxying proves unreliable on macOS, **stop and report back immediately.** Do not attempt heroic workarounds. The project owner wants the pivot signal — if PTY is not the answer, the project may pivot to a real shell implementation. This is a strategic decision, not an implementation detail. Escalate, don't fix.

Structure the PTY stress test (step 2) as the first bead to execute after scaffolding. Block everything else on it.

---

## Tech Stack

Rust, tokio, `vte` crate (ANSI parsing), `nix` crate (PTY via `forkpty`), TOML config, `clap` for CLI, `serde`/`serde_json` for serialization, `regex` for pattern matching. Full dependency list in `mish_spec.md` §17.

---

## How to Proceed

1. Read `docs/ARCHITECTURE.md` (unified execution model, module structure)
2. Read `docs/mish_spec.md` §15 (the build plan above in full detail)
3. Run `/decompose` to break Phase 1 into beads first
4. After Phase 1 beads are validated, decompose Phase 2, then Phase 3
5. Execute via agent teams with parallelism matching the dependency graph
