# mish — LLM-Native Shell

One binary, two interfaces, shared core. mish sits between the shell and its caller — whether that's an LLM agent, a human developer, or both — and returns structured, context-efficient responses. It categorizes every command and applies the right handler: condensing verbose output, narrating silent operations, parsing structured results, and warning about dangerous commands. Pure heuristics, no LLM in the loop.

## Two Modes

**CLI proxy** (`mish <command>`) — wraps individual commands with category-aware output. Works with any LLM tool today, or standalone for humans who want cleaner terminal output.

**MCP server** (`mish serve`) — a full process supervisor over JSON-RPC. Manages concurrent PTY sessions, provides ambient process state on every response, detects when processes need input, and hands off to human operators for authentication. Built for LLM agents that need temporal control over long-running processes.

Both modes share the same core: category router, squasher pipeline, grammar system, and shared primitives. See [ARCHITECTURE.md](ARCHITECTURE.md) for the unified execution model.

## Problem

LLMs interact with the shell through a Bash tool. The interface is lossy in both directions:

- **Verbose commands** (npm install, cargo build) dump hundreds of lines into context where the signal is one line
- **Silent commands** (cp, mkdir, chmod) return nothing — the LLM gets no confirmation of what happened
- **Dangerous commands** (rm -rf, force push) execute without guardrails
- **Interactive commands** (vim, psql) break the non-interactive tool model

The LLM wastes context on noise, lacks context where it needs it, and has no safety layer.

## Solution

mish wraps every shell command. The LLM never calls bash directly.

```
Before:  LLM → Bash("npm install")  → 1400 lines raw output
After:   LLM → mish("npm install") → "1400 lines → exit 0\n + 147 packages"

Before:  LLM → Bash("cp file backup/") → "" (nothing)
After:   LLM → mish("cp file backup/") → "→ cp: file → backup/ (4.2KB)"

Before:  LLM → Bash("rm -rf node_modules") → "" (nothing, 312MB gone)
After:   LLM → mish("rm -rf node_modules") → "⚠ rm -rf: node_modules/ (47K files, 312MB) — destructive"
```

Every command is categorized and handled appropriately:

| Category | Commands | Behavior |
|----------|----------|----------|
| Condense | npm, cargo, docker, make, pytest | PTY capture → classify → condensed summary |
| Narrate | cp, mv, mkdir, rm, chmod | Inspect → execute → narrate what happened |
| Passthrough | cat, grep, ls, jq, diff | Output verbatim + metadata footer |
| Structured | git status, docker ps | Machine-readable parse → condensed view |
| Interactive | vim, htop, psql, node REPL | Transparent passthrough |
| Dangerous | rm -rf, force push, reset --hard | Warn before executing |

See [PROXY.md](PROXY.md) for the full proxy architecture and category system.

## Output Format

```
$ npm install
1400 lines → exit 0 (12.3s)
 + 147 packages installed
 ~ npm warn deprecated: inflight@1.0.6 (x3)
 ~ 2 moderate vulnerabilities
```

```
$ cargo build
340 lines → exit 101 (8.1s)
 ~ warning: unused variable `x` (x4 files)
 ! error[E0308]: mismatched types (src/main.rs:42:9)
   expected `String`, found `&str`
```

```
$ ./run-migration.sh
892 lines → exit 0 (45s)
 ... 847 lines
 ! WARNING: column "email" already exists, skipping
 ... 44 lines
 last: "Migration complete. 12 tables updated."
```

### Symbols

| Symbol | Meaning |
|--------|---------|
| `+` | Success / created / installed |
| `-` | Removed / deleted |
| `~` | Warning / non-fatal |
| `!` | Error / failure |
| `?` | Awaiting input |
| `→` | Narration (action taken, e.g. file ops) |
| `⚠` | Dangerous operation detected |
| `...` | Noise (line count only) |
| `last:` | Final line(s) before exit (unknown tools) |

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for the unified execution model showing both modes.

```
                     mish binary
                         │
           ┌─────────────┼─────────────┐
           ▼                           ▼
   CLI: mish <cmd>            MCP: mish serve
           │                           │
           └──────────┬────────────────┘
                      ▼
              Category Router
           (condense · narrate ·
        passthrough · structured ·
        interactive · dangerous)
                      │
                      ▼
              Shared Primitives
  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────┐
  │ PTY      │→│ Line     │→│ Classify │→│ Emit     │
  │ Capture  │ │ Buffer   │ │ Engine   │ │ Buffer   │
  └──────────┘ └──────────┘ └──────────┘ └──────────┘
                                  │
                             ┌────┴────┐
                             │ Dedup   │
                             └─────────┘
```

### Three-Tier Classification

1. **Tool grammars** — declarative rule files for known tools (npm, cargo, git, docker, etc.)
2. **Universal patterns** — ANSI color signals, stack trace shapes, error/warning keywords
3. **Structural heuristics** — edit-distance dedup, volume compression, temporal patterns, exit codes

See [GRAMMARS.md](GRAMMARS.md) for the grammar schema and tool grammar specs.
See [CLASSIFIER.md](CLASSIFIER.md) for the classification engine and heuristics.
See [DEDUP.md](DEDUP.md) for the deduplication engine.
See [STREAMING.md](STREAMING.md) for the streaming buffer and PTY capture.
See [PREFLIGHT.md](PREFLIGHT.md) for argument injection (quiet and verbose).
See [VERBOSITY.md](VERBOSITY.md) for verbose flag injection and file stat enrichment.
See [PROXY.md](PROXY.md) for the universal proxy architecture and command categories.
See [ENRICH.md](ENRICH.md) for error enrichment (failure diagnostics).

## Usage

### CLI Proxy Mode

```sh
# Every command goes through mish
mish npm install lodash
mish cp src/main.rs backup/
mish git status
mish cargo build --release

# Output modes
mish --json npm test            # structured JSON for tool-use
mish --passthrough cargo build  # full output + summary at end

# Compound commands (quoted)
mish "cd /project && npm install && npm test"

# Custom grammar directory
mish --grammars ./my-grammars make -j8
```

### MCP Server Mode

```sh
# Start as MCP server (configured in your MCP client)
mish serve --config ~/.config/mish/mish.toml
```

The MCP server exposes 5 tools: `sh_run`, `sh_spawn`, `sh_interact`, `sh_session`, `sh_help`. See [mish_spec.md](mish_spec.md) for the full MCP server specification including process table digest, watch patterns, yield engine, and operator handoff.

### As an LLM tool

In CLI proxy mode, mish can be exposed as the shell tool directly:

```json
{
  "name": "shell",
  "description": "Execute a shell command with structured output.",
  "parameters": {
    "command": { "type": "string" }
  }
}
```

Or the LLM is instructed to prefix `mish` before all Bash calls.

In MCP server mode, mish registers as an MCP server and the LLM calls `sh_run`/`sh_spawn`/`sh_interact` directly through JSON-RPC.

## Preflight

mish handles every command, but differently per category:

- **Quiet flag injection** — tools with `--quiet`, `--no-progress`, etc. can have those flags auto-injected in summary mode, reducing noise at the source before heuristics even run.
- **Recommendations** — when mish knows a tool has noise-reducing flags, it reports them back to the caller (LLM or human) for future use.

See [PREFLIGHT.md](PREFLIGHT.md) for quiet flag injection detail.
See [PROXY.md](PROXY.md) for the full category system and proxy architecture.

## Non-Goals

- No LLM in the loop. Pure heuristics for classification and routing.
- Not a shell replacement. CLI mode proxies individual commands; MCP mode supervises processes within existing shells.
- Not a log aggregator. Operates on single process streams (CLI) or managed sessions (MCP).
