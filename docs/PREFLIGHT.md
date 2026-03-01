# Preflight: Argument Injection

## Overview

mish controls both sides of the pipe: arguments going in, output coming out. In preflight, mish modifies the command's arguments to normalize the output into a range that's optimal for classification and narration.

This is **bidirectional**:

- **Too verbose?** Inject `--quiet`, `--no-progress` → reduce noise at the source
- **Too terse?** Inject `-v`, `--stats` → get richer output for narration
- **Just right?** Pass through unchanged

See also [VERBOSITY.md](VERBOSITY.md) for the verbose injection design.
See also [PROXY.md](PROXY.md) for command categories (replaces the old deny list).

## Quiet Flag Injection (Reducing Verbosity)

Many tools have flags that reduce output noise:
- `--quiet` / `-q`
- `--silent` / `-s`
- `--no-progress`
- `--no-color` (sometimes useful — mish already handles ANSI)
- `--machine-readable` / `--porcelain`

Since mish is already condensing output, injecting these flags at the source reduces the volume of noise the heuristics need to process. Less noise in → cleaner classification.

### Two modes

#### 1. Auto-inject (preflight modification)

mish modifies the command before execution, adding quiet flags.

```
User runs:        mish npm install lodash
mish executes:    npm install lodash --no-progress
```

This is the aggressive approach. Pro: less noise to process. Con: changes the command semantics, might suppress output the user wants in passthrough mode.

**Only safe when:**
- The flag doesn't change behavior, only output (e.g., `--no-progress`, not `--quiet` which might suppress errors)
- mish is in summary mode (not passthrough)
- The flag is well-understood and documented in the grammar

#### 2. Recommend (post-flight advisory)

mish includes a recommendation in its output for the LLM to use next time.

```json
{
  "summary": "1400 lines → exit 0 (12.3s)\n + 147 packages installed",
  "recommendations": [
    {
      "flag": "--no-progress",
      "reason": "Suppresses progress bar (847 lines of noise eliminated)"
    }
  ]
}
```

This is the conservative approach. The LLM or user decides whether to adopt the flag next time. No command modification.

### Schema

In the grammar front matter:

```toml
[tool]
name = "npm"
detect = ["npm"]

[quiet]
# Flags that reduce noise without changing behavior
safe_inject = ["--no-progress"]      # safe to always add
recommend = ["--loglevel=warn"]      # suggest but don't inject

# Per-action overrides
[quiet.actions.install]
safe_inject = ["--no-progress", "--no-audit"]
recommend = ["--loglevel=warn"]

[quiet.actions.test]
# Don't inject anything for tests — progress matters
safe_inject = []
recommend = []
```

### Tool-specific quiet flags

```toml
# npm
safe_inject = ["--no-progress"]
recommend = ["--loglevel=warn"]

# cargo
safe_inject = []                          # cargo's output is already structured
recommend = ["--message-format=json"]     # enables machine-readable output

# pip
safe_inject = ["--no-color", "--progress-bar=off"]
recommend = ["-q"]

# docker build
safe_inject = ["--progress=plain"]        # disables fancy BuildKit progress
recommend = ["-q"]                        # only outputs image ID

# git
safe_inject = []                          # git output is already terse
recommend = ["--porcelain"]               # machine-readable for status/diff

# curl
safe_inject = ["-s"]                      # suppress progress meter
recommend = []

# wget
safe_inject = ["--no-verbose"]
recommend = ["-q"]

# rsync
safe_inject = []
recommend = ["--info=progress2"]          # single progress line instead of per-file

# terraform
safe_inject = ["-no-color"]
recommend = []

# ansible
safe_inject = []
recommend = ["--one-line"]                # condensed output format

# kubectl
safe_inject = []
recommend = ["-o=json"]                   # machine-readable
```

### Injection logic

```rust
fn preflight(command: &mut Vec<String>, grammar: &Grammar, mode: OutputMode) -> Vec<Recommendation> {
    let mut recommendations = Vec::new();

    // Only inject in summary mode, not passthrough
    if mode == OutputMode::Summary {
        if let Some(quiet) = &grammar.quiet {
            let action_quiet = grammar.current_action
                .and_then(|a| quiet.actions.get(&a.name));

            let inject_flags = action_quiet
                .map(|q| &q.safe_inject)
                .unwrap_or(&quiet.safe_inject);

            for flag in inject_flags {
                // Don't add if already present
                if !command.iter().any(|arg| arg.starts_with(flag.split('=').next().unwrap())) {
                    command.push(flag.clone());
                }
            }

            // Collect recommendations
            let rec_flags = action_quiet
                .map(|q| &q.recommend)
                .unwrap_or(&quiet.recommend);

            for flag in rec_flags {
                recommendations.push(Recommendation {
                    flag: flag.clone(),
                    reason: format!("Reduces output noise for {}", grammar.tool.name),
                });
            }
        }
    }

    recommendations
}
```

### Safety constraints

1. **Never inject flags that change behavior** — only output formatting flags
2. **Never inject if the flag is already present** — current detection uses exact prefix matching (`--no-progress` checks for args starting with `--no-progress`). Short/long form equivalence (`-q` vs `--quiet`) is not yet handled. Research needed for the most common commands where this matters — even covering the top 20 tools would drastically improve agent workflows.
3. **Never inject in passthrough mode** — the user wants to see everything
4. **Never inject flags that suppress errors** — `--quiet` on some tools hides warnings too
5. **Each injection must be declared in the grammar** — no guessing

### Machine-readable mode (piano roll → tracker)

Some tools have a structured output mode that's dramatically more efficient to parse than human-readable text — the same way a tracker format is more token-efficient than an ASCII piano roll while carrying richer data (velocity, duration, round-trip fidelity).

When a tool offers a machine-readable mode, mish can switch to it and parse structured output directly instead of regex-matching human text:

| Tool | Human output | Machine-readable flag | What it gives mish |
|------|-------------|----------------------|-------------------|
| cargo | Color text, warnings, errors | `--message-format=json` | Structured JSON per diagnostic |
| git status | Formatted text | `--porcelain=v2` | Machine-parseable status per file |
| kubectl | Formatted tables | `-o json` | Full resource state |
| docker ps | Formatted table | `--format json` | Container state per entry |

```toml
[quiet.machine_readable]
flag = "--message-format=json"
parser = "cargo-json"              # tool-specific structured parser
```

This requires a small structured parser per tool instead of regex rules. The payoff is high for tools with good machine-readable output (cargo, git, kubectl) — vastly better classification with zero heuristic ambiguity.

## Integration Points

### For LLM tool-use callers

When mish is used as an LLM tool, recommendations flow back to the LLM:

```json
{
  "tool": "mish",
  "input": { "command": "npm install lodash" },
  "output": {
    "summary": "1400 lines → exit 0 (12.3s)\n + 147 packages installed",
    "exit_code": 0,
    "injected_flags": ["--no-progress"],
    "recommendations": [
      { "flag": "--loglevel=warn", "reason": "Reduces noise from 1400 to ~50 lines" }
    ]
  }
}
```

**Adoption model:** Using mish is optional. The operator MAY instruct the LLM to prefix commands with `mish`. If the LLM finds the structured responses useful, it continues using mish — trust-based adoption, not enforcement. The LLM can:
- Adopt `--loglevel=warn` on future npm calls
- Make informed decisions about which flags to use
- Choose raw Bash when it needs unfiltered output

### For CLI users

Passthrough commands run normally with a metadata footer (see PROXY.md categories):
```
$ mish cat file.txt
[cat output appears normally]
── 47 lines, 1.2KB ──
```

Quiet injection: mish mentions what it added:
```
$ mish npm install
mish: added --no-progress
1400 lines → exit 0 (12.3s)
 + 147 packages installed
```
