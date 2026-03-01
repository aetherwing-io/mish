# Streaming Buffer & PTY Capture

## Overview

The streaming layer handles the real-time mechanics: spawning processes in a PTY, assembling bytes into lines, managing flush timing, and producing the final condensed output. This is the runtime glue that connects PTY capture → line buffering → classification → emission.

## PTY Capture

### Why PTY

A pipe changes program behavior. Many tools detect `isatty()` and switch to a machine-readable (but different) output format:

- npm: disables colors, changes progress display
- cargo: disables colors, removes progress bar
- git: disables pager, changes diff format
- ls: switches to single-column
- grep: disables color highlighting

mish needs to see what the user would see. A PTY makes the child process believe it's connected to a real terminal.

### Spawn sequence

```rust
fn spawn(args: &[String]) -> Result<PtyProcess> {
    // 1. Query real terminal dimensions
    let (cols, rows) = terminal_size()?;

    // 2. Create PTY pair
    let (master, slave) = openpty(cols, rows)?;

    // 3. Fork
    match fork()? {
        Child => {
            // Close master side
            close(master);

            // Set slave as stdin/stdout/stderr
            dup2(slave, STDIN);
            dup2(slave, STDOUT);
            dup2(slave, STDERR);

            // Create new session (detach from parent terminal)
            setsid();

            // Set controlling terminal
            ioctl(slave, TIOCSCTTY);

            // Exec the command
            exec(args[0], &args);
        }
        Parent(child_pid) => {
            // Close slave side
            close(slave);

            Ok(PtyProcess {
                master_fd: master,
                child_pid,
                start_time: Instant::now(),
            })
        }
    }
}
```

### SIGWINCH forwarding

If the real terminal resizes while mish is running, forward the new dimensions to the PTY:

```rust
// Register SIGWINCH handler
signal(SIGWINCH, || {
    let (cols, rows) = terminal_size().unwrap();
    pty.resize(cols, rows);
});
```

### stdin forwarding

User input must pass through to the child unmodified. This is critical for interactive prompts (password entry, y/n questions, REPLs).

```rust
// In the event loop:
if poll_stdin_ready() {
    let n = read(STDIN, &mut buf);
    write(pty.master_fd, &buf[..n]);
}
```

## Event Loop

Single-threaded async loop using `poll`/`epoll` (or tokio for async Rust):

```rust
enum Event {
    PtyOutput(Vec<u8>),          // child wrote to stdout/stderr
    StdinInput(Vec<u8>),         // user typed something
    ChildExited(ExitStatus),     // child process ended
    PartialLineTimeout,          // 500ms since last incomplete line
    SilenceTimeout,              // 2s since any output
    FlushTimer,                  // 5s periodic flush
    Signal(Signal),              // SIGWINCH, SIGINT, etc.
}

fn run(command: &[String], config: &Config) -> Result<Summary> {
    let pty = spawn(command)?;
    let mut line_buffer = LineBuffer::new();
    let mut classifier = Classifier::new(config);
    let mut emit_buffer = EmitBuffer::new();

    loop {
        match poll_events(&pty)? {
            PtyOutput(bytes) => {
                let lines = line_buffer.ingest(&bytes);
                for line in lines {
                    let classified = classifier.classify(line);
                    emit_buffer.accept(classified);
                }
            }

            StdinInput(bytes) => {
                pty.write_stdin(&bytes)?;
                // If we were in AWAITING_INPUT state,
                // transition back to RUNNING
                classifier.resume_from_prompt();
            }

            ChildExited(status) => {
                // Drain any remaining bytes from PTY
                let remaining = pty.drain()?;
                let lines = line_buffer.finalize(&remaining);
                for line in lines {
                    let classified = classifier.classify(line);
                    emit_buffer.accept(classified);
                }

                return Ok(emit_buffer.finalize(status));
            }

            PartialLineTimeout => {
                if let Some(partial) = line_buffer.emit_partial() {
                    let classified = classifier.classify(partial);
                    emit_buffer.accept(classified);
                }
            }

            SilenceTimeout => {
                emit_buffer.flush_pending();
            }

            FlushTimer => {
                emit_buffer.flush_pending();
            }

            Signal(SIGWINCH) => {
                let (cols, rows) = terminal_size()?;
                pty.resize(cols, rows)?;
            }

            Signal(SIGINT) => {
                // Forward to child, don't exit mish
                pty.signal(SIGINT)?;
            }
        }
    }
}
```

## Line Buffer

### State

```rust
struct LineBuffer {
    partial: Vec<u8>,
    overwrite_mode: bool,
    last_byte_time: Instant,
    partial_timeout: Duration,    // 500ms default
}
```

### Byte processing

```rust
impl LineBuffer {
    fn ingest(&mut self, bytes: &[u8]) -> Vec<Line> {
        let mut lines = Vec::new();
        self.last_byte_time = Instant::now();

        for &byte in bytes {
            match byte {
                b'\n' => {
                    let content = String::from_utf8_lossy(&self.partial).to_string();
                    self.partial.clear();

                    if self.overwrite_mode {
                        // CR was followed by content then LF
                        // This is the final state of an overwritten line
                        // Emit as Overwrite — the classifier will strip it
                        lines.push(Line::Overwrite(content));
                        self.overwrite_mode = false;
                    } else {
                        lines.push(Line::Complete(content));
                    }
                }

                b'\r' => {
                    if !self.partial.is_empty() {
                        // Had content before CR — this is a rewrite
                        // Discard the previous content
                        self.partial.clear();
                    }
                    self.overwrite_mode = true;
                }

                _ => {
                    self.partial.push(byte);
                }
            }
        }

        lines
    }

    fn emit_partial(&mut self) -> Option<Line> {
        if self.partial.is_empty() {
            return None;
        }

        if self.last_byte_time.elapsed() >= self.partial_timeout {
            let content = String::from_utf8_lossy(&self.partial).to_string();
            // Don't clear — the process might still add to this line
            // But emit what we have for prompt detection
            Some(Line::Partial(content))
        } else {
            None
        }
    }

    fn finalize(&mut self, remaining: &[u8]) -> Vec<Line> {
        let mut lines = self.ingest(remaining);

        // Emit any remaining partial as a Complete line
        if !self.partial.is_empty() {
            let content = String::from_utf8_lossy(&self.partial).to_string();
            self.partial.clear();
            lines.push(Line::Complete(content));
        }

        lines
    }
}
```

### ANSI sequence handling in the line buffer

ANSI escape sequences can span multiple bytes and appear mid-line. The line buffer does **not** strip them — it preserves raw bytes. ANSI processing happens in the classifier, where it's used as metadata before being stripped for pattern matching.

However, the line buffer does need to handle one ANSI case: **cursor movement sequences** that effectively erase or overwrite content.

```
\x1b[K     →  Erase to end of line (treat like \r for overwrite detection)
\x1b[2K    →  Erase entire line
\x1b[A     →  Cursor up (multi-line progress — treat as overwrite)
```

These are handled by extending the overwrite detection:

```rust
fn is_erase_sequence(bytes: &[u8], pos: usize) -> Option<usize> {
    if bytes[pos] != 0x1b { return None; }
    if pos + 1 >= bytes.len() || bytes[pos + 1] != b'[' { return None; }

    // Parse CSI sequence
    let mut i = pos + 2;
    while i < bytes.len() && (bytes[i] >= b'0' && bytes[i] <= b'9' || bytes[i] == b';') {
        i += 1;
    }

    if i < bytes.len() {
        match bytes[i] {
            b'K' => Some(i + 1),  // Erase in line
            b'A' => Some(i + 1),  // Cursor up
            b'J' => Some(i + 1),  // Erase in display
            _ => None,
        }
    } else {
        None
    }
}
```

## Emit Buffer

### State

```rust
struct EmitBuffer {
    pending_count: u64,                    // unclassified lines buffered
    outcomes: Vec<CapturedOutcome>,        // for final summary
    hazards: Vec<EmittedHazard>,           // hazards already emitted (for summary)
    ring: RingBuffer<String, 5>,           // last 5 lines
    line_count: u64,                       // total lines seen
    dedup: DedupEngine,
    start_time: Instant,
    last_emit_time: Instant,
    output: Vec<String>,                   // accumulated output lines
}

struct CapturedOutcome {
    captures: HashMap<String, String>,
    text: String,
}

struct EmittedHazard {
    severity: Severity,
    text: String,
    attached_lines: Vec<String>,
}
```

### Accept logic

```rust
impl EmitBuffer {
    fn accept(&mut self, classified: Classification) {
        self.line_count += 1;

        // Always update ring buffer with raw text
        if let Some(text) = classified.text() {
            self.ring.push(text.to_string());
        }

        match classified {
            Classification::Hazard { severity, text, .. } => {
                // Flush pending noise count first (preserve ordering)
                self.flush_pending_count();
                self.dedup.flush_all(&mut self.output);

                // Emit immediately
                let prefix = match severity {
                    Severity::Error => "!",
                    Severity::Warning => "~",
                };
                self.output.push(format!(" {} {}", prefix, text));
                self.hazards.push(EmittedHazard { severity, text, attached_lines: vec![] });
                self.last_emit_time = Instant::now();
            }

            Classification::Outcome { text, captures } => {
                self.outcomes.push(CapturedOutcome { captures, text });
            }

            Classification::Noise { action: NoiseAction::Strip } => {
                // Gone. Count it, nothing else.
                self.pending_count += 1;
            }

            Classification::Noise { action: NoiseAction::Dedup } => {
                if let Some(text) = classified.text() {
                    self.dedup.ingest(text);
                }
                self.pending_count += 1;
            }

            Classification::Prompt { text } => {
                self.flush_pending_count();
                self.dedup.flush_all(&mut self.output);
                self.output.push(format!(" ? {}", text));
                self.last_emit_time = Instant::now();
            }

            Classification::Unknown { .. } => {
                self.pending_count += 1;
            }
        }
    }

    fn flush_pending_count(&mut self) {
        // Only emit count if there's something to report
        // and we haven't just emitted a count
        if self.pending_count > 0 {
            // Don't emit for small counts — it's noisier than the noise
            // Only show count when it's substantial
            if self.pending_count >= 10 {
                self.output.push(format!(" ... {} lines", self.pending_count));
            }
            self.pending_count = 0;
        }
    }
}
```

### Finalize (on process exit)

```rust
impl EmitBuffer {
    fn finalize(&mut self, status: ExitStatus, grammar: Option<&Grammar>) -> Summary {
        // Flush any remaining pending
        self.flush_pending_count();
        self.dedup.flush_all(&mut self.output);

        let elapsed = self.start_time.elapsed();
        let exit_code = status.code().unwrap_or(-1);

        // Build header
        let header = format!(
            "{} lines → exit {} ({:.1}s)",
            self.line_count, exit_code, elapsed.as_secs_f64()
        );

        // Build summary lines from outcomes
        let summary_lines = if let Some(grammar) = grammar {
            grammar.format_summary(&self.outcomes, exit_code)
        } else if !self.outcomes.is_empty() {
            // No grammar — show outcomes as-is
            self.outcomes.iter()
                .map(|o| format!(" + {}", o.text))
                .collect()
        } else {
            // No grammar, no outcomes — use last lines from ring buffer
            self.ring.iter()
                .map(|line| format!(" last: {}", line))
                .collect()
        };

        Summary {
            header,
            summary_lines,
            hazard_lines: std::mem::take(&mut self.output),
            exit_code,
        }
    }
}
```

### Summary output assembly

The final output is assembled in this order:

```
{header}                              ← "1400 lines → exit 0 (12.3s)"
{summary_lines}                       ← " + 147 packages installed"
{hazard_lines from stream}            ← " ~ npm warn deprecated (x3)"
                                        " ! error: something broke"
```

If there are no grammar outcomes and the tool is unknown:

```
{header}                              ← "892 lines → exit 0 (45s)"
{inline hazard/noise from stream}     ← " ... 847 lines"
                                        " ! WARNING: column exists"
                                        " ... 44 lines"
{last lines}                          ← " last: Migration complete."
```

## Timing Configuration

All timing values are configurable with sensible defaults:

```rust
struct TimingConfig {
    /// How long to wait before declaring a partial line a prompt
    partial_line_timeout: Duration,    // default: 500ms

    /// How long silence must last before flushing pending output
    silence_timeout: Duration,         // default: 2s

    /// Periodic flush interval for long-running processes
    flush_interval: Duration,          // default: 5s

    /// Minimum gap between consecutive flushes (debounce)
    flush_debounce: Duration,          // default: 200ms
}
```

## Output Modes

mish supports multiple output modes for different consumers:

### Human mode (default)

Condensed output printed to the terminal with symbols and indentation:

```
$ mish npm install
1400 lines → exit 0 (12.3s)
 + 147 packages installed
 ~ npm warn deprecated: inflight@1.0.6 (x3)
 ~ 2 moderate vulnerabilities
```

### JSON mode (`--json`)

Structured output for programmatic consumption:

```json
{
  "command": "npm install",
  "exit_code": 0,
  "elapsed_seconds": 12.3,
  "total_lines": 1400,
  "outcomes": [
    { "type": "success", "text": "147 packages installed", "captures": { "count": "147", "time": "12.3s" } }
  ],
  "hazards": [
    { "severity": "warning", "text": "npm warn deprecated: inflight@1.0.6", "count": 3 },
    { "severity": "warning", "text": "2 moderate vulnerabilities" }
  ]
}
```

### Passthrough mode (`--passthrough`)

Full output passes through to the terminal in real time (user sees everything), but mish also produces a condensed summary at the end. Useful when you want to watch the output but also get a summary.

```
[full npm install output streams through normally]

── mish summary ──
1400 lines → exit 0 (12.3s)
 + 147 packages installed
 ~ npm warn deprecated: inflight@1.0.6 (x3)
```

### Context mode (`--context`)

Designed for LLM context injection. Produces the most compressed possible output optimized for token efficiency:

```
npm install: ok 147pkg 12.3s ~deprecated(x3) ~2vuln
```

## Signal Handling

```
Signal    Action
────────────────────────────────────
SIGINT    Forward to child. If child exits, finalize normally.
          If second SIGINT within 1s, mish exits too.
SIGTERM   Forward to child, wait briefly, finalize.
SIGWINCH  Resize PTY, no other action.
SIGCHLD   Child exited — trigger drain and finalize.
SIGTSTP   Forward to child (Ctrl-Z suspend).
SIGCONT   Forward to child (resume from suspend).
```

## Passthrough and Interactivity

mish is transparent to interactive programs. Since it uses a PTY and forwards stdin, programs like vim, less, htop, or any REPL work normally. The classification engine simply accumulates lines and produces a summary, but the user sees everything in real time.

For non-interactive commands (the common case), mish can suppress output entirely and only show the condensed summary. This is the default mode.

For interactive commands, mish auto-detects interactivity:
- Raw mode switch on the PTY → interactive (pass through, summarize at end)
- No raw mode → non-interactive (suppress output, show summary only)
