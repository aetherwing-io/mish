# Command Proxy: Universal LLM Shell Interface

## The Shift

Original concept: mish wraps commands that produce verbose output and condenses it.

New concept: **mish wraps every command.** The LLM never calls bash directly. Every shell interaction goes through `mish`, which returns structured, context-efficient responses appropriate to the command type.

```
Before:
  LLM → Bash("npm install lodash")     → 1400 lines raw output → LLM context
  LLM → Bash("cp file.txt backup/")    → "" (empty stdout)     → LLM context
  LLM → Bash("cat config.json")        → 200 lines raw file    → LLM context

After:
  LLM → Bash("mish npm install lodash") → "1400 lines → exit 0\n + 147 packages"
  LLM → Bash("mish cp file.txt backup/") → "cp: file.txt → backup/ (2.3KB, ok)"
  LLM → Bash("mish cat config.json")     → file contents (passthrough, but with metadata)
```

## Relationship to MCP Server Mode

The category system described here is **shared core infrastructure** used by both CLI proxy mode and MCP server mode. In MCP mode, `sh_run` and `sh_spawn` invoke the same category router internally — the LLM calls `sh_run(cmd="cp file backup/")` and the narrate handler produces `"→ cp: file → backup/ (4.2KB)"` just as it would in CLI mode.

Two categories behave differently depending on mode:

| Category | CLI Mode | MCP Mode |
|----------|----------|----------|
| **Interactive** | Detect raw mode → transparent passthrough → session summary on exit | Return error/warning — interactive commands can't run over MCP stdio |
| **Dangerous** | Warn on terminal → prompt human → maybe execute | Return structured warning to LLM → policy engine evaluates → LLM decides or escalates to operator via handoff |

The entry point differs (CLI argument parsing vs JSON-RPC dispatch) and the output sink differs (terminal vs structured JSON response with process table digest), but the routing and handling logic is the same code path. See [ARCHITECTURE.md](ARCHITECTURE.md) for the four-layer execution model.

See [mish_spec.md](mish_spec.md) for MCP server features: process table, session management, yield engine, operator handoff, policy engine.

---

## Command Categories

Every command falls into one of these categories. The category determines how mish handles it.

### Category 1: Condense

Commands that produce verbose output where the signal is the outcome, not the stream. This is the original mish use case.

```
npm install, cargo build, docker build, make, pip install,
go build, yarn, pnpm, terraform apply, ansible-playbook,
webpack, tsc, gcc, pytest, jest, cargo test
```

Behavior: full PTY capture, classification, condensed summary.

### Category 2: Narrate

Commands that are quick, have minimal output, but the LLM currently gets nothing useful back. mish adds structured context.

```
cp, mv, ln, mkdir, rm, touch, chmod, chown, rmdir
```

These commands succeed silently or fail with a terse error. The LLM gets no confirmation of what actually happened. mish narrates the action:

```
$ mish cp src/main.rs src/main.rs.bak
cp: src/main.rs → src/main.rs.bak (4.2KB, ok)

$ mish mkdir -p src/components/auth
mkdir: created src/components/auth/ (3 dirs)

$ mish rm old-file.txt
rm: old-file.txt (1.1KB, removed)

$ mish mv config.json config.json.old
mv: config.json → config.json.old (892B, ok)

$ mish chmod 755 deploy.sh
chmod: deploy.sh 644 → 755

$ mish cp nonexistent.txt backup/
cp: error — nonexistent.txt: no such file (exit 1)

$ mish rm -rf node_modules
rm: node_modules/ (47,231 files, 312MB, removed)
```

Implementation: mish doesn't need PTY capture for these. It inspects the arguments, gathers metadata (file sizes, existence, permissions) before execution, runs the command, and reports what happened.

```rust
fn narrate_cp(args: &[String]) -> NarratedResult {
    let src = &args[0];
    let dst = &args[1];

    // Pre-flight: gather context
    let src_meta = fs::metadata(src);
    let dst_exists = fs::exists(dst);

    // Execute
    let status = Command::new("cp").args(args).status();

    // Narrate
    match (src_meta, status) {
        (Ok(meta), Ok(s)) if s.success() => {
            NarratedResult::ok(format!(
                "cp: {} → {} ({}{})",
                src, dst,
                human_size(meta.len()),
                if dst_exists { ", overwritten" } else { ", ok" }
            ))
        }
        (Err(e), _) => {
            NarratedResult::err(format!("cp: error — {}: {}", src, e))
        }
        (_, Ok(s)) => {
            NarratedResult::err(format!("cp: failed (exit {})", s.code().unwrap_or(-1)))
        }
    }
}
```

### Category 3: Passthrough

Commands where the output IS the value. mish passes content through but adds lightweight metadata.

```
cat, head, tail, less, grep, find, ls, echo, printf,
wc, sort, uniq, diff, jq, awk, sed (when outputting)
```

```
$ mish cat config.json
[config.json contents pass through verbatim]
── 47 lines, 1.2KB ──

$ mish ls -la src/
[ls output passes through verbatim]
── 12 entries ──

$ mish grep -r "TODO" src/
[grep output passes through verbatim]
── 8 matches in 5 files ──
```

The metadata footer is small but useful for the LLM: it knows how much content it just ingested.

### Category 4: Structured

Commands that have well-known output formats where mish can return richer structured data than the raw output.

```
git status, git diff, git log
docker ps, docker images
kubectl get pods
```

```
$ mish git status
git status:
 M src/main.rs (modified)
 M src/lib.rs (modified)
 ? src/new_file.rs (untracked)
 3 files (2 modified, 1 untracked)

$ mish docker ps
docker ps:
 ▪ myapp-web     (nginx:1.25)     Up 2h    :80→8080
 ▪ myapp-db      (postgres:16)    Up 2h    :5432
 ▪ myapp-redis   (redis:7)        Up 2h    :6379
 3 containers running
```

These are optional enhancements. If no structured handler exists, the command falls through to passthrough.

### Category 5: Interactive

Commands that take over the terminal. mish steps aside entirely.

```
vim, nvim, nano, emacs, htop, top, tmux, screen,
psql, mysql, node (REPL), python (REPL), less, man
```

```
$ mish vim file.txt
[vim runs normally, mish is transparent]
── session ended (42s) ──
```

mish detects interactivity (raw mode on PTY) and becomes fully transparent. On exit, it reports only the session duration.

### Category 6: Dangerous

Commands that modify system state in significant ways. mish can add a confirmation layer.

```
rm -rf, git push --force, git reset --hard,
docker system prune, drop table, chmod -R
```

```
$ mish rm -rf node_modules
rm -rf: node_modules/ (47,231 files, 312MB)
⚠ destructive — proceed? [narrating without confirmation in auto mode]

$ mish git push --force origin main
git push --force: will overwrite remote main
⚠ force push to main — 3 commits behind remote
```

Whether mish actually blocks or just warns depends on configuration. In LLM tool-use mode, the warning goes back to the LLM as structured data so it can make an informed decision (or escalate to the user).

## Category Detection

Categories are defined in the grammar front matter:

```toml
# grammars/_meta/categories.toml

[categories]

[categories.condense]
commands = ["npm", "cargo", "docker build", "make", "pip", "go build",
            "yarn", "pnpm", "terraform", "ansible-playbook", "webpack",
            "tsc", "gcc", "g++", "clang", "pytest", "jest"]

[categories.narrate]
commands = ["cp", "mv", "ln", "mkdir", "rmdir", "rm", "touch",
            "chmod", "chown", "chgrp", "install"]

[categories.passthrough]
commands = ["cat", "head", "tail", "grep", "rg", "find", "fd",
            "ls", "echo", "printf", "wc", "sort", "uniq",
            "diff", "jq", "awk", "sed", "tree", "file", "stat",
            "which", "whereis", "type", "date", "whoami", "pwd",
            "id", "uname", "env", "printenv", "hostname"]

[categories.structured]
commands = ["git status", "git diff", "git log",
            "docker ps", "docker images",
            "kubectl get"]

[categories.interactive]
commands = ["vim", "nvim", "nano", "emacs", "vi", "ed",
            "htop", "top", "btop", "tmux", "screen",
            "less", "more", "man", "fzf", "tig"]
# Also detected dynamically via raw mode on PTY

[categories.dangerous]
# Patterns, not just commands — because the danger is in the flags
patterns = [
    { pattern = '^rm\s+.*-.*r.*-.*f', reason = "recursive force delete" },
    { pattern = '^rm\s+-rf', reason = "recursive force delete" },
    { pattern = 'git\s+push\s+.*--force', reason = "force push" },
    { pattern = 'git\s+reset\s+--hard', reason = "hard reset" },
    { pattern = 'git\s+clean\s+-.*f', reason = "force clean" },
    { pattern = 'docker\s+system\s+prune', reason = "system prune" },
    { pattern = 'DROP\s+TABLE', reason = "drop table" },
    { pattern = 'chmod\s+-R\s+777', reason = "world-writable recursion" },
    { pattern = 'dd\s+', reason = "disk write" },
]
```

### Category Resolution Order

1. **Grammar front matter** — if the command matches a grammar with a `category` field, that category wins
2. **`categories.toml` mapping** — explicit command → category mapping for commands without grammars
3. **Fallback** — unknown commands default to **condense**

### Fallback

Unknown commands (not in any category map or grammar) default to **condense** — mish wraps with PTY, classifies output, returns a summary. This is the safest default because it never suppresses data, it just compresses it.

## Response Format

Every mish response follows a consistent structure the LLM can parse:

```
{status_symbol} {command}: {summary}
```

### Status symbols (consistent across all categories)

```
+   success
!   error
~   warning
?   awaiting input / confirmation needed
→   narration (action taken)
```

### Examples across categories

```
# Condense
+ npm install: 147 packages (12.3s)
~ npm install: 147 packages, 2 vulnerabilities (12.3s)
! cargo build: error[E0308] mismatched types (src/main.rs:42)

# Narrate
→ cp: src/main.rs → backup/ (4.2KB)
→ mkdir: created src/components/auth/ (3 dirs)
! rm: error — file not found

# Passthrough
[raw output]
── 47 lines, 1.2KB ──

# Structured
→ git status: 3 files (2 modified, 1 untracked)
→ docker ps: 3 containers running

# Interactive
→ vim: session ended (42s)

# Dangerous
⚠ rm -rf: node_modules/ (47,231 files, 312MB) — destructive
```

## LLM Integration

### System prompt instruction

The LLM is instructed to prefix all bash commands with `mish`:

```
When executing shell commands, always prefix with `mish`.
Instead of: Bash("npm install lodash")
Use:        Bash("mish npm install lodash")

mish returns condensed, structured output optimized for your context window.
Do not call shell commands without the mish prefix.
```

### Tool definition (alternative)

Or mish can be exposed as its own tool alongside Bash:

```json
{
  "name": "shell",
  "description": "Execute a shell command with structured output. Returns condensed results optimized for LLM context. Use this instead of raw Bash for all shell operations.",
  "parameters": {
    "command": { "type": "string", "description": "The shell command to execute" }
  }
}
```

This is cleaner — the LLM doesn't need to remember the prefix.

### Context savings estimate

Rough estimates for typical LLM coding sessions:

```
Command                  Raw output    mish output    Savings
─────────────────────────────────────────────────────────────
npm install              1400 lines    3 lines        99.8%
cargo build              340 lines     2 lines        99.4%
git status               12 lines      3 lines        75%
cp file backup/          0 lines       1 line         (adds useful info)
cat config.json          47 lines      47+1 lines     ~0% (passthrough)
docker build             800 lines     3 lines        99.6%
pytest                   200 lines     4 lines        98%
ls -la                   15 lines      15+1 lines     ~0% (passthrough)
mkdir -p path            0 lines       1 line         (adds useful info)
```

Over a typical 100-command coding session, mish could reduce total terminal context by 80-90%, while adding useful narration for commands that currently return nothing.

## Architecture

The category system routes every command through the appropriate execution path:

```
Every command goes through mish
    → categorize command (grammar front matter + categories.toml + fallback)
    → branch:
        condense    → PTY capture → classify → dedup → condense
        narrate     → inspect args → execute → narrate result
        passthrough → execute → pass output + metadata
        structured  → execute → parse → structured response
        interactive → [mode-aware: passthrough or error]
        dangerous   → [mode-aware: prompt or policy] → maybe execute
```

The condense pipeline (PTY → classify → dedup → emit) is the core for verbose commands. The other categories use lighter execution paths that may invoke individual shared primitives (stat, VTE stripping) without the full pipeline.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the unified four-layer module structure showing how CLI proxy mode and MCP server mode share the category router, handlers, and primitives.

## Narration Engine Detail

The narrate category needs per-command handlers. Each handler knows:
- What metadata to gather pre-execution
- How to interpret the arguments
- How to format the narration

### Generic narration (fallback)

For commands without a specific handler:

```rust
fn narrate_generic(command: &str, args: &[String], status: ExitStatus) -> String {
    if status.success() {
        format!("→ {}: ok (exit 0)", command)
    } else {
        format!("! {}: failed (exit {})", command, status.code().unwrap_or(-1))
    }
}
```

### File operation narrators

```rust
// Shared helper
fn file_info(path: &str) -> String {
    match fs::metadata(path) {
        Ok(m) => {
            let size = human_size(m.len());
            let kind = if m.is_dir() { "dir" } else { "file" };
            format!("{} ({})", kind, size)
        }
        Err(_) => "not found".to_string(),
    }
}

fn narrate_cp(args: &CpArgs) -> String {
    let src_info = file_info(&args.source);
    let dst_exists = Path::new(&args.dest).exists();

    // Execute cp
    let status = run_command("cp", &args.raw);

    if status.success() {
        let note = if dst_exists { ", overwritten" } else { "" };
        format!("→ cp: {} → {} ({}{})", args.source, args.dest, src_info, note)
    } else {
        format!("! cp: {} → {} — failed", args.source, args.dest)
    }
}

fn narrate_rm(args: &RmArgs) -> String {
    let mut total_files = 0u64;
    let mut total_bytes = 0u64;

    for target in &args.targets {
        if let Ok(info) = gather_tree_info(target) {
            total_files += info.file_count;
            total_bytes += info.total_bytes;
        }
    }

    let status = run_command("rm", &args.raw);

    if status.success() {
        if args.recursive {
            format!("→ rm: {} ({} files, {})",
                args.targets.join(", "),
                total_files,
                human_size(total_bytes))
        } else {
            format!("→ rm: {} ({})",
                args.targets.join(", "),
                human_size(total_bytes))
        }
    } else {
        format!("! rm: failed")
    }
}

fn narrate_mkdir(args: &MkdirArgs) -> String {
    let status = run_command("mkdir", &args.raw);

    if status.success() {
        let created_count = count_created_dirs(&args.targets);
        format!("→ mkdir: created {} ({} dirs)", args.targets.join(", "), created_count)
    } else {
        format!("! mkdir: failed")
    }
}

fn narrate_chmod(args: &ChmodArgs) -> String {
    let old_perms = get_permissions(&args.target);
    let status = run_command("chmod", &args.raw);

    if status.success() {
        let new_perms = get_permissions(&args.target);
        format!("→ chmod: {} {} → {}", args.target, old_perms, new_perms)
    } else {
        format!("! chmod: failed")
    }
}
```

### Structured handlers

For commands with well-known output formats:

```rust
fn handle_git_status(args: &[String]) -> String {
    // Run git status --porcelain=v2 for machine-readable output
    // (this is a safe quiet-flag injection — porcelain doesn't change behavior)
    let output = run_command("git", &["status", "--porcelain=v2"]);

    let mut modified = 0;
    let mut added = 0;
    let mut deleted = 0;
    let mut untracked = 0;
    let mut files = Vec::new();

    for line in output.lines() {
        match line.chars().next() {
            Some('1') => { /* ordinary change */ }
            Some('2') => { /* rename/copy */ }
            Some('?') => { untracked += 1; }
            _ => {}
        }
        // Parse each line, count by status
    }

    let total = modified + added + deleted + untracked;
    let mut parts = Vec::new();
    if modified > 0 { parts.push(format!("{} modified", modified)); }
    if added > 0 { parts.push(format!("{} added", added)); }
    if deleted > 0 { parts.push(format!("{} deleted", deleted)); }
    if untracked > 0 { parts.push(format!("{} untracked", untracked)); }

    format!("→ git status: {} files ({})", total, parts.join(", "))
}
```

## Pipes and Compound Commands

When the command contains pipes or compound operators, mish needs to decide what to do:

### Pipes

```
mish cat file.txt | grep "error"
```

Problem: mish can't wrap a pipeline by prefixing — the shell parses `|` before mish sees it.

Solutions:
1. **Quote the whole command**: `mish "cat file.txt | grep error"`
   - mish receives the full pipeline, spawns it in a shell
   - Wraps the entire pipeline as a unit
   - Category = passthrough (the final output is what matters)

2. **LLM tool wrapping**: the tool definition receives the full command string, mish handles parsing internally

### Compound commands

```
mish "cd /project && npm install && npm test"
```

mish splits on `&&` / `||` / `;` and handles each segment:
- `cd /project` → narrate (→ cd: /project)
- `npm install` → condense (+ 147 packages)
- `npm test` → condense (+ 42 tests passed)

Combined output:
```
→ cd: /project
+ npm install: 147 packages (12.3s)
+ npm test: 42 passed (3.1s)
```
