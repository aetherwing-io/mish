# Verbosity Injection

## The Principle

mish controls both sides of the pipe: the arguments going in and the output coming out. This means it can **increase verbosity** on commands that are too terse, then condense the richer output into exactly the tokens the LLM needs.

mish is not a filter. It's a translator. The goal is maximum signal per token in either direction.

```
Raw command:     cp file backup/
  → LLM gets:   "" (nothing)

With mish:       cp -v file backup/    (mish added -v)
  → cp says:    "file -> backup/file"
  → mish stat:  file=70KB,mtime=1983-09-17  backup/file=70KB,mtime=2026-02-28
  → LLM gets:   "→ cp: file (70KB) → backup/file (70KB, ✓ size match)"
```

The verbose output never reaches the LLM. mish consumes it internally to build a richer, compressed confirmation.

## Verbosity Flags by Tool

Each grammar declares flags that mish injects to get richer output for narration.

```toml
[verbosity]
# Flags mish adds to get more information from the command
inject = ["-v"]               # or ["--verbose"], ["-v", "--stats"], etc.

# What the verbose output gives us that silent doesn't
provides = ["per-file confirmation", "path resolution"]
```

### File operations

**Dialect note:** The `-v` output format varies between GNU and BSD coreutils. macOS ships BSD where `cp -v` output differs from GNU. The grammar dialect system (see GRAMMARS.md) handles these platform differences — each tool grammar can declare dialect-specific patterns for parsing verbose output.

| Command | Silent output | mish injects | Verbose output | mish returns |
|---------|--------------|--------------|----------------|--------------|
| `cp a b` | (nothing) | `-v` | `'a' -> 'b'` | `→ cp: a (4KB) → b (4KB, ✓)` |
| `cp -r dir/ dst/` | (nothing) | `-v` | per-file listing | `→ cp: dir/ → dst/ (47 files, 12MB)` |
| `mv a b` | (nothing) | `-v` | `renamed 'a' -> 'b'` | `→ mv: a → b (4KB)` |
| `rm file` | (nothing) | `-v` | `removed 'file'` | `→ rm: file (4KB, removed)` |
| `rm -rf dir/` | (nothing) | `-v` | per-file listing | `→ rm: dir/ (312 files, 48MB, removed)` |
| `mkdir -p a/b/c` | (nothing) | `-v` | per-dir listing | `→ mkdir: a/b/c (created 3 dirs)` |
| `chmod 755 f` | (nothing) | `-v` | `mode changed 0644 → 0755` | `→ chmod: f 644 → 755` |
| `chown user f` | (nothing) | `-v` | `changed owner` | `→ chown: f root → user` |
| `ln -s a b` | (nothing) | `-v` | `'b' -> 'a'` | `→ ln: b → a (symlink)` |

### Network/transfer

| Command | Silent output | mish injects | Verbose output | mish returns |
|---------|--------------|--------------|----------------|--------------|
| `rsync -a src/ dst/` | (nothing) | `-v --stats` | per-file + stats | `→ rsync: 147 files, 12MB xfr, 892MB total` |
| `scp file host:` | minimal | `-v` | connection + transfer detail | `→ scp: file (4KB) → host:file (✓)` |
| `curl -o f url` | progress bar | `-s -w '%{http_code} %{size_download}'` | structured stats | `→ curl: url → f (200, 4.2KB, 0.3s)` |
| `wget url` | progress bar | `-nv` | single-line result | `→ wget: url → file (4.2KB)` |

### Package managers (reducing verbosity)

Package managers are already too chatty — mish *reduces* rather than increases their verbosity. See [PREFLIGHT.md](PREFLIGHT.md) for quiet flag injection on these tools.

| Command | Normal output | mish injects | Result |
|---------|--------------|--------------|--------|
| `pip install pkg` | verbose already | `--no-color --progress-bar=off` | strips noise, keeps result |
| `npm install` | verbose already | `--no-progress` | strips progress, keeps summary |
| `cargo build` | verbose already | (nothing) | already rich enough |
| `apt install pkg` | verbose already | `-q` here we REDUCE | strips lists, keeps actions |

### Git

| Command | Normal output | mish injects | mish returns |
|---------|--------------|--------------|--------------|
| `git add file` | (nothing) | `-v` | `→ git add: file (staged, +47 -12)` |
| `git commit -m "msg"` | hash + msg | `--stat` | `→ commit abc1234: "msg" (3 files, +89 -23)` |
| `git push` | ref update | (nothing) | `→ push: main → origin (3 commits)` |
| `git status` | human format | `--porcelain=v2` | `→ status: 3 files (2M, 1?)` |
| `git stash` | one line | (nothing) | `→ stash: saved WIP (3 files modified)` |

### Build tools

| Command | Normal output | mish injects | Result |
|---------|--------------|--------------|--------|
| `make` | recipe echo | (nothing or `--no-print-directory`) | condense, already verbose |
| `docker build .` | layer output | `--progress=plain` | condense layer noise |
| `terraform apply` | verbose already | `-no-color` | condense, strip ANSI |

## Enrichment Beyond -v

`-v` is just one source of enrichment. mish can also gather context through its own inspection, independent of the command. The stat primitives below are shared infrastructure with [ENRICH.md](ENRICH.md) — on success they feed narration, on failure they feed error diagnostics.

### Pre-execution stat

Before running the command, mish can stat relevant files:

```rust
struct PreFlightInfo {
    source_size: Option<u64>,
    source_mtime: Option<SystemTime>,
    source_permissions: Option<u32>,
    dest_exists: bool,
    dest_size: Option<u64>,
    dest_mtime: Option<SystemTime>,
}
```

### Post-execution stat

After the command completes, stat the targets again:

```rust
struct PostFlightInfo {
    dest_size: Option<u64>,
    dest_mtime: Option<SystemTime>,
    size_match: bool,              // src size == dst size (for copies)
    file_count: Option<u64>,       // for recursive operations
    total_bytes: Option<u64>,      // for recursive operations
}
```

### Enriched narration

Combine verbose output + stat data for the richest possible compact response:

```
→ cp: /src/data.json (70KB, Sep 17 1983) → /dst/data.json (70KB, Feb 28 2026, ✓ size)
```

The LLM knows:
- Source existed (70KB, old mtime)
- Destination was created (today's mtime = fresh)
- Sizes match (integrity indicator)
- Operation succeeded

All in one line. Without mish, the LLM knows nothing.

## Grammar Schema Addition

The verbosity config extends the grammar:

```toml
[tool]
name = "cp"
detect = ["cp"]
category = "narrate"

[verbosity]
inject = ["-v"]
provides = ["per-file-confirmation"]

# What to stat before execution
[preflight]
stat_args = "positional"           # stat the positional args
# or stat_args = "after-flag:o"    # stat the arg after -o flag

# What to stat after execution
[postflight]
stat_target = "last_positional"    # stat the last positional arg
verify_size = true                 # compare src/dst sizes
```

### Injection safety

Same constraints as quiet flag injection (PREFLIGHT.md), plus:

1. **Never inject flags that change behavior** — `-v` must only add output, not change semantics
2. **Never inject if already present** — if the user already passed `-v`, don't add `-vv`
3. **Know the tool's -v behavior** — some tools use `-v` for something other than verbose (rare but possible)
4. **Inject at the right position** — flags must go in the correct argument position for the command

```rust
fn inject_verbosity(command: &mut Vec<String>, grammar: &Grammar) {
    if let Some(verbosity) = &grammar.verbosity {
        for flag in &verbosity.inject {
            // Don't double-inject
            if command.iter().any(|arg| arg == flag) {
                continue;
            }

            // Don't inject if a stronger variant exists
            // e.g., don't add -v if -vv is already there
            if flag == "-v" && command.iter().any(|arg| arg.starts_with("-v")) {
                continue;
            }

            // Insert after the command name but before positional args
            let insert_pos = find_flag_insertion_point(command);
            command.insert(insert_pos, flag.clone());
        }
    }
}
```

## The Full Picture

mish's flag control is now bidirectional:

```
                    ┌─────────────────────┐
                    │   mish preflight    │
                    │                     │
  command in ──────▶│  Too verbose?       │
                    │   → inject --quiet  │
                    │   → inject --no-progress
                    │                     │
                    │  Too terse?         │
                    │   → inject -v       │
                    │   → inject --stats  │
                    │   → stat files      │
                    │                     │
                    │  Just right?        │
                    │   → pass through    │
                    └─────────┬───────────┘
                              │
                              ▼
                    ┌─────────────────────┐
                    │   execute command    │
                    └─────────┬───────────┘
                              │
                              ▼
                    ┌─────────────────────┐
                    │  mish postflight    │
                    │                     │
                    │  Verbose output?    │
                    │   → classify        │
                    │   → condense        │
                    │                     │
                    │  Terse output?      │
                    │   → enrich with     │
                    │     verbose + stat   │
                    │   → narrate         │
                    └─────────┬───────────┘
                              │
                              ▼
                    Optimal LLM context
                    (max signal, min tokens)
```

The LLM always gets the same quality of response regardless of whether the underlying tool is verbose or silent. mish normalizes everything into the signal the LLM needs for its next reasoning step.
