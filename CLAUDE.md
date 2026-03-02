# mish

LLM-native shell. A Rust binary that is both a **CLI command proxy** and a **full MCP server** for process supervision. It wraps shell commands with structured, context-efficient responses, manages concurrent PTY sessions, and provides process lifecycle control — all through a single binary.

## Status

Design/spec phase. No source code yet. All work is in `docs/`.

## Naming

The project is **mish**. Never use "llmsh" — that was the old name.

## Docs

**Start here:** `docs/ARCHITECTURE.md` is the **unified execution model** — it shows how both modes (CLI proxy and MCP server) share a four-layer architecture: Entry Points → Category Router → Mode-Aware Handlers → Shared Primitives.

| Doc | Covers |
|-----|--------|
| `ARCHITECTURE.md` | **Unified execution model**, module structure, PTY risk assessment |
| `SPEC.md` | Project overview and quick reference |
| `mish_spec.md` | **MCP server mode** definitive spec (process table, yield, handoff, policy) |
| `PROXY.md` | **CLI proxy mode** and the 6-category routing system |
| `GRAMMARS.md` | Grammar schema, tool grammar specs, dialect system |
| `CLASSIFIER.md` | Three-tier classification engine |
| `STREAMING.md` | Streaming buffer, PTY capture |
| `DEDUP.md` | Deduplication engine |
| `PREFLIGHT.md` | Argument injection (quiet/verbose flags) |
| `VERBOSITY.md` | Verbose flag injection, file stat enrichment |
| `ENRICH.md` | Error enrichment (failure diagnostics) |

## Two Modes, One Binary

### MCP Server (`mish serve`)

A full MCP server over stdio with 5 tools for process supervision. Defined in `mish_spec.md`.

- **`sh_run`** — synchronous execution with squashed output, watch patterns, enriched truncation
- **`sh_spawn`** — background processes with `wait_for` regex, alias management
- **`sh_interact`** — send input, read_tail, signal, kill, status on running processes
- **`sh_session`** — named PTY session lifecycle
- **`sh_help`** — self-documenting reference card

Key MCP-mode features:
- **Process table digest** on every response — the LLM has ambient awareness of all process state without polling
- **Watch patterns** — regex filters (`watch="@errors"`) that surface only matching lines, with presets
- **Yield engine** — detects when a process is waiting for input (silence + prompt heuristic), routes through policy → LLM → operator escalation
- **Operator handoff** — human takes over a PTY session for auth/MFA, crypto-random single-use handoff IDs, credential-blind return
- **Policy engine** — TOML-configured auto_confirm, yield_to_operator, forbidden rules (operational safety net, not security boundary)
- **Squasher pipeline** — VTE-based ANSI stripping → progress bar removal → dedup → Oreo truncation with enriched markers

### CLI Proxy (`mish <command>`)

Wraps individual commands with category-aware structured output. Defined in `PROXY.md`.

- **6 command categories**: condense, narrate, passthrough, structured, interactive, dangerous
- **Preflight** (bidirectional): too verbose → inject `--quiet`; too terse → inject `-v`
- **Error enrichment**: on failure, pre-fetches diagnostics the LLM would request next (path walks, stat, permissions) — read-only, fast (<100ms), non-speculative
- **Grammar dialect system**: handles platform differences (BSD vs GNU coreutils)

## Key Concepts

**Shared core (both modes use the same code path):**
- **Category router** — every command is categorized (condense/narrate/passthrough/structured/interactive/dangerous) and dispatched to the appropriate handler. `sh_run` in MCP mode invokes this same router.
- **Squasher pipeline** — VTE parse, progress removal, dedup, truncation (condense-category handler)
- **File stat primitives** — shared by VERBOSITY.md (narration on success) and ENRICH.md (diagnostics on failure)
- **Grammar system** — TOML tool grammars with dialect support, used for classification, categorization, and preflight in both modes

## Development

**Always use `make test` instead of bare `cargo test`.** The Makefile kills orphaned test runners before starting new tests. Interrupted test runs leave zombie `target/debug/deps/mish-*` processes that hold PTY file descriptors and cargo locks, blocking subsequent runs.

| Command | What |
|---------|------|
| `make test` | Full test suite (cleans zombies first) |
| `make test-lib` | Lib unit tests only |
| `make test-integration` | Grammar + fixture pipeline tests |
| `make release` | Release build + update symlink |

**Never pipe `cargo test`** — pipes swallow output and create unkillable zombie processes.

## Tech Stack

Rust, tokio, `vte` crate (ANSI parsing), `nix` crate (PTY via `forkpty`), TOML config, MCP stdio transport.

## Conventions

- Output symbols: `+` success, `-` removed, `~` warning, `!` error, `?` awaiting input, `→` narration, `⚠` dangerous
- Grammars are TOML files in `grammars/` with tool-specific rules
- Config at `~/.config/mish/mish.toml`, audit log at `~/.local/share/mish/audit.log`
- mish usage is optional — trust-based adoption, not enforced
