# Classification Engine

## Overview

The classifier assigns a classification to every line of terminal output. It operates in three tiers, evaluated in order. First match wins at each tier; if no tier matches, the line is classified as `Unknown` and buffered.

```
Line arrives
    │
    ▼
┌────────────────┐
│ Line type?     │──── Overwrite ───→ Noise(Strip)
│                │──── Partial ────→ check prompt patterns
└───────┬────────┘
        │ Complete
        ▼
┌────────────────┐
│ Tier 1:        │──── match ──→ Classified (grammar-specific)
│ Tool grammar   │
└───────┬────────┘
        │ no match
        ▼
┌────────────────┐
│ Tier 2:        │──── match ──→ Classified (universal)
│ Universal      │
└───────┬────────┘
        │ no match
        ▼
┌────────────────┐
│ Tier 3:        │──── match ──→ Classified (structural)
│ Structural     │
└───────┬────────┘
        │ no match
        ▼
    Unknown (buffer)
```

## Tier 1: Tool Grammar Rules

When a grammar is loaded (tool was detected from the command), its rules are evaluated in this fixed order:

### 1a. Hazard rules

Evaluated first. If any hazard pattern matches, the line is immediately classified as a hazard with the associated severity. This prevents noise rules from accidentally suppressing errors.

```rust
for rule in grammar.current_action.hazard_rules {
    if rule.pattern.is_match(&line) {
        return Classification::Hazard {
            severity: rule.severity,
            text: line,
            captures: rule.extract_captures(&line),
        };
    }
}
```

**Multiline attachment:** When a hazard rule has `multiline = N`, the next N lines are unconditionally attached to this hazard, regardless of what they contain. This captures stack traces, error context, and assertion details that follow an error header.

### 1b. Outcome rules

Matched second. These are lines that carry information needed for the final summary (e.g., "added 147 packages in 12.3s"). They're stored, not immediately emitted.

```rust
for rule in grammar.current_action.outcome_rules {
    if rule.pattern.is_match(&line) {
        return Classification::Outcome {
            text: line,
            captures: rule.extract_captures(&line),
        };
    }
}
```

### 1c. Noise rules

Matched last within the grammar. Lines that match are either silently discarded (`Strip`) or sent to the dedup engine (`Dedup`).

```rust
for rule in grammar.current_action.noise_rules {
    if rule.pattern.is_match(&line) {
        return Classification::Noise {
            action: rule.action, // Strip or Dedup
        };
    }
}
```

### Global noise rules

`global_noise` rules from the grammar (and its inherited grammars) are checked as part of the noise tier. They apply regardless of which action is active.

Evaluation order within noise:
1. Action-specific noise rules
2. Grammar global noise rules
3. Inherited grammar global noise rules (in inheritance order)

## Tier 2: Universal Patterns

These patterns are always active, regardless of whether a tool grammar is loaded. They catch common conventions that nearly all tools follow.

### 2a. ANSI Color Classification

ANSI color codes are a strong classification signal. Tools use them intentionally to convey severity.

```
\x1b[31m  →  red    →  error signal
\x1b[91m  →  bright red → error signal
\x1b[33m  →  yellow →  warning signal
\x1b[93m  →  bright yellow → warning signal
\x1b[32m  →  green  →  success signal
\x1b[92m  →  bright green → success signal
\x1b[36m  →  cyan   →  info (usually noise)
```

Color alone doesn't classify. It's combined with content:

```rust
fn classify_with_color(line: &str, colors: &[AnsiColor]) -> Option<Classification> {
    let clean = strip_ansi(line);

    // Red + error-like content → Hazard(Error)
    if colors.contains(&Red) && has_error_keywords(&clean) {
        return Some(Classification::Hazard { severity: Error, .. });
    }

    // Yellow + warning-like content → Hazard(Warning)
    if colors.contains(&Yellow) && has_warning_keywords(&clean) {
        return Some(Classification::Hazard { severity: Warning, .. });
    }

    // Green + success-like content → Outcome
    if colors.contains(&Green) && has_success_keywords(&clean) {
        return Some(Classification::Outcome { .. });
    }

    None
}
```

### 2b. Error Keywords

Patterns that indicate errors regardless of tool:

```
# Line-start anchored (strongest signal)
^(error|ERROR|Error)[:\s]
^(FAIL|FAILED|FATAL|fatal|Fatal)[:\s]
^(panic|Panic|PANIC)[:\s]
^Traceback \(most recent call last\)
^Exception
^Unhandled\s

# Anywhere in line (weaker, require additional context)
command not found$
permission denied
No such file or directory
ENOENT|EACCES|EPERM|ECONNREFUSED
segmentation fault|SIGSEGV
killed|OOMKilled
out of memory
cannot find module
ModuleNotFoundError
ImportError
SyntaxError
```

### 2c. Warning Keywords

```
^(warn|WARN|Warning|WARNING|DEPRECAT)[:\s!]
\bdeprecated\b
⚠
# npm/yarn audit
\b(moderate|high|critical)\s+vulnerabilit
```

### 2d. Stack Trace Detection

Stack traces are multi-line error details. Detect the start pattern, then consume subsequent indented lines.

```
# Node.js
^\s+at\s+\S+\s+\(.*:\d+:\d+\)
^\s+at\s+\S+\s+\(node:

# Python
^\s+File ".+", line \d+
^\s{4}\S  (code line in traceback)

# Go
^goroutine \d+
^\s+.+\.go:\d+

# Rust
^\s+\d+:\s+0x[0-9a-f]+\s+-\s+

# Java/Kotlin
^\s+at\s+[\w.$]+\([\w.]+:\d+\)
^Caused by:

# Generic (file:line:col pattern)
^\S+:\d+:\d+:
```

When a stack trace start is detected, switch to "consuming" mode: subsequent lines that are indented or match continuation patterns are attached to the trace. The trace ends when a non-matching, non-indented line appears.

Stack trace compression:
```
Input:
  at UserService.getUser (src/user.ts:42:5)
  at AuthMiddleware.handle (src/auth.ts:18:12)
  at Router.dispatch (node_modules/express/lib/router.js:73:3)
  at Layer.handle (node_modules/express/lib/router.js:284:5)
  ... 8 more frames ...
  at main (src/index.ts:8:1)

Output:
  at UserService.getUser (src/user.ts:42:5)
  ... 10 frames
  at main (src/index.ts:8:1)
```

Rule: keep first frame (most specific/useful), keep last frame (entry point), count the middle.

### 2e. Prompt Detection

Lines that indicate the process is waiting for user input.

```
# Explicit prompt patterns
\?\s*$                        # ends with ?
\(y\/n\)|\(Y\/N\)|\[y\/N\]   # choice brackets
[Pp]assword:?\s*$             # password prompt
[Ee]nter\s.*:\s*$             # Enter something:
[Pp]ress any key              # Press any key
\S+>\s*$                      # REPL prompt (node>, irb>)
:\s*$                         # generic colon prompt
\$\s*$                        # shell prompt
```

Prompt detection requires **both** a pattern match **and** the line being `Partial` (no newline terminator after 500ms timeout). A line containing "?" that ends with `\n` is just a question in log output, not a prompt.

## Tier 3: Structural Heuristics

These rules operate on the **shape** of lines rather than their content. They handle unknown tools and unmatched lines from known tools.

### 3a. Decorative Lines

Lines that are purely visual separators with no semantic content:

```
# All same character (with optional whitespace)
^[\s=\-─━_*#~.·•]{3,}$

# Box-drawing characters
^[┌┐└┘├┤┬┴┼│─]+$

# Empty or whitespace-only
^\s*$
```

Classification: `Noise(Strip)`

### 3b. Edit-Distance Similarity

For consecutive lines that aren't caught by grammar rules, compute normalized edit distance:

```rust
fn is_similar(current: &str, previous: &str) -> bool {
    let max_len = current.len().max(previous.len());
    if max_len == 0 { return true; }

    let distance = levenshtein(current, previous);
    let normalized = distance as f64 / max_len as f64;

    normalized < 0.3  // less than 30% different
}
```

When lines are similar, they're grouped and deduped. This catches repetitive output from unknown tools without needing a grammar rule.

Optimization: don't compute edit distance for every pair. Only check when:
- Lines are similar length (within 2x of each other)
- Lines start with the same first word/token

### 3c. Volume-Based Compression

When lines accumulate without any hazard/outcome classification, compress by count:

```rust
const VOLUME_THRESHOLD: usize = 10;

// If we've buffered N consecutive Unknown lines
// with no hazard interruption, collapse to count
if unknown_buffer.len() >= VOLUME_THRESHOLD {
    emit(format!("  ... {} lines", unknown_buffer.len()));
    unknown_buffer.clear();
}
```

### 3d. Temporal Heuristics

Lines that arrive after a gap are more likely to be meaningful:

```rust
const SILENCE_THRESHOLD: Duration = Duration::from_secs(2);

if now - last_line_time > SILENCE_THRESHOLD {
    // This line broke a silence — probably significant
    // Boost it: don't auto-classify as noise even if
    // edit distance suggests similarity
    bypass_structural_noise = true;
}
```

Lines that arrive in rapid bursts (>50 lines/sec) are more likely to be noise. This doesn't auto-classify them, but it lowers the threshold for structural dedup.

### 3e. Last Lines Before Exit

The final 3-5 lines before a process exits are almost always the summary or result. These are preserved regardless of other classification.

```rust
// Ring buffer of last 5 lines, maintained continuously
ring_buffer.push(line);

// On process exit, if no grammar produced a summary:
if !has_outcome {
    for line in ring_buffer.iter() {
        emit(format!("  last: {}", line));
    }
}
```

This is the ultimate fallback for unknown tools. Even without understanding the output, the last lines + exit code gives a useful summary.

## ANSI Handling

ANSI escape sequences are processed in a dedicated pass before classification:

```rust
struct AnsiMetadata {
    colors: Vec<AnsiColor>,       // colors used in this line
    has_cursor_movement: bool,    // CSI sequences for cursor positioning
    has_erase: bool,              // erase line/screen sequences
    clean_text: String,           // text with all ANSI stripped
}

fn process_ansi(raw: &str) -> AnsiMetadata {
    // Extract color info for classification boost
    // Strip sequences for pattern matching
    // Detect cursor movement (additional progress indicator)
}
```

Pattern matching always runs against `clean_text` (ANSI stripped). Color metadata is used as a classification signal in Tier 2, not as pattern content.

## State Machine

The classifier maintains a state machine for process lifecycle:

```
        IDLE
          │
          │ [first line received]
          ▼
       RUNNING ◄──────────────────────────┐
          │                                │
          ├── [silence > 500ms] ──▶ MAYBE_PROMPT
          │                           │
          │                    [output resumes]
          │                           │
          │◄──────────────────────────┘
          │
          ├── [prompt pattern + Partial line] ──▶ AWAITING_INPUT
          │                                          │
          │                                   [stdin received]
          │                                          │
          │◄─────────────────────────────────────────┘
          │
          ├── [exit code 0] ──────▶ DONE_SUCCESS
          ├── [exit code != 0] ───▶ DONE_FAILURE
          ├── [signal received] ──▶ DONE_KILLED
          └── [multiline hazard] ─▶ CONSUMING_TRACE
                                       │
                                [non-matching line]
                                       │
                                       ▼
                                    RUNNING
```

State affects classification:
- `CONSUMING_TRACE`: all lines are attached to the current hazard, regardless of content
- `AWAITING_INPUT`: emit prompt classification, pause flush timers
- `DONE_*`: trigger final flush and summary generation
