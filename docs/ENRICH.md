# Error Enrichment

## The Principle

When a command fails, the LLM's next actions are predictable. It's going to probe the environment — check if files exist, list directories, read error logs, inspect permissions. That's 3-5 additional round trips of context and latency spent on pure diagnostics.

mish already knows the command, the arguments, and the exit code. It can infer what the LLM will need and gather it in milliseconds before responding.

This is not about fixing the command (that's the LLM's job). It's about returning the failure alongside the diagnostic context the LLM was about to go collect anyway. One response instead of five.

```
Without mish:
  Turn 1:  mv file.txt /opt/data/archive/    → "No such file or directory" (exit 1)
  Turn 2:  ls /opt/data/                      → "No such file or directory"
  Turn 3:  ls /opt/                           → "bin  etc  homebrew  local"
  Turn 4:  ls -la file.txt                    → "-rw-r--r-- 4.2KB file.txt"
  Turn 5:  mkdir -p /opt/data/archive         → (ok)
  Turn 6:  mv file.txt /opt/data/archive/     → (ok)

With mish:
  Turn 1:  mish mv file.txt /opt/data/archive/
           → ! mv: /opt/data/archive/ — no such directory
             source: file.txt (4.2KB, exists ✓)
             path:   /opt/ ✓  /opt/data/ ✗
           (exit 1)

  Turn 2:  mish mkdir -p /opt/data/archive && mv file.txt /opt/data/archive/
           → done
```

Six turns became two. The LLM didn't need to probe — mish already told it where the path breaks.

## Relationship to Narration

For narrate-category commands (cp, mv, mkdir, rm, chmod, etc.), the narrate handler runs first — it is the category handler. If the command fails (exit code != 0), error enrichment adds diagnostic context **below** the narrate output. The narrate handler reports what happened; enrichment reports why it failed.

```
! cp: file.txt → /opt/data/archive/ — failed (exit 1)
  source:  file.txt (4.2KB ✓)
  path:    /opt/ ✓  /opt/data/ ✗
  nearest: /opt/ contains: bin/, etc/, homebrew/, local/
```

The first line comes from the narrate handler. The indented lines come from enrichment. For condense-category commands, enrichment appends to the squasher summary instead.

## How It Works

Error enrichment triggers **only on failure** (exit code != 0). On success, there's nothing to diagnose.

When a command fails, mish:

1. Parses the command to understand the **intent** (what files/paths were involved)
2. Runs cheap, read-only diagnostic checks relevant to that intent
3. Returns the error + diagnostics as a single response

```
Command fails
    │
    ▼
┌──────────────────┐
│ What was the      │
│ intent?           │──── file operation (cp, mv, rm, touch)
│                   │──── directory operation (mkdir, cd, ls)
│                   │──── network operation (curl, ssh, git push)
│                   │──── build/run (cargo, npm, python)
│                   │──── unknown
└────────┬─────────┘
         │
         ▼
┌──────────────────┐
│ Gather relevant   │
│ diagnostics       │──── stat source/dest paths
│ (read-only, fast) │──── walk path hierarchy
│                   │──── check permissions
│                   │──── check common prerequisites
└────────┬─────────┘
         │
         ▼
┌──────────────────┐
│ Return error +    │
│ diagnostics       │
└──────────────────┘
```

## Diagnostic Strategies

**Note:** The file stat and path inspection primitives here are shared infrastructure with VERBOSITY.md's pre/post-flight stat. On success, the stat data feeds narration; on failure, it feeds enrichment. Same machinery, different consumers.

### Path resolution

The most common failure mode. A path doesn't exist, or part of it doesn't. Walk the path and report where it breaks.

```rust
fn diagnose_path(path: &Path) -> PathDiagnosis {
    let mut current = PathBuf::new();
    let mut last_valid = PathBuf::new();

    for component in path.components() {
        current.push(component);
        if current.exists() {
            last_valid = current.clone();
        } else {
            return PathDiagnosis {
                requested: path.to_path_buf(),
                last_valid,
                breaks_at: current,
                // What exists at the last valid point
                siblings: list_dir(&last_valid),
            };
        }
    }

    PathDiagnosis {
        requested: path.to_path_buf(),
        last_valid: path.to_path_buf(),
        breaks_at: PathBuf::new(),  // fully valid
        siblings: vec![],
    }
}
```

Output:
```
path: /opt/ ✓  /opt/data/ ✗
  /opt/ contains: bin/, etc/, homebrew/, local/
```

The LLM now knows the exact break point AND what's adjacent (maybe it typo'd `data` and meant `homebrew/data` or similar).

### Source/target existence

For file operations, always report whether the source and target exist:

```rust
fn diagnose_file_op(source: &Path, dest: &Path) -> FileOpDiagnosis {
    FileOpDiagnosis {
        source_exists: source.exists(),
        source_info: stat_if_exists(source),     // size, perms, mtime
        dest_exists: dest.exists(),
        dest_info: stat_if_exists(dest),
        dest_parent_exists: dest.parent().map(|p| p.exists()).unwrap_or(false),
        dest_parent_info: dest.parent().and_then(|p| stat_if_exists(p)),
    }
}
```

Output:
```
source: file.txt (4.2KB, -rw-r--r--, exists ✓)
dest:   /opt/data/archive/file.txt
  parent /opt/data/archive/ ✗
  path: /opt/ ✓  /opt/data/ ✗
```

### Permission checks

When exit code suggests permission denied (EACCES, exit 126, or error text matches):

```rust
fn diagnose_permissions(path: &Path) -> PermissionDiagnosis {
    let meta = fs::metadata(path);
    let euid = unsafe { libc::geteuid() };
    let egid = unsafe { libc::getegid() };

    PermissionDiagnosis {
        path: path.to_path_buf(),
        owner: get_owner(path),
        group: get_group(path),
        mode: meta.permissions().mode(),
        running_as: get_username(euid),
        running_group: get_groupname(egid),
        issue: describe_permission_gap(meta, euid, egid),
    }
}
```

Output:
```
! permission denied: /etc/nginx/nginx.conf
  file: root:root 0644 (-rw-r--r--)
  running as: deploy (uid 1000)
  issue: write requires owner (root) or sudo
```

### Command not found

When exit 127 (command not found):

```rust
fn diagnose_command_not_found(cmd: &str) -> CommandDiagnosis {
    CommandDiagnosis {
        command: cmd.to_string(),
        in_path: check_path_for(cmd),
        package_hint: guess_package(cmd),       // common cmd → package mapping
    }
}
```

Output:
```
! command not found: rg
  not in PATH
  install: brew install ripgrep | apt install ripgrep | cargo install ripgrep
```

Package hints are loaded from an extensible TOML config alongside grammars (not compiled in):

```toml
# grammars/_meta/packages.toml

[packages]
rg = "ripgrep"
fd = "fd-find"
bat = "bat"
exa = "exa"
jq = "jq"
yq = "yq"
fzf = "fzf"
htop = "htop"
tree = "tree"
wget = "wget"
curl = "curl"
```

Operators can extend this with their own command→package mappings without recompiling.

## Intent-Specific Enrichment

Different command categories have different failure modes. mish uses the command category (from PROXY.md) to determine which diagnostics to run.

### File operations (cp, mv, ln, rm, touch, chmod)

On failure, always check:
```
1. Source exists?           → stat source
2. Dest parent exists?     → walk dest path
3. Permissions adequate?   → check write perms on dest parent
4. Disk space?             → df on dest filesystem (only if ENOSPC)
```

```
! mv: file.txt → /opt/data/archive/ — no such directory (exit 1)
  source:  file.txt (4.2KB ✓)
  path:    /opt/ ✓  /opt/data/ ✗
  nearest: /opt/ contains: bin/, etc/, homebrew/, local/
```

### Git operations

On failure, check:
```
1. In a git repo?          → git rev-parse --git-dir
2. Remote reachable?       → (from error text, don't probe network)
3. Branch exists?          → git branch --list
4. Clean working tree?     → git status --porcelain (for operations that need it)
5. Auth issue?             → detect from error text
```

```
! git push: rejected — non-fast-forward (exit 1)
  branch:  main (3 commits ahead, 2 behind origin/main)
  hint:    pull and merge, or push --force
```

```
! git checkout: error — feature/xyz not found (exit 1)
  local branches:  main, develop, feature/xy, feature/xz
  similar:         feature/xy, feature/xz
```

### Build/compile errors

Build errors already contain rich information (file:line:col). mish enrichment is lighter here — mostly confirming the referenced files exist and showing relevant context.

```
! cargo build: error[E0308] at src/main.rs:42:9 (exit 101)
  src/main.rs:42 exists ✓ (last modified 2 min ago)
```

For missing dependency errors:

```
! npm install: ERESOLVE — dependency conflict (exit 1)
  node_modules/ exists (1,247 packages)
  package-lock.json mtime: 3 days ago
```

### Network operations (curl, wget, ssh)

On failure, minimal enrichment (don't make network calls to diagnose network failures):

```
! curl: connection refused — localhost:3000 (exit 7)
  port 3000: not listening
```

```
! ssh: connection timed out — prod-server.example.com (exit 255)
  (no further diagnostics — network probing would be slow)
```

The port check uses a non-blocking `connect()` with a 10ms timeout. If the port is filtered (firewall DROP rather than REJECT), the check returns "port status unknown" rather than blocking. Platform-specific diagnostic behavior (e.g., which system calls are available for port inspection) is handled via the grammar dialect system — see GRAMMARS.md.

### Process execution

On exit codes with specific meanings:

```
exit 126 → permission denied
  → diagnose_permissions(command_path)

exit 127 → command not found
  → diagnose_command_not_found(command)

exit 137 → killed (OOM)
  → report: "killed by signal 9 (likely OOM)"
  → if available: memory usage at time of kill

exit 139 → segfault
  → report: "segmentation fault (SIGSEGV)"

exit 130 → ctrl-C
  → report: "interrupted by user (SIGINT)"
  → (no enrichment needed, user chose this)
```

## What Enrichment Is NOT

Enrichment is bounded by strict constraints:

1. **Read-only.** Never modify the filesystem, network, or process state. Only stat, list, and read.

2. **Fast.** Every diagnostic must complete in under 100ms. No network calls (except local port checks). No recursive directory walks deeper than 2 levels. No reading file contents (just stat).

3. **Relevant.** Only run diagnostics related to the command's intent and failure mode. Don't check permissions when the error is "file not found." Don't walk the path when the error is "permission denied."

4. **Cheap.** Maximum 5-10 diagnostic operations per failure. Don't enumerate entire directory trees. Don't stat every file in a directory — stat the specific files the command referenced.

5. **Non-speculative.** Report what IS, not what MIGHT be. "source exists, dest parent doesn't" — not "maybe you meant /opt/local/data." No `hint:` fields — the LLM already infers next actions better from the actual diagnostic facts than from canned suggestions.

The LLM does the reasoning. mish does the observation. The enrichment is just pre-fetching the observations the LLM was going to request anyway.

## Enrichment in the Grammar

Enrichment rules are declared in the grammar alongside verbosity and classification:

```toml
[tool]
name = "mv"
detect = ["mv"]
category = "narrate"

[enrich]
# What to diagnose on failure
on_failure = ["source_exists", "dest_path_walk", "permissions"]

# Argument mapping: which args are source vs dest
[enrich.args]
source = "positional[0..-1]"     # all positional args except last
dest = "positional[-1]"          # last positional arg
```

```toml
[tool]
name = "git"
detect = ["git"]

[enrich]
on_failure = ["is_git_repo", "branch_exists", "working_tree_clean"]

[enrich.actions.push]
on_failure = ["remote_ref_status", "ahead_behind"]

[enrich.actions.checkout]
on_failure = ["branch_list_similar"]
```

### Built-in diagnostic functions

These are the reusable diagnostic primitives:

```
source_exists         stat the source argument(s)
dest_path_walk        walk the destination path, find break point
permissions           check read/write/execute on relevant paths
is_git_repo           git rev-parse --git-dir
branch_exists         git branch --list {branch}
branch_list_similar   git branch --list, fuzzy match against target
working_tree_clean    git status --porcelain
remote_ref_status     git remote show (from cached/local data only)
ahead_behind          git rev-list --count --left-right
port_listening        non-blocking connect() to localhost:port (10ms timeout)
dir_listing           ls the parent or nearest existing directory
disk_space            df on the target filesystem
node_modules_check    existence + age of node_modules and lockfile
```

Each function is tagged with its cost:

```
instant  (<1ms):    source_exists, permissions, is_git_repo, disk_space
fast     (<10ms):   dest_path_walk, dir_listing, branch_exists, port_listening
moderate (<100ms):  branch_list_similar, command_similar, ahead_behind, working_tree_clean
```

mish runs up to 100ms worth of diagnostics total. It picks from the `on_failure` list in declaration order, skipping any that would exceed the budget.

## Output Format

Enrichment appears as indented context below the error line:

```
! {error description} (exit {code})
  {diagnostic key}: {value}
  {diagnostic key}: {value}
  ...
```

Examples:

```
! mv: file.txt → /opt/data/archive/ — no such directory (exit 1)
  source:  file.txt (4.2KB ✓)
  path:    /opt/ ✓  /opt/data/ ✗
  nearest: /opt/ contains: bin/, etc/, homebrew/, local/
```

```
! cargo build: error[E0433] unresolved import `serde` (exit 101)
  Cargo.toml: serde not in [dependencies]
```

```
! python app.py — ModuleNotFoundError: flask (exit 1)
  python: /usr/bin/python3 (3.11.5)
  venv:   .venv/ exists but not activated
```

```
! docker compose up — port already in use :8080 (exit 1)
  port 8080: listening (pid 12345, node)
```

The format is compact, each line carries one diagnostic fact, and the LLM can parse it trivially to decide its next action.
