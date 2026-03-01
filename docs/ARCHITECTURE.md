# Architecture

## Execution Model

mish is one binary with two interfaces sharing a common core. Both modes use the same category router, squasher pipeline, grammar system, and shared primitives.

```
┌─────────────────────────────────────────────────────────────────────┐
│                           mish binary                               │
│                                                                     │
│  Layer 1: Entry Points                                              │
│  ┌─────────────────────┐              ┌───────────────────────────┐ │
│  │   CLI Proxy Mode    │              │    MCP Server Mode        │ │
│  │   mish <command>    │              │    mish serve             │ │
│  │                     │              │                           │ │
│  │   Parse command,    │              │   JSON-RPC over stdio     │ │
│  │   enter router      │              │   sh_run  → router        │ │
│  │                     │              │   sh_spawn → router       │ │
│  │                     │              │   sh_interact             │ │
│  │                     │              │   sh_session              │ │
│  │                     │              │   sh_help                 │ │
│  └──────────┬──────────┘              └─────────────┬─────────────┘ │
│             │                                       │               │
│  Layer 2: Category Router (shared)                  │               │
│  ┌──────────▼───────────────────────────────────────▼─────────────┐ │
│  │                     Category Router                            │ │
│  │   command → grammar lookup → categorize → dispatch to handler  │ │
│  │                                                                │ │
│  │   Condense · Narrate · Passthrough · Structured ·              │ │
│  │   Interactive · Dangerous                                      │ │
│  └──────┬──────────┬──────────┬──────────┬──────────┬─────────┬──┘ │
│         │          │          │          │          │         │     │
│  Layer 3: Category Handlers (mode-aware)                           │
│  ┌──────▼───┐ ┌───▼────┐ ┌──▼──────┐ ┌─▼────────┐ ┌▼─────┐ ┌▼──┐ │
│  │ Condense │ │Narrate │ │Passthru │ │Structured│ │Inter │ │Dng│ │
│  │          │ │        │ │         │ │          │ │active│ │   │ │
│  │ squasher │ │inspect │ │execute  │ │execute   │ │      │ │   │ │
│  │ pipeline │ │execute │ │+ meta   │ │+ parse   │ │(mode │ │(mode│
│  │          │ │narrate │ │footer   │ │+ format  │ │aware)│ │aware│
│  └──────┬───┘ └───┬────┘ └──┬──────┘ └─┬────────┘ └┬─────┘ └┬──┘ │
│         │         │         │          │           │        │     │
│  Layer 4: Shared Primitives                                        │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │  PTY · VTE Parser · Grammar System · Line Buffer ·         │   │
│  │  Classifier · Dedup Engine · Emit Buffer · Stat Primitives │   │
│  │  Preflight · Enrichment · Format                           │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                                                                     │
│  MCP-Only Infrastructure (Layer 1 extensions)                       │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │  Process Table · Session Manager · Yield Engine ·           │   │
│  │  Policy Engine · Operator Handoff · Audit Log               │   │
│  └─────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────┘
```

### How the Layers Connect

**CLI proxy mode:** `mish npm install` → parse command → category router identifies "condense" → squasher pipeline (PTY → classify → dedup → emit) → condensed output to terminal.

**MCP server mode:** `sh_run(cmd="npm install")` → JSON-RPC dispatch → category router identifies "condense" → squasher pipeline → structured JSON response with process table digest.

The category router is the same code path in both modes. The difference is the entry point (CLI argument parsing vs JSON-RPC dispatch) and the output sink (terminal vs MCP response).

### Mode-Aware Handler Behavior

Most handlers behave identically in both modes. Two categories diverge:

| Category | CLI Mode | MCP Mode |
|----------|----------|----------|
| Interactive | Detect raw mode → transparent passthrough → session summary on exit | Return error/warning — interactive commands can't run over MCP stdio |
| Dangerous | Warn on terminal → prompt human → maybe execute | Return structured warning to LLM → policy engine → LLM decides or escalates to operator |

### Key Architectural Decision

**`sh_run` uses the category router internally.** This is critical. Without it, `sh_run("cp file backup/")` would run the squasher on a command that produces no output, returning nothing useful. With the category router, the same call triggers the narrate handler: `"→ cp: file → backup/ (4.2KB)"`.

The category system is not a CLI-only feature. It is shared core infrastructure that both modes depend on.

---

## Category Router

Every command entering mish (from either mode) is routed through the category system. See [PROXY.md](PROXY.md) for the full category definitions and routing rules.

```
command string
    │
    ▼
┌────────────────┐
│ Grammar Lookup │ ← load TOML grammar if command matches a known tool
└───────┬────────┘
        │
        ▼
┌────────────────┐
│  Categorize    │ ← grammar front matter declares category
│                │   fallback: check categories.toml mapping
│                │   unknown: default to condense
└───────┬────────┘
        │
        ├── Condense ────→ squasher pipeline (PTY → classify → dedup → emit)
        ├── Narrate ─────→ inspect args → stat files → execute → narrate result
        ├── Passthrough ─→ execute → pass output + metadata footer
        ├── Structured ──→ execute (maybe inject --porcelain) → parse → format
        ├── Interactive ─→ [mode-aware: passthrough or error]
        └── Dangerous ───→ [mode-aware: prompt or policy engine] → maybe execute
```

### Fallback

Unknown commands (not in any grammar or category map) default to **condense**. The squasher pipeline handles arbitrary output gracefully — structural heuristics (Tier 3) and the last-lines fallback produce useful summaries even without a grammar.

---

## Shared Core: Squasher Pipeline

The condense category and MCP `sh_run` (for condense-category commands) both use this pipeline. It is the original mish core.

```
┌──────────┐     ┌──────────┐     ┌──────────┐     ┌──────────┐
│ PTY      │────▶│ Line     │────▶│ Classify │────▶│ Emit     │
│ Capture  │     │ Buffer   │     │ Engine   │     │ Buffer   │
└──────────┘     └──────────┘     └──────────┘     └──────────┘
     │                                  │                │
     │                             ┌────┴────┐           │
     │                             │ Dedup   │           │
     │                             │ Engine  │           │
     │                             └─────────┘           │
     │                                                   │
     ▼                                                   ▼
  stdin passthrough                              Condensed output
  (user input forwarded                          (to terminal, LLM,
   to child process)                              or structured JSON)
```

---

## Stage 1: PTY Capture

**Responsibility:** Spawn the child process in a pseudoterminal, capture all output bytes, forward stdin transparently.

**Why PTY, not pipe:**

| Capability | Pipe | PTY |
|-----------|------|-----|
| stdout text | yes | yes |
| ANSI color codes | no (usually) | yes |
| CR vs LF distinction | yes | yes |
| Child detects terminal | no | yes |
| Progress bars/spinners work | no | yes |
| Terminal width/height queries | no | yes |

Many tools change their output format when they detect a non-terminal (pipe). npm suppresses colors, cargo changes its progress display, git disables pagers. Using a PTY ensures mish sees the same output the user would see.

**Implementation:**

```
PtyCapture {
    master_fd: RawFd       // our side of the PTY
    child_pid: Pid         // spawned process
    start_time: Instant    // for elapsed time tracking

    fn spawn(command: &[String]) → Result<Self>
    fn read_output(buf: &mut [u8]) → Result<usize>    // non-blocking
    fn write_stdin(buf: &[u8]) → Result<usize>         // passthrough
    fn wait() → Result<ExitStatus>
}
```

**Key behaviors:**
- Set PTY slave's COLUMNS/LINES to match the real terminal
- Forward SIGWINCH (terminal resize) to the child
- Non-blocking reads with poll/epoll for event-driven processing
- Forward all stdin to the child unmodified (user can still interact)

### Platform Risk: PTY Fidelity

PTY behavior varies between platforms. macOS and Linux have different `openpty` semantics, `SIGWINCH` timing, terminal attribute propagation, and raw mode detection. The `nix` crate abstracts some of this but not all.

**This is a critical-path risk.** If PTY proxying proves unreliable on macOS (the primary development platform for LLM coding tools), the entire architecture needs to pivot. Possible alternatives:

- **`portable-pty` crate** — higher-level abstraction over platform PTY differences
- **Real shell implementation** — replace PTY proxying with a native shell that controls execution directly, eliminating the proxy layer entirely
- **Pipe mode with `script` fallback** — use pipes for most commands, fall back to `script(1)` for commands that need terminal detection

**Early validation required:** The first implementation milestone must include a PTY stress test on macOS covering: ANSI color passthrough, progress bar detection (CR without LF), SIGWINCH forwarding, raw mode detection for interactive commands, and multi-byte UTF-8 at buffer boundaries.

**Pivot directive:** If PTY proxying proves unreliable on macOS, **stop and report back immediately.** Do not attempt heroic workarounds. The project owner wants the pivot signal — if PTY is not the answer, the project may pivot to a real shell implementation (mish owns the process lifecycle directly via fork/exec, no PTY proxy layer). This is a strategic decision, not an implementation detail. Escalate, don't fix.

---

## Stage 2: Line Buffer

**Responsibility:** Assemble raw bytes into logical lines, detect overwrite mode (progress bars), detect partial lines (prompts).

```
LineBuffer {
    partial: Vec<u8>           // incomplete line
    overwrite_mode: bool       // saw CR without LF
    last_byte_time: Instant    // for partial line timeout

    fn ingest(&mut self, bytes: &[u8]) → Vec<Line>
}

enum Line {
    Complete(String),           // terminated by \n
    Overwrite(String),          // CR without LF (progress/spinner)
    Partial(String),            // no terminator after timeout
}
```

**Byte-level rules:**

| Sequence | Meaning | Action |
|----------|---------|--------|
| `\n` | Line complete | Emit `Complete`, clear buffer |
| `\r\n` | Line complete (Windows) | Emit `Complete`, clear buffer |
| `\r` (alone) | Overwrite in place | Mark `overwrite_mode`, don't emit yet |
| `\r` then more bytes | Progress bar rewrite | Discard previous partial, accumulate new |
| No terminator, 500ms elapsed | Probable prompt | Emit `Partial` |

**Overwrite collapsing:** When `overwrite_mode` is true, each new CR discards the buffered partial line. Only the final state (before a `\n` or timeout) matters. This eliminates all spinner frames and progress bar updates in one rule.

---

## Stage 3: Classification Engine

**Responsibility:** Assign a classification to each line using three tiers of rules.

See [CLASSIFIER.md](CLASSIFIER.md) for full detail.

```
enum Classification {
    Hazard { severity: Severity, text: String, captures: HashMap<String, String> },
    Outcome { text: String, captures: HashMap<String, String> },
    Noise { action: NoiseAction },
    Prompt { text: String },
    Unknown { text: String },
}

enum Severity { Error, Warning }
enum NoiseAction { Strip, Dedup }
```

**Evaluation order (first match wins):**

```
1. Line type check:
   - Line::Overwrite → always Noise(Strip)
   - Line::Partial   → check prompt patterns, else Unknown

2. Tool grammar rules (if grammar loaded):
   a. Hazard rules     → Classification::Hazard
   b. Outcome rules    → Classification::Outcome
   c. Noise rules      → Classification::Noise

3. Universal patterns (always active):
   a. ANSI red + error keywords   → Hazard(Error)
   b. ANSI yellow + warn keywords → Hazard(Warning)
   c. Stack trace patterns         → Hazard(Error) with multiline
   d. Prompt patterns + partial    → Prompt

4. Structural heuristics:
   a. Decorative lines (===, ---)  → Noise(Strip)
   b. Edit-distance similarity     → Noise(Dedup)
   c. Default                      → Unknown (buffered)
```

---

## Stage 4: Emit Buffer

**Responsibility:** Accumulate classified lines, emit condensed output on flush triggers.

```
EmitBuffer {
    pending_unknown: Vec<Classification>   // unclassified lines, count only
    outcomes: Vec<Classification>          // stored for final summary
    ring: RingBuffer<String, 5>            // last 5 lines for unknown tools
    line_count: u64
    dedup: DedupEngine
    start_time: Instant

    fn accept(&mut self, classified: Classification)
    fn flush(&mut self) → Vec<EmitLine>
    fn finalize(&mut self, exit_code: i32) → Summary
}
```

**Flush triggers:**

| Trigger | Action |
|---------|--------|
| Process exits | Flush everything, build final summary |
| Hazard detected | Flush pending count, then emit hazard immediately |
| Prompt detected | Flush pending count, then emit prompt immediately |
| Silence > 2s | Flush pending (state might have changed) |
| Periodic timer (5s) | Flush pending (keep consumer updated during long runs) |
| Pending buffer > 500 lines | Flush count (don't accumulate unbounded) |

**Hazard lines always emit immediately.** They are never buffered, never collapsed. Errors and warnings are the highest-priority signal.

---

## Tool Detection

At invocation, before any output arrives, mish parses the command to determine which grammar to load:

```
fn detect_tool(args: &[String]) → Option<(Grammar, Action)> {
    // Walk args, match against grammar detect patterns
    // "npm" → npm grammar
    // "install" → npm.actions.install
    // Return grammar + action, or None for unknown tools
}
```

For compound commands (`&&`, `||`, `;`), mish tracks which segment is currently executing and switches grammars accordingly.

---

## Data Flow Example

```
Input (npm install, 1400 lines):

Line 1:    "npm warn deprecated inflight@1.0.6"
           → Grammar match: noise(dedup)
           → DedupEngine: template "npm warn deprecated {pkg}" count=1

Line 2-5:  More deprecation warnings
           → DedupEngine: count=4

Line 6:    "added 147 packages in 12.3s"
           → Grammar match: outcome, captures {count: 147, time: 12.3s}

Line 7-1400: Resolution/reify/fetch noise
           → Grammar match: noise(strip) — discarded

Exit code 0 → finalize():

Output:
  1400 lines → exit 0 (12.3s)
   + 147 packages installed
   ~ npm warn deprecated: inflight@1.0.6 (x4)
```

---

## Concurrency Model

**CLI proxy mode:** Single-threaded async event loop (tokio). One command at a time. The bottleneck is I/O (waiting for the child process), not CPU. Classification and dedup are fast string operations.

Event sources:
- Poll PTY for output bytes
- Poll stdin for user input to forward
- Timer for partial-line timeout (500ms)
- Timer for periodic flush (5s)
- Timer for silence detection (2s)

**MCP server mode:** Same tokio runtime, but multiplexed across sessions. Each session has its own PTY and event sources. The session manager coordinates access via per-session mutexes. The process table is a shared data structure updated atomically.

See [mish_spec.md](mish_spec.md) §10 for session management detail.

---

## Module Structure

The file structure reflects the layered architecture. Both CLI and MCP entry points share everything below the entry point layer.

```
mish/
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
│   ├── router/                 # Layer 2: Category router (shared)
│   │   ├── mod.rs              # Top-level routing by category
│   │   └── categories.rs       # Command categorization (grammar + TOML mapping + fallback)
│   │
│   ├── handlers/               # Layer 3: Category handlers
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
│   │   ├── utf8.rs             # Streaming UTF-8 decode
│   │   ├── progress.rs         # Progress bar detection/removal
│   │   ├── truncate.rs         # Oreo truncation with enriched markers
│   │   ├── pattern.rs          # Watch regex matching + presets
│   │   └── dedup.rs            # Deduplication engine
│   │
│   ├── core/                   # Layer 4: Shared primitives — other
│   │   ├── pty.rs              # PTY allocation and management
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
│   │   └── boundary.rs         # Command boundary detection
│   │
│   ├── process/                # MCP-only: process table
│   │   ├── table.rs            # Global process table + digest
│   │   ├── state.rs            # Process state machine
│   │   └── spool.rs            # Raw output circular buffer
│   │
│   ├── yield_engine/           # MCP-only: yield detection
│   │   ├── detector.rs         # Silence + prompt pattern detection
│   │   └── syscall.rs          # /proc/pid/syscall (Linux-only)
│   │
│   ├── policy/                 # MCP-only: policy engine
│   │   ├── config.rs           # TOML parsing + validation
│   │   ├── matcher.rs          # Rule matching engine
│   │   └── scope.rs            # Command scope resolution
│   │
│   ├── handoff/                # MCP-only: operator handoff
│   │   ├── state.rs            # Handoff state machine
│   │   ├── attach.rs           # CLI attach via Unix domain socket
│   │   └── summary.rs          # Credential-blind return
│   │
│   ├── audit/                  # MCP-only: audit logging
│   │   └── logger.rs           # Append-only audit log
│   │
│   └── shutdown.rs             # Graceful shutdown, process group cleanup
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
│   ├── make.toml
│   └── ...
│
└── tests/
    └── fixtures/
        ├── npm_install.txt
        ├── cargo_build.txt
        ├── pytest_output.txt
        └── ...
```
