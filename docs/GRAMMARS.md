# Tool Grammars

## Overview

Tool grammars are declarative rule files that teach mish how to classify output from known tools. Each grammar contains patterns for noise, hazards, outcomes, and summary templates.

Grammars are loaded at invocation time based on the command being executed. When no grammar matches, the classifier falls back to universal patterns and structural heuristics.

## Schema

```
Grammar {
    tool: String                          // identifier
    detect: Vec<String>                   // command prefixes that activate this grammar
    inherit: Vec<String>                  // shared grammars to compose
    global_noise: Vec<Pattern>            // patterns stripped regardless of action
    actions: HashMap<String, Action>      // keyed by subcommand
    fallback: Option<Action>              // when subcommand isn't recognized
}

Action {
    detect: Vec<String>                   // subcommand tokens that trigger this action
    noise: Vec<Rule>
    hazard: Vec<Rule>
    outcome: Vec<Rule>
    summary: SummaryTemplate
}

Rule {
    pattern: Regex
    action: RuleAction                    // strip, dedup, keep, promote
    severity: Option<Severity>            // for hazard rules: error, warning
    captures: Vec<String>                 // named capture groups for template vars
    multiline: Option<u32>               // attach N subsequent lines
}

enum RuleAction {
    Strip,                                // discard entirely
    Dedup,                                // send to dedup engine
    Keep,                                 // emit as-is
    Promote,                              // extract to summary
}

SummaryTemplate {
    success: String                       // template with {var} placeholders
    failure: String
    partial: String                       // while process still running
}
```

## File Format

Grammars use TOML. Each tool gets one file.

```
grammars/
├── _shared/
│   ├── ansi-progress.toml
│   ├── node-stacktrace.toml
│   ├── python-traceback.toml
│   └── c-compiler-output.toml
├── npm.toml
├── cargo.toml
├── git.toml
├── docker.toml
├── make.toml
├── gcc.toml
├── go.toml
├── pip.toml
├── pytest.toml
├── jest.toml
├── kubectl.toml
├── terraform.toml
├── apt.toml
├── brew.toml
├── curl.toml
├── rsync.toml
├── ssh.toml
├── systemctl.toml
├── ansible.toml
└── webpack.toml
```

## Rule Evaluation Order

Within a grammar, rules are evaluated in this fixed order. First match wins.

```
1. Hazard rules      // never suppress an error
2. Outcome rules     // extract summary-worthy info
3. Noise rules       // strip or dedup
4. (no match)        // falls to universal patterns / structural heuristics
```

Hazard-first is non-negotiable. A noise rule must never accidentally swallow an error that happens to match a noise pattern.

## Shared Grammars

Shared grammars contain patterns common across multiple tools. They are composed into tool grammars via `inherit`.

Inherited rules are evaluated **after** the tool's own rules. This allows a tool grammar to override shared behavior when needed.

### ansi-progress.toml

```toml
[tool]
name = "ansi-progress"

[[global_noise]]
# CR without LF — line overwriting (spinners, progress bars)
# Note: this is also handled at the LineBuffer level,
# but kept here for grammars that process log files.
pattern = '\r[^\n]'
action = "strip"

[[global_noise]]
# Common progress bar shapes
pattern = '[\[({][\s#=█░▓►>.\-]{4,}[\])}]'
action = "strip"

[[global_noise]]
# Standalone percentage (mid-stream, not final)
pattern = '^\s*\d{1,3}%\s*$'
action = "strip"
```

### node-stacktrace.toml

```toml
[tool]
name = "node-stacktrace"

[[global_noise]]
# Node.js stack trace frames — dedup (keep first + last, count middle)
pattern = '^\s+at\s+\S+'
action = "dedup"
```

### python-traceback.toml

```toml
[tool]
name = "python-traceback"

[[global_noise]]
# Python traceback frames
pattern = '^\s+File ".+", line \d+'
action = "dedup"

[[global_noise]]
# The code line after "File" line in tracebacks
pattern = '^\s{4}\S'
action = "dedup"
```

### c-compiler-output.toml

```toml
[tool]
name = "c-compiler-output"

# Note: hazard rules so they're evaluated first
[[global_noise]]
# Individual compile commands echoed by make
pattern = '^(gcc|g\+\+|cc|c\+\+|clang|clang\+\+)\s+'
action = "strip"
```

## Tool Grammars

### npm.toml

```toml
[tool]
name = "npm"
detect = ["npm", "npx"]
inherit = ["ansi-progress", "node-stacktrace"]

[[global_noise]]
pattern = '^npm (timing|http|sill|verb)'
action = "strip"

# ---- install action ----

[actions.install]
detect = ["install", "i", "add", "ci"]

[[actions.install.noise]]
pattern = '^(idealTree|reify|resolv)'
action = "strip"

[[actions.install.noise]]
pattern = '^npm warn'
action = "dedup"

[[actions.install.outcome]]
pattern = '^added (?P<count>\d+) packages? in (?P<time>.+)'
action = "promote"
captures = ["count", "time"]

[[actions.install.outcome]]
pattern = '^up to date'
action = "promote"

[[actions.install.hazard]]
pattern = 'ERESOLVE'
severity = "error"
action = "keep"

[[actions.install.hazard]]
pattern = 'EACCES|EPERM'
severity = "error"
action = "keep"

[[actions.install.hazard]]
pattern = '(?P<count>\d+) vulnerabilit'
severity = "warning"
action = "keep"
captures = ["count"]

[actions.install.summary]
success = "+ {count} packages installed ({time})"
failure = "! npm install failed (exit {exit_code})"
partial = "... installing ({lines} lines)"

# ---- test action ----

[actions.test]
detect = ["test", "t"]

[[actions.test.outcome]]
pattern = 'Tests?:\s+(?P<passed>\d+) passed'
action = "promote"
captures = ["passed"]

[[actions.test.outcome]]
pattern = '(?P<failed>\d+) failed'
action = "promote"
captures = ["failed"]

[[actions.test.hazard]]
pattern = '^\s*(FAIL|✕|✗|×)\s+'
severity = "error"
action = "keep"
multiline = 5

[actions.test.summary]
success = "+ {passed} tests passed"
failure = "! {failed} tests failed"
partial = "... running tests ({lines} lines)"

# ---- run action (generic scripts) ----

[actions.run]
detect = ["run", "run-script", "start"]

# npm run is too generic to have specific rules
# falls through to universal patterns + structural heuristics

[actions.run.summary]
success = "+ script completed"
failure = "! script failed (exit {exit_code})"
partial = "... running ({lines} lines)"
```

### cargo.toml

```toml
[tool]
name = "cargo"
detect = ["cargo"]
inherit = ["ansi-progress"]

[[global_noise]]
# Individual crate compile lines
pattern = '^\s*(Compiling|Downloading|Downloaded)\s+'
action = "dedup"

[[global_noise]]
# Fetch/update lines
pattern = '^\s*(Updating|Fetching)\s+'
action = "strip"

# ---- build action ----

[actions.build]
detect = ["build", "b"]

[[actions.build.outcome]]
pattern = '^\s*Finished\s+.*in\s+(?P<time>.+)'
action = "promote"
captures = ["time"]

[[actions.build.hazard]]
pattern = '^error\[(?P<code>E\d+)\]:\s+(?P<msg>.+)'
severity = "error"
action = "keep"
captures = ["code", "msg"]
multiline = 3

[[actions.build.hazard]]
pattern = '^warning(\[.+\])?:\s+(?P<msg>.+)'
severity = "warning"
action = "dedup"
captures = ["msg"]

[actions.build.summary]
success = "+ built in {time}"
failure = "! build failed (exit {exit_code})"
partial = "... compiling ({lines} lines)"

# ---- test action ----

[actions.test]
detect = ["test", "t"]

[[actions.test.noise]]
pattern = '^\s*running \d+ tests?$'
action = "strip"

[[actions.test.noise]]
pattern = '^test .+ \.\.\. ok$'
action = "dedup"

[[actions.test.outcome]]
pattern = '^test result: ok\. (?P<passed>\d+) passed'
action = "promote"
captures = ["passed"]

[[actions.test.outcome]]
pattern = '^test result: FAILED\. .+ (?P<failed>\d+) failed'
action = "promote"
captures = ["failed"]

[[actions.test.hazard]]
pattern = '^test .+ \.\.\. FAILED$'
severity = "error"
action = "keep"
multiline = 10

[actions.test.summary]
success = "+ {passed} tests passed"
failure = "! {failed} tests failed"
partial = "... testing ({lines} lines)"
```

### git.toml

```toml
[tool]
name = "git"
detect = ["git"]
inherit = ["ansi-progress"]

# ---- push action ----

[actions.push]
detect = ["push"]

[[actions.push.noise]]
pattern = '^(Enumerating|Counting|Compressing|Writing) objects'
action = "strip"

[[actions.push.noise]]
pattern = '^Delta compression'
action = "strip"

[[actions.push.noise]]
pattern = '^remote: (Enumerating|Counting|Compressing)'
action = "strip"

[[actions.push.outcome]]
pattern = '(?P<src>\S+)\s+->\s+(?P<dst>\S+)'
action = "promote"
captures = ["src", "dst"]

[[actions.push.hazard]]
pattern = '^\s*!\s+\[rejected\]'
severity = "error"
action = "keep"

[[actions.push.hazard]]
pattern = 'error: failed to push'
severity = "error"
action = "keep"

[actions.push.summary]
success = "+ pushed {src} → {dst}"
failure = "! push rejected"
partial = "... pushing"

# ---- pull action ----

[actions.pull]
detect = ["pull", "fetch"]

[[actions.pull.noise]]
pattern = '^(Unpacking|remote:) '
action = "strip"

[[actions.pull.outcome]]
pattern = '(?P<count>\d+) files? changed'
action = "promote"
captures = ["count"]

[[actions.pull.outcome]]
pattern = 'Already up to date'
action = "promote"

[[actions.pull.hazard]]
pattern = 'CONFLICT'
severity = "error"
action = "keep"

[[actions.pull.hazard]]
pattern = 'error: Your local changes'
severity = "error"
action = "keep"

[actions.pull.summary]
success = "+ pulled ({count} files changed)"
failure = "! pull failed"
partial = "... pulling"

# ---- clone action ----

[actions.clone]
detect = ["clone"]

[[actions.clone.noise]]
pattern = '^(Cloning into|Receiving|Resolving)'
action = "strip"

[[actions.clone.outcome]]
pattern = '^Cloning into .(?P<dir>[^.]+).'
action = "promote"
captures = ["dir"]

[actions.clone.summary]
success = "+ cloned into {dir}"
failure = "! clone failed"
partial = "... cloning"
```

### docker.toml

```toml
[tool]
name = "docker"
detect = ["docker"]
inherit = ["ansi-progress"]

# ---- build action ----

[actions.build]
detect = ["build", "buildx"]

[[actions.build.noise]]
pattern = '^\s*#\d+\s+(CACHED|DONE)'
action = "dedup"

[[actions.build.noise]]
pattern = '^\s*#\d+\s+\d+\.\d+\s'
action = "strip"

[[actions.build.outcome]]
pattern = 'exporting to image'
action = "promote"

[[actions.build.outcome]]
pattern = 'writing image sha256:(?P<hash>[0-9a-f]{12})'
action = "promote"
captures = ["hash"]

[[actions.build.hazard]]
pattern = '^(ERROR|error)\s'
severity = "error"
action = "keep"
multiline = 3

[[actions.build.hazard]]
pattern = 'DEPRECATED'
severity = "warning"
action = "keep"

[actions.build.summary]
success = "+ built image {hash}"
failure = "! docker build failed"
partial = "... building ({lines} lines)"

# ---- compose up ----

[actions.up]
detect = ["compose up", "up"]

[[actions.up.noise]]
pattern = '^\s*(Creating|Pulling|Starting)\s+'
action = "dedup"

[[actions.up.outcome]]
pattern = '(?P<name>\S+)\s+(Started|Running)'
action = "promote"
captures = ["name"]

[[actions.up.hazard]]
pattern = '(error|Error|exited with code [^0])'
severity = "error"
action = "keep"

[actions.up.summary]
success = "+ {name} started"
failure = "! compose up failed"
partial = "... starting containers"
```

### make.toml

```toml
[tool]
name = "make"
detect = ["make", "gmake"]
inherit = ["ansi-progress", "c-compiler-output"]

[fallback]

[[fallback.noise]]
# Recipe echo lines (commands being run)
pattern = '^\s*(gcc|g\+\+|cc|clang|ar|ld|ranlib|strip|install)\s+'
action = "dedup"

[[fallback.noise]]
# Make entering/leaving directory
pattern = '^make(\[\d+\])?: (Entering|Leaving) directory'
action = "strip"

[[fallback.outcome]]
pattern = '^make(\[\d+\])?: Nothing to be done'
action = "promote"

[[fallback.hazard]]
pattern = '^make(\[\d+\])?: \*\*\*'
severity = "error"
action = "keep"

[[fallback.hazard]]
# Compiler errors (file:line:col: error:)
pattern = '^\S+:\d+:\d+:\s+error:'
severity = "error"
action = "keep"
multiline = 3

[[fallback.hazard]]
# Compiler warnings
pattern = '^\S+:\d+:\d+:\s+warning:'
severity = "warning"
action = "dedup"

[[fallback.hazard]]
# Linker errors
pattern = 'undefined reference to'
severity = "error"
action = "keep"

[fallback.summary]
success = "+ make complete"
failure = "! make failed (exit {exit_code})"
partial = "... building ({lines} lines)"
```

### pytest.toml

```toml
[tool]
name = "pytest"
detect = ["pytest", "py.test", "python -m pytest"]
inherit = ["ansi-progress", "python-traceback"]

[fallback]

[[fallback.noise]]
# Collecting tests
pattern = '^(collecting|collected)\s+'
action = "strip"

[[fallback.noise]]
# Individual PASSED lines
pattern = '^.*PASSED\s*$'
action = "dedup"

[[fallback.outcome]]
pattern = '(?P<passed>\d+) passed'
action = "promote"
captures = ["passed"]

[[fallback.outcome]]
pattern = '(?P<failed>\d+) failed'
action = "promote"
captures = ["failed"]

[[fallback.outcome]]
pattern = '(?P<skipped>\d+) skipped'
action = "promote"
captures = ["skipped"]

[[fallback.hazard]]
pattern = '^(FAILED|ERROR)\s+'
severity = "error"
action = "keep"
multiline = 10

[[fallback.hazard]]
pattern = '^(E\s+|>\s+assert)'
severity = "error"
action = "keep"

[fallback.summary]
success = "+ {passed} passed"
failure = "! {failed} failed, {passed} passed"
partial = "... testing ({lines} lines)"
```

### jest.toml

```toml
[tool]
name = "jest"
detect = ["jest", "npx jest", "yarn jest", "pnpm jest"]
inherit = ["ansi-progress", "node-stacktrace"]

[fallback]

[[fallback.noise]]
# Individual PASS suite lines
pattern = '^\s*(PASS)\s+'
action = "dedup"

[[fallback.noise]]
# Test timing lines
pattern = '^\s*Time:\s+'
action = "strip"

[[fallback.outcome]]
pattern = 'Tests?:\s+(?P<passed>\d+) passed'
action = "promote"
captures = ["passed"]

[[fallback.outcome]]
pattern = '(?P<failed>\d+) failed'
action = "promote"
captures = ["failed"]

[[fallback.outcome]]
pattern = 'Test Suites:\s+(?P<suites>\d+) passed'
action = "promote"
captures = ["suites"]

[[fallback.hazard]]
pattern = '^\s*(FAIL)\s+'
severity = "error"
action = "keep"
multiline = 5

[[fallback.hazard]]
pattern = '^\s*(●|✕|✗)\s+'
severity = "error"
action = "keep"
multiline = 5

[fallback.summary]
success = "+ {passed} tests passed ({suites} suites)"
failure = "! {failed} failed, {passed} passed"
partial = "... testing ({lines} lines)"
```

## Adding a New Grammar

1. Create `grammars/toolname.toml`
2. Set `detect` to the command(s) that trigger it
3. Add `inherit` for any shared patterns that apply
4. Define actions for each subcommand you want to handle
5. For each action, add rules in priority order: hazards first, then outcomes, then noise
6. Write summary templates using `{variable}` placeholders from captured groups
7. Test against real captured output (see testing section in SPEC)

### Minimal grammar template

```toml
[tool]
name = "mytool"
detect = ["mytool"]
inherit = ["ansi-progress"]

[fallback]

[[fallback.hazard]]
pattern = '(error|Error|ERROR)'
severity = "error"
action = "keep"

[[fallback.hazard]]
pattern = '(warn|Warning|WARNING)'
severity = "warning"
action = "dedup"

[fallback.summary]
success = "+ mytool complete"
failure = "! mytool failed (exit {exit_code})"
partial = "... running ({lines} lines)"
```
