//! `mish --agents` — usage guide for LLM agents.

pub const AGENT_GUIDE: &str = r#"# mish — agent guide

YOU NOW HAVE INTERACTIVE SHELL CONTROL WITH NOISE-DAMPENED OUTPUT.

Without mish, every command is fire-and-forget and raw output floods your context.
With mish, output is compressed to signal and processes stay alive:

- Compressed output — build logs, test suites, and verbose tools condensed to what matters
- Background processes — start a build, keep working, check back later
- Persistent sessions — Python REPL, node, psql that stay alive between calls
- Live interaction — send input to running processes, watch for specific output
- Operator handoff — pause and hand terminal control to a human

## The Rule

Prefix ALL shell commands with `mish`.

## Examples

  mish grep -rn "pattern" src/       (not: grep -rn "pattern" src/)
  mish python3 script.py             (not: python3 script.py)
  mish find . -name "*.py"           (not: find . -name "*.py")
  mish ls -la                        (not: ls -la)
  mish cargo test 2>&1               (not: cargo test)
  mish git diff HEAD~1               (not: git diff HEAD~1)

## How mish enhances your signal

grep — raw bash dumps ANSI color codes into your context:

  raw:  \x1b[35msrc/foo.rs\x1b[0m:\x1b[32m42\x1b[0m:    let x = \x1b[1;31mpattern\x1b[0m;
  mish: src/foo.rs:42:    let x = pattern;

npm install — 150 progress lines become 1:

  raw:  ⠋ reify:lodash: timing reifyNode...  (x150 similar lines)
        added 247 packages in 12s
  mish: added 247 packages in 12s
        -- + 1 line (152 compressed) --

cargo test — passing tests are noise, failures are signal:

  raw:  test foo::bar ... ok  (x200)
        test foo::qux ... FAILED
        <failures section buried in 200 lines>
  mish: test foo::qux ... FAILED
        panicked at src/foo.rs:42
        test result: 1 passed; 1 failed

In each case: noise dampened, signal preserved, tokens saved.

## File operations

Use your platform's file tools (slipstream, Read/Edit, etc.) for file reads and
edits. Don't pipe through mish — it's for commands, not file content.

  WRONG: cat src/foo.py
  WRONG: sed -i 's/old/new/' src/foo.py
  WRONG: cat > file.py << 'EOF'
  RIGHT: slipstream / structured file tools for reads, edits, and creation

## Persistent sessions

For interactive interpreters (Python REPL, node, psql), use `mish session`:

  mish session start py --cmd python3
  mish session send py "import json; print(json.dumps({'a': 1}))"
  mish session send py "result = some_function(); print(result)"
  mish session close py

DON'T run `mish python3` bare — it opens a REPL that hangs.

## When commands fail

Don't fall back to bare bash. mish isn't broken — check your syntax and retry.
Raw output will waste more tokens than fixing the command.

## Piping and output capture

`mish <cmd>` (proxy mode) always compresses output. Do not pipe it:

  WRONG: mish grep pattern src/ | wc -l    (compressed count is wrong)
  RIGHT: mish grep pattern src/             (read the output directly)

`mish -c "cmd"` and `bash -c "cmd"` (when bash is symlinked to mish) detect
whether stdout is a terminal. If stdout is piped, redirected, or captured by a
subshell, mish execs the real shell directly — no headers, no dedup, no
compression. Output is byte-for-byte identical to /bin/bash.

  bash -c "cat patch.diff" | git apply     # safe — raw passthrough
  result=$(bash -c "python3 script.py")    # safe — raw passthrough
  bash -c "echo hello"                     # terminal — mish compresses

This means harnesses, test runners, and pipelines that invoke `bash -c` through
a mish symlink get clean output automatically. No special handling needed.

## Anti-patterns

  DON'T drop the mish prefix after a failure
  DON'T use cat/head/tail to read files — use file tools
  DON'T use sed/awk/heredoc to edit files — use file tools
  DON'T run mish python3 bare — use mish session for REPLs
  DON'T pipe mish proxy output to other commands — mish IS the pipe
"#;
