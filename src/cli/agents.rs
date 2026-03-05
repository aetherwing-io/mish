//! `mish --agents` — usage guide for LLM agents.

pub const AGENT_GUIDE: &str = r#"# mish agent guide

mish is an MCP server that gives you 5 tools for running and managing shell
commands. This guide teaches you WHEN and HOW to use each one.

## The 5 Tools

  sh_run      Run a command, wait for it to finish, get output
  sh_spawn    Start a background process, don't wait for it to finish
  sh_interact Send input / read output / signal a spawned process
  sh_session  Manage named PTY sessions
  sh_help     Show the reference card (tool params, watch presets, limits)

## sh_run — synchronous commands

Use for commands that finish quickly and whose output you need now.

  GOOD: sh_run("ls -la src/")
  GOOD: sh_run("cargo check 2>&1")
  GOOD: sh_run("git diff HEAD~1")
  GOOD: sh_run("python3 -c 'print(1+1)'")

  BAD:  sh_run("cargo build")          — may take minutes, blocks you
  BAD:  sh_run("npm start")            — server never exits, you'll timeout
  BAD:  sh_run("tail -f /var/log/sys") — infinite stream, you'll timeout

Rule: if a command might take more than ~30s or runs forever, use sh_spawn.

### watch — filter noisy output

  sh_run("make test", watch="FAIL|error")        — only lines matching regex
  sh_run("cargo test", watch="@errors")           — use a preset filter
  sh_run("cargo test", watch="@errors", unmatched="drop")  — hide non-matches

## sh_spawn — background processes

Use for anything long-running: builds, servers, watchers, test suites.

  sh_spawn(alias="build", cmd="cargo build 2>&1")
  sh_spawn(alias="server", cmd="python3 -m http.server 8080")
  sh_spawn(alias="tests", cmd="cargo test 2>&1", wait_for="test result")

### wait_for — confirm startup before moving on

  sh_spawn(alias="api", cmd="node server.js", wait_for="listening on port")

  wait_for is a regex. sh_spawn returns as soon as a matching line appears.
  This lets you confirm a server is ready before using it.

### Naming aliases

  GOOD: alias="build", alias="api", alias="db"    — short, meaningful
  BAD:  alias="process1", alias="thing"            — unhelpful when you have 5

## sh_interact — work with spawned processes

After sh_spawn, use sh_interact to monitor and control the process.

### read_tail — check recent output

  sh_interact(alias="build", action="read_tail")
  sh_interact(alias="build", action="read_tail", lines=100)

  This is your main feedback loop. Spawn something, do other work, then
  read_tail to see what happened.

### send — provide input

  sh_interact(alias="db", action="send", input="SELECT 1;\n")

  Always include \n — that's the Enter key.

### signal / kill — manage lifecycle

  sh_interact(alias="server", action="signal", input="SIGINT")
  sh_interact(alias="server", action="kill")

### status — check if still running

  sh_interact(alias="build", action="status")

## sh_session — PTY session lifecycle

  sh_session(action="list")   — see all active sessions and processes

  The process table digest is included in every response, so you rarely
  need to call this explicitly. It's there when you need a full picture.

## Patterns

### 1. Fire and check back

  Start a build, do other work, check later:

    sh_spawn(alias="build", cmd="cargo build 2>&1")
    ... edit files, read docs, whatever ...
    sh_interact(alias="build", action="read_tail")

  DO NOT sh_run a long build and sit there waiting.

### 2. Start server, confirm ready, then use it

    sh_spawn(alias="api", cmd="./start-server.sh", wait_for="ready on :3000")
    sh_run("curl localhost:3000/health")

### 3. Run tests, filter to failures

    sh_run("cargo test 2>&1", watch="FAILED|panicked|error\\[")

  Or for a large test suite:

    sh_spawn(alias="tests", cmd="cargo test 2>&1", wait_for="test result")
    sh_interact(alias="tests", action="read_tail")

### 4. Interactive REPL (python, node, psql)

    sh_spawn(alias="py", cmd="python3")
    sh_interact(alias="py", action="send", input="import json\n")
    sh_interact(alias="py", action="send", input="json.dumps({'a': 1})\n")
    sh_interact(alias="py", action="read_tail")

### 5. Multiple concurrent tasks

    sh_spawn(alias="build-fe", cmd="cd frontend && npm run build 2>&1")
    sh_spawn(alias="build-be", cmd="cargo build 2>&1")
    ... later ...
    sh_interact(alias="build-fe", action="read_tail")
    sh_interact(alias="build-be", action="read_tail")

## Anti-Patterns

  DON'T sh_run a server or daemon        — it blocks until timeout
  DON'T sh_run a build you can't predict  — spawn it, check back
  DON'T poll in a tight loop              — use wait_for on spawn
  DON'T ignore the process table          — every response shows it
  DON'T spawn without a meaningful alias  — you'll lose track
  DON'T send input without \n             — nothing happens without Enter
  DON'T sh_run("cat bigfile.txt")         — use your file-read tools instead
  DON'T sh_run("sleep 60 && cmd")         — spawn it, interact later

## Output Behavior

mish squashes output to save tokens:
  - ANSI escape codes are stripped
  - Progress bars and spinners are removed
  - Repeated/similar lines are deduplicated
  - Long output is truncated with head/tail (Oreo style)

Use watch="" to filter further. Use read_tail with lines=N for more context.

## Process Table Digest

Every mish response includes a process table showing all running processes,
their PIDs, aliases, uptimes, and states. Use this ambient awareness instead
of manually checking each process. If something exited or errored, you'll
see it without asking.
"#;
