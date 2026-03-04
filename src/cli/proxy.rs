/// CLI proxy entry point.
///
/// Parses the command, invokes the category router, and formats terminal output.
/// Handles compound commands (split on &&, ||, ;) and output mode flags.
///
/// Also provides an async event loop (`run_interactive_loop`) for signal
/// handling and stdin forwarding when commands need interactive I/O.

use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::config_loader::load_runtime_config;
use crate::core::format::{
    self, EnrichmentLine, FormatInput, HazardEntry, OutputMode, RecommendationEntry,
};
use crate::core::pty::PtyCapture;
use crate::handlers::structured::StructuredData;
use crate::router::categories::{CategoriesConfig, ExecutionMode};
use crate::router::{self, HandlerOutput, RouterResult};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Compound command operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompoundOp {
    /// `&&` — run next only if previous succeeded.
    And,
    /// `||` — run next only if previous failed.
    Or,
    /// `;` — always run next.
    Seq,
}

/// A segment of a compound command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompoundSegment {
    pub command: Vec<String>,
    pub operator: Option<CompoundOp>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Split args on compound operators (&&, ||, ;).
///
/// Each segment's `operator` indicates the operator that *follows* it.
/// The last segment always has `operator: None`.
pub fn split_compound(args: &[String]) -> Vec<CompoundSegment> {
    let mut segments = Vec::new();
    let mut current = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        if arg == "&&" {
            if !current.is_empty() {
                segments.push(CompoundSegment {
                    command: std::mem::take(&mut current),
                    operator: Some(CompoundOp::And),
                });
            }
        } else if arg == "||" {
            if !current.is_empty() {
                segments.push(CompoundSegment {
                    command: std::mem::take(&mut current),
                    operator: Some(CompoundOp::Or),
                });
            }
        } else if arg == ";" {
            if !current.is_empty() {
                segments.push(CompoundSegment {
                    command: std::mem::take(&mut current),
                    operator: Some(CompoundOp::Seq),
                });
            }
        } else {
            current.push(arg.clone());
        }

        i += 1;
    }

    // Push the last segment (no trailing operator)
    if !current.is_empty() {
        segments.push(CompoundSegment {
            command: current,
            operator: None,
        });
    }

    segments
}

/// Extract output mode flag from args, returning (mode, remaining_args).
///
/// Recognises `--json`, `--passthrough`, `--context` as the first argument.
pub fn parse_mode(args: &[String]) -> (OutputMode, Vec<String>) {
    if args.is_empty() {
        return (OutputMode::Human, vec![]);
    }

    match args[0].as_str() {
        "--json" => (OutputMode::Json, args[1..].to_vec()),
        "--passthrough" => (OutputMode::Passthrough, args[1..].to_vec()),
        "--context" => (OutputMode::Context, args[1..].to_vec()),
        _ => (OutputMode::Human, args.to_vec()),
    }
}

/// Check whether args contain a bare `|` token (indicating a shell pipeline).
///
/// Detects pipes at the token level — a standalone `|` argument indicates a pipeline.
/// Tokens like `|` inside a larger string (e.g. `"hello|world"`) are NOT treated as pipes.
pub fn contains_pipe(args: &[String]) -> bool {
    args.iter().any(|a| a == "|")
}

/// Run the CLI proxy pipeline: parse mode from args, split compounds, route, format, print.
/// Returns the exit code of the last executed command.
pub fn run(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let (mode, command_args) = parse_mode(args);
    run_with_mode(&command_args, mode)
}

/// Run the CLI proxy pipeline with an explicit output mode.
/// Returns the exit code of the last executed command.
pub fn run_with_mode(args: &[String], mode: OutputMode) -> Result<i32, Box<dyn std::error::Error>> {
    if args.is_empty() {
        return Err("usage: mish <command> [args...]".into());
    }

    // Pipe detection: if args contain a bare `|`, run the whole thing as a
    // single shell pipeline. The final output is what matters, so we treat
    // it as passthrough category.
    if contains_pipe(args) {
        return run_pipeline(args, mode);
    }

    let segments = split_compound(args);

    // Load runtime config (grammars, categories, dangerous patterns)
    let (grammars, categories_config, dangerous_patterns) = match load_runtime_config(None) {
        Ok(rc) => (rc.grammars, rc.categories_config, rc.dangerous_patterns),
        Err(_) => (
            HashMap::new(),
            CategoriesConfig {
                categories: HashMap::new(),
            },
            Vec::new(),
        ),
    };

    let mut results: Vec<FormatInput> = Vec::new();
    let mut last_exit_code = 0i32;

    for (i, segment) in segments.iter().enumerate() {
        // Compound operator logic: check previous segment's operator
        if i > 0 {
            if let Some(prev_op) = segments[i - 1].operator {
                match prev_op {
                    CompoundOp::And => {
                        if last_exit_code != 0 {
                            continue; // skip — previous failed
                        }
                    }
                    CompoundOp::Or => {
                        if last_exit_code == 0 {
                            continue; // skip — previous succeeded
                        }
                    }
                    CompoundOp::Seq => {} // always run
                }
            }
        }

        let router_result = router::route(
            &segment.command,
            &grammars,
            &categories_config,
            &dangerous_patterns,
            mode,
            ExecutionMode::Cli,
        )?;

        last_exit_code = router_result.exit_code;
        results.push(router_result_to_format_input(&router_result, &segment.command));
    }

    // Format and print
    let formatted = if results.len() == 1 {
        format::format_result(&results[0], mode)
    } else {
        format::format_results(&results, mode)
    };

    println!("{}", formatted);

    Ok(last_exit_code)
}

/// Run a pipeline command (args containing `|`) as a single unit via `/bin/sh -c`.
///
/// The full argument list is joined and executed as a shell command. The output is
/// treated as passthrough — the final pipeline output is what matters for the LLM.
fn run_pipeline(args: &[String], mode: OutputMode) -> Result<i32, Box<dyn std::error::Error>> {
    let pipeline_str = args.join(" ");

    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(&pipeline_str)
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let combined = if stderr.is_empty() {
        stdout.into_owned()
    } else {
        format!("{}{}", stdout, stderr)
    };

    let exit_code = output.status.code().unwrap_or(-1);

    let result = FormatInput {
        command: pipeline_str,
        exit_code,
        category: "passthrough".to_string(),
        body: combined.clone(),
        raw_output: Some(combined),
        total_lines: None,
        elapsed_secs: None,
        outcomes: vec![],
        hazards: vec![],
        enrichment: vec![],
        recommendations: vec![],
    };

    let formatted = format::format_result(&result, mode);
    println!("{}", formatted);

    Ok(exit_code)
}

// ---------------------------------------------------------------------------
// Interactive event loop with signal handling + stdin forwarding
// ---------------------------------------------------------------------------

/// Result from the interactive event loop.
#[derive(Debug)]
pub struct EventLoopResult {
    /// Child exit code (0 = success), or 128+signal if killed.
    pub exit_code: i32,
    /// All output captured from the child.
    pub output: Vec<u8>,
    /// Whether mish itself was terminated by double-SIGINT.
    pub force_exit: bool,
}

/// Run a command with full signal proxying and stdin forwarding.
///
/// This spawns the command in a PTY and enters an async event loop that:
/// - Forwards stdin from the user to the child process
/// - Forwards PTY output to stdout
/// - Proxies signals: SIGINT, SIGTERM, SIGWINCH, SIGTSTP, SIGCONT
/// - Double SIGINT (within 1s) causes mish to exit
/// - SIGTERM triggers graceful shutdown: forward to child, wait, drain, finalize
///
/// Returns an `EventLoopResult` with exit code and captured output.
pub async fn run_interactive_loop(
    command: &[String],
) -> Result<EventLoopResult, Box<dyn std::error::Error>> {
    if command.is_empty() {
        return Err("run_interactive_loop: empty command".into());
    }

    // Set up async stdin reader from real stdin
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let _stdin_handle = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let stdin = std::io::stdin();
        let mut stdin_lock = stdin.lock();
        let mut buf = [0u8; 4096];
        loop {
            match stdin_lock.read(&mut buf) {
                Ok(0) => break,  // EOF
                Ok(n) => {
                    if stdin_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break; // receiver dropped
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });

    event_loop_inner(command, stdin_rx, true).await
}

/// Inner event loop — separated for testability.
///
/// `stdin_rx` provides input to forward to the child's PTY.
/// `write_stdout` controls whether PTY output is echoed to stdout (false for tests).
async fn event_loop_inner(
    command: &[String],
    mut stdin_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    write_stdout: bool,
) -> Result<EventLoopResult, Box<dyn std::error::Error>> {
    // Spawn child in PTY
    let pty = PtyCapture::spawn(command)?;

    // Shared state for signal handling
    let force_exit = Arc::new(AtomicBool::new(false));
    let last_sigint = Arc::new(std::sync::Mutex::new(Option::<Instant>::None));

    // Set up signal handlers using tokio
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigwinch =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;

    let child_pid = pty.pid();

    let mut all_output = Vec::new();
    let force_exit_clone = force_exit.clone();
    let last_sigint_clone = last_sigint.clone();
    let mut stdin_open = true;

    // Event loop
    let mut pty_buf = [0u8; 4096];

    loop {
        // Check for force exit (double SIGINT)
        if force_exit_clone.load(Ordering::SeqCst) {
            // Send SIGKILL to child and exit
            let _ = nix::sys::signal::kill(child_pid, nix::sys::signal::Signal::SIGKILL);
            let _ = pty.wait();
            return Ok(EventLoopResult {
                exit_code: 130, // 128 + SIGINT(2)
                output: all_output,
                force_exit: true,
            });
        }

        // Read available PTY output (non-blocking)
        loop {
            match pty.read_output(&mut pty_buf) {
                Ok(0) => break, // no more data right now
                Ok(n) => {
                    all_output.extend_from_slice(&pty_buf[..n]);
                    if write_stdout {
                        use std::io::Write;
                        let _ = std::io::stdout().write_all(&pty_buf[..n]);
                        let _ = std::io::stdout().flush();
                    }
                }
                Err(_) => break, // PTY error — child likely exited
            }
        }

        // Check if child has exited
        if let Ok(Some(status)) = pty.try_wait() {
            // Child exited — drain remaining output
            if let Ok(remaining) = pty.drain() {
                all_output.extend_from_slice(&remaining);
                if write_stdout && !remaining.is_empty() {
                    use std::io::Write;
                    let _ = std::io::stdout().write_all(&remaining);
                    let _ = std::io::stdout().flush();
                }
            }

            let exit_code = match (status.code, status.signal) {
                (Some(code), _) => code,
                (None, Some(sig)) => 128 + sig,
                _ => 1,
            };

            return Ok(EventLoopResult {
                exit_code,
                output: all_output,
                force_exit: false,
            });
        }

        // Use tokio::select! to wait for next event.
        // When stdin is closed, use a pending future so it never fires.
        tokio::select! {
            // SIGINT handling
            _ = sigint.recv() => {
                let mut last = last_sigint_clone.lock().unwrap();
                let now = Instant::now();

                if let Some(prev) = *last {
                    if now.duration_since(prev).as_millis() < 1000 {
                        // Double SIGINT within 1s — force exit
                        force_exit_clone.store(true, Ordering::SeqCst);
                        let _ = nix::sys::signal::kill(child_pid, nix::sys::signal::Signal::SIGKILL);
                        let _ = pty.wait();
                        return Ok(EventLoopResult {
                            exit_code: 130,
                            output: all_output,
                            force_exit: true,
                        });
                    }
                }

                *last = Some(now);
                // Forward single SIGINT to child
                let _ = nix::sys::signal::kill(child_pid, nix::sys::signal::Signal::SIGINT);
            }

            // SIGTERM handling — graceful shutdown
            _ = sigterm.recv() => {
                let _ = nix::sys::signal::kill(child_pid, nix::sys::signal::Signal::SIGTERM);
                // Wait briefly for child to exit
                let deadline = Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    if Instant::now() > deadline {
                        // Timeout — escalate to SIGKILL
                        let _ = nix::sys::signal::kill(child_pid, nix::sys::signal::Signal::SIGKILL);
                        break;
                    }
                    if let Ok(Some(_)) = pty.try_wait() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }

                // Drain remaining output
                if let Ok(remaining) = pty.drain() {
                    all_output.extend_from_slice(&remaining);
                }

                return Ok(EventLoopResult {
                    exit_code: 143, // 128 + SIGTERM(15)
                    output: all_output,
                    force_exit: false,
                });
            }

            // SIGWINCH handling — resize PTY
            _ = sigwinch.recv() => {
                // Query current terminal size and forward to PTY
                let ws = query_terminal_size();
                let _ = pty.resize(ws.0, ws.1);
            }

            // stdin forwarding — only poll when stdin is still open
            data = stdin_rx.recv(), if stdin_open => {
                match data {
                    Some(bytes) => {
                        let _ = pty.write_stdin(&bytes);
                    }
                    None => {
                        // stdin closed (EOF) — stop polling
                        stdin_open = false;
                    }
                }
            }

            // Poll interval — drives PTY read and child exit checks
            _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                // PTY output is read at the top of the loop
            }
        }
    }
}

/// Query the current terminal dimensions (cols, rows).
///
/// Returns (cols, rows), falling back to (80, 24).
fn query_terminal_size() -> (u16, u16) {
    let mut ws = nix::pty::Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    unsafe {
        if nix::libc::ioctl(nix::libc::STDOUT_FILENO, nix::libc::TIOCGWINSZ, &mut ws) == -1 {
            if nix::libc::ioctl(nix::libc::STDERR_FILENO, nix::libc::TIOCGWINSZ, &mut ws) == -1 {
                ws.ws_row = 24;
                ws.ws_col = 80;
            }
        }
    }

    if ws.ws_row == 0 { ws.ws_row = 24; }
    if ws.ws_col == 0 { ws.ws_col = 80; }

    (ws.ws_col, ws.ws_row)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Convert a RouterResult into a FormatInput for the formatter.
fn router_result_to_format_input(result: &RouterResult, command: &[String]) -> FormatInput {
    let cmd_str = command.join(" ");
    let category = result.category.to_string();

    let (body, raw_output, outcomes, hazards, category_override) = match &result.output {
        HandlerOutput::Condensed(summary) => {
            if summary.interactive_session {
                // Interactive transition: output was shown live, use session summary
                let body = format!("{}: session ended", cmd_str);
                (body, None, vec![], vec![], Some("interactive".to_string()))
            } else {
                // Standard condensed: assemble body from summary parts
                let mut body_parts = vec![summary.header.clone()];
                body_parts.extend(summary.summary_lines.iter().cloned());
                body_parts.extend(summary.hazard_lines.iter().cloned());
                let body = body_parts.join("\n");

                let outcomes: Vec<String> = summary
                    .summary_lines
                    .iter()
                    .filter_map(|l| l.strip_prefix(" + ").map(|s| s.to_string()))
                    .collect();

                let hazards: Vec<HazardEntry> = summary
                    .hazard_lines
                    .iter()
                    .map(|l| {
                        if let Some(text) = l.strip_prefix(" ! ") {
                            HazardEntry {
                                severity: "error".to_string(),
                                text: text.to_string(),
                            }
                        } else if let Some(text) = l.strip_prefix(" ~ ") {
                            HazardEntry {
                                severity: "warning".to_string(),
                                text: text.to_string(),
                            }
                        } else {
                            HazardEntry {
                                severity: "info".to_string(),
                                text: l.to_string(),
                            }
                        }
                    })
                    .collect();

                (body, None, outcomes, hazards, None)
            }
        }
        HandlerOutput::Narrated(nr) => (nr.message.clone(), None, vec![], vec![], None),
        HandlerOutput::Passthrough(pr) => {
            let body = format!("{}\n\u{2500}\u{2500} {} \u{2500}\u{2500}", pr.output, pr.footer);
            (body, Some(pr.output.clone()), vec![], vec![], None)
        }
        HandlerOutput::Structured(sr) => {
            let body = format_structured_data(&sr.parsed);
            (body, None, vec![], vec![], None)
        }
        HandlerOutput::Interactive(ir) => {
            let secs = ir.duration.as_secs();
            let body = format!("{}: session ended ({}s)", cmd_str, secs);
            (body, None, vec![], vec![], None)
        }
        HandlerOutput::Dangerous(dr) => (dr.warning.clone(), None, vec![], vec![], None),
    };

    let enrichment = result
        .enrichment
        .as_ref()
        .map(|lines| {
            lines
                .iter()
                .map(|l| EnrichmentLine {
                    kind: l.kind.clone(),
                    message: l.message.clone(),
                })
                .collect()
        })
        .unwrap_or_default();

    let recommendations = result
        .preflight
        .as_ref()
        .map(|pf| {
            pf.recommendations
                .iter()
                .map(|r| RecommendationEntry {
                    flag: r.flag.clone(),
                    reason: r.reason.clone(),
                })
                .collect()
        })
        .unwrap_or_default();

    FormatInput {
        command: cmd_str,
        exit_code: result.exit_code,
        category: category_override.unwrap_or(category),
        body,
        raw_output,
        total_lines: None,
        elapsed_secs: None,
        outcomes,
        hazards,
        enrichment,
        recommendations,
    }
}

/// Format structured data for human display.
fn format_structured_data(data: &StructuredData) -> String {
    match data {
        StructuredData::GitStatus(info) => {
            let total = info.modified + info.added + info.deleted + info.untracked;
            let mut parts = Vec::new();
            if info.modified > 0 {
                parts.push(format!("{} modified", info.modified));
            }
            if info.added > 0 {
                parts.push(format!("{} added", info.added));
            }
            if info.deleted > 0 {
                parts.push(format!("{} deleted", info.deleted));
            }
            if info.untracked > 0 {
                parts.push(format!("{} untracked", info.untracked));
            }
            format!("git status: {} files ({})", total, parts.join(", "))
        }
        StructuredData::DockerPs(containers) => {
            format!("docker ps: {} containers running", containers.len())
        }
        StructuredData::Generic(raw) => raw.clone(),
    }
}


// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    // -----------------------------------------------------------------------
    // Test 12: split_compound with &&
    // -----------------------------------------------------------------------
    #[test]
    fn test_split_compound_and() {
        let input = args(&["echo", "hello", "&&", "echo", "world"]);
        let segments = split_compound(&input);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].command, args(&["echo", "hello"]));
        assert_eq!(segments[0].operator, Some(CompoundOp::And));
        assert_eq!(segments[1].command, args(&["echo", "world"]));
        assert_eq!(segments[1].operator, None);
    }

    // -----------------------------------------------------------------------
    // Test 13: split_compound with ||
    // -----------------------------------------------------------------------
    #[test]
    fn test_split_compound_or() {
        let input = args(&["false", "||", "echo", "fallback"]);
        let segments = split_compound(&input);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].command, args(&["false"]));
        assert_eq!(segments[0].operator, Some(CompoundOp::Or));
        assert_eq!(segments[1].command, args(&["echo", "fallback"]));
        assert_eq!(segments[1].operator, None);
    }

    // -----------------------------------------------------------------------
    // Test 14: split_compound with ;
    // -----------------------------------------------------------------------
    #[test]
    fn test_split_compound_seq() {
        let input = args(&["echo", "a", ";", "echo", "b"]);
        let segments = split_compound(&input);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].command, args(&["echo", "a"]));
        assert_eq!(segments[0].operator, Some(CompoundOp::Seq));
        assert_eq!(segments[1].command, args(&["echo", "b"]));
        assert_eq!(segments[1].operator, None);
    }

    // -----------------------------------------------------------------------
    // Test 15: parse_mode extracts flags correctly
    // -----------------------------------------------------------------------
    #[test]
    fn test_parse_mode_flags() {
        let (mode, cmd) = parse_mode(&args(&["--json", "echo", "hello"]));
        assert_eq!(mode, OutputMode::Json);
        assert_eq!(cmd, args(&["echo", "hello"]));

        let (mode, cmd) = parse_mode(&args(&["--passthrough", "ls"]));
        assert_eq!(mode, OutputMode::Passthrough);
        assert_eq!(cmd, args(&["ls"]));

        let (mode, cmd) = parse_mode(&args(&["--context", "npm", "install"]));
        assert_eq!(mode, OutputMode::Context);
        assert_eq!(cmd, args(&["npm", "install"]));

        let (mode, cmd) = parse_mode(&args(&["echo", "hello"]));
        assert_eq!(mode, OutputMode::Human);
        assert_eq!(cmd, args(&["echo", "hello"]));
    }

    // -----------------------------------------------------------------------
    // Test 16: exit code propagation through run()
    // -----------------------------------------------------------------------
    #[test]
    #[serial_test::serial(pty)]
    fn test_exit_code_propagation() {
        let exit_code = run(&args(&["/bin/sh", "-c", "exit 1"])).unwrap();
        assert_ne!(exit_code, 0, "/bin/sh -c 'exit 1' should return non-zero");

        let exit_code = run(&args(&["echo", "hello"])).unwrap();
        assert_eq!(exit_code, 0, "echo should return zero");
    }

    // -----------------------------------------------------------------------
    // Test 17: split_compound with mixed operators
    // -----------------------------------------------------------------------
    #[test]
    fn test_split_compound_mixed() {
        let input = args(&["echo", "a", "&&", "echo", "b", ";", "echo", "c"]);
        let segments = split_compound(&input);
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].command, args(&["echo", "a"]));
        assert_eq!(segments[0].operator, Some(CompoundOp::And));
        assert_eq!(segments[1].command, args(&["echo", "b"]));
        assert_eq!(segments[1].operator, Some(CompoundOp::Seq));
        assert_eq!(segments[2].command, args(&["echo", "c"]));
        assert_eq!(segments[2].operator, None);
    }

    // -----------------------------------------------------------------------
    // Test 18: Unknown flags are NOT consumed by parse_mode — passed through
    // -----------------------------------------------------------------------
    #[test]
    fn test_parse_mode_unknown_flags_passed_through() {
        // Flags that look like they could be mish flags but aren't should
        // be passed through as part of the command, not consumed.
        let (mode, cmd) = parse_mode(&args(&["--verbose", "echo", "hello"]));
        assert_eq!(mode, OutputMode::Human, "unknown flag should not set a mode");
        assert_eq!(
            cmd,
            args(&["--verbose", "echo", "hello"]),
            "unknown flag should remain in command args"
        );

        let (mode, cmd) = parse_mode(&args(&["--loglevel=warn", "npm", "install"]));
        assert_eq!(mode, OutputMode::Human);
        assert_eq!(
            cmd,
            args(&["--loglevel=warn", "npm", "install"]),
            "tool-specific flags should remain in command args"
        );

        // A flag appearing after the command name should also pass through
        let (mode, cmd) = parse_mode(&args(&["npm", "--json", "install"]));
        assert_eq!(
            mode,
            OutputMode::Human,
            "--json after command should not be consumed by mish"
        );
        assert_eq!(
            cmd,
            args(&["npm", "--json", "install"]),
            "--json after command should stay in args"
        );
    }

    // -----------------------------------------------------------------------
    // Test 19: Unknown flags pass through the full run_with_mode pipeline
    // -----------------------------------------------------------------------
    #[test]
    fn test_unknown_flags_passed_through_to_command() {
        // `echo --verbose hello` should execute `echo --verbose hello`,
        // not consume --verbose as a mish flag.
        let exit_code = run_with_mode(
            &args(&["echo", "--verbose", "hello"]),
            OutputMode::Human,
        )
        .unwrap();
        assert_eq!(exit_code, 0, "echo with unknown flags should succeed");
    }

    // -----------------------------------------------------------------------
    // Test 20: split_compound with single command (no operators)
    // -----------------------------------------------------------------------
    #[test]
    fn test_split_compound_single_command() {
        let input = args(&["npm", "install", "lodash"]);
        let segments = split_compound(&input);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].command, args(&["npm", "install", "lodash"]));
        assert_eq!(segments[0].operator, None);
    }

    // -----------------------------------------------------------------------
    // Test 21: split_compound with empty input
    // -----------------------------------------------------------------------
    #[test]
    fn test_split_compound_empty() {
        let segments = split_compound(&[]);
        assert!(segments.is_empty(), "empty input should produce no segments");
    }

    // -----------------------------------------------------------------------
    // Test 22: run_with_mode full pipeline — simple echo
    // -----------------------------------------------------------------------
    #[test]
    fn test_run_with_mode_simple_echo() {
        let exit_code = run_with_mode(
            &args(&["echo", "hello"]),
            OutputMode::Human,
        )
        .unwrap();
        assert_eq!(exit_code, 0, "echo hello should exit 0");
    }

    // -----------------------------------------------------------------------
    // Test 23: run_with_mode with JSON output mode
    // -----------------------------------------------------------------------
    #[test]
    fn test_run_with_mode_json() {
        // Verify JSON mode doesn't crash and returns correct exit code
        let exit_code = run_with_mode(
            &args(&["echo", "hello"]),
            OutputMode::Json,
        )
        .unwrap();
        assert_eq!(exit_code, 0);
    }

    // -----------------------------------------------------------------------
    // Test 24: run_with_mode empty args returns error
    // -----------------------------------------------------------------------
    #[test]
    fn test_run_with_mode_empty_args_error() {
        let result = run_with_mode(&[], OutputMode::Human);
        assert!(result.is_err(), "empty args should return error");
    }

    // -----------------------------------------------------------------------
    // Test 25: parse_mode with empty args
    // -----------------------------------------------------------------------
    #[test]
    fn test_parse_mode_empty_args() {
        let (mode, cmd) = parse_mode(&[]);
        assert_eq!(mode, OutputMode::Human, "empty args should default to Human");
        assert!(cmd.is_empty(), "empty args should produce empty command");
    }

    // =======================================================================
    // Signal handling + stdin forwarding tests
    // =======================================================================
    use serial_test::serial;

    /// Helper: create a fake stdin channel and immediately close sender.
    fn closed_stdin() -> tokio::sync::mpsc::Receiver<Vec<u8>> {
        let (_tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        rx
    }

    /// Helper: create a stdin channel pair for tests that send input.
    fn test_stdin() -> (tokio::sync::mpsc::Sender<Vec<u8>>, tokio::sync::mpsc::Receiver<Vec<u8>>) {
        tokio::sync::mpsc::channel::<Vec<u8>>(64)
    }

    // -----------------------------------------------------------------------
    // Test 26: Event loop — basic command runs and captures output
    // -----------------------------------------------------------------------
    #[tokio::test]
    #[serial(pty)]
    async fn test_event_loop_basic_output() {
        let stdin_rx = closed_stdin();
        let result = event_loop_inner(
            &args(&["/bin/sh", "-c", "echo event_loop_works"]),
            stdin_rx,
            false,
        )
        .await
        .expect("event loop should succeed");

        assert_eq!(result.exit_code, 0, "echo should exit 0");
        assert!(!result.force_exit, "should not be force exit");

        let output = String::from_utf8_lossy(&result.output);
        assert!(
            output.contains("event_loop_works"),
            "output should contain echoed text, got: {:?}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 27: Event loop — exit code propagation
    // -----------------------------------------------------------------------
    #[tokio::test]
    #[serial(pty)]
    async fn test_event_loop_exit_code() {
        let stdin_rx = closed_stdin();
        let result = event_loop_inner(
            &args(&["/bin/sh", "-c", "exit 42"]),
            stdin_rx,
            false,
        )
        .await
        .expect("event loop should succeed");

        assert_eq!(result.exit_code, 42, "should propagate exit code 42");
        assert!(!result.force_exit);
    }

    // -----------------------------------------------------------------------
    // Test 28: Event loop — stdin forwarding via channel
    // -----------------------------------------------------------------------
    #[tokio::test]
    #[serial(pty)]
    async fn test_event_loop_stdin_forwarding() {
        let (stdin_tx, stdin_rx) = test_stdin();

        let send_handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let _ = stdin_tx.send(b"test_input\n".to_vec()).await;
        });

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            event_loop_inner(
                &args(&["/bin/sh", "-c", "read line && echo got:$line"]),
                stdin_rx,
                false,
            ),
        )
        .await
        .expect("should complete within timeout")
        .expect("event loop should succeed");

        let _ = send_handle.await;

        let output = String::from_utf8_lossy(&result.output);
        assert!(
            output.contains("got:test_input"),
            "stdin should be forwarded to child, got: {:?}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 29: SIGWINCH — PTY resize
    // -----------------------------------------------------------------------
    #[tokio::test]
    #[serial(pty)]
    async fn test_event_loop_sigwinch_resize() {
        use crate::core::pty::PtyCapture;

        let pty = PtyCapture::spawn(&args(&[
            "/bin/sh", "-c",
            "sleep 0.3 && stty size 2>/dev/null || echo unknown"
        ]))
        .expect("spawn should succeed");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        pty.resize(132, 50).expect("resize should succeed");

        let mut all = Vec::new();
        let mut buf = [0u8; 4096];
        let deadline = Instant::now() + std::time::Duration::from_secs(5);

        loop {
            if Instant::now() > deadline { break; }
            match pty.read_output(&mut buf) {
                Ok(0) => {
                    if let Ok(Some(_)) = pty.try_wait() {
                        if let Ok(remaining) = pty.drain() {
                            all.extend_from_slice(&remaining);
                        }
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Ok(n) => all.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }

        let output = String::from_utf8_lossy(&all);
        assert!(
            output.contains("50 132"),
            "expected '50 132' in stty output after resize, got: {:?}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 30: SIGTERM forwarding — child receives SIGTERM
    // -----------------------------------------------------------------------
    #[tokio::test]
    #[serial(pty)]
    async fn test_event_loop_sigterm_forwarding() {
        use crate::core::pty::PtyCapture;

        let pty = PtyCapture::spawn(&args(&[
            "/bin/sh", "-c",
            "trap 'echo SIGTERM_RECEIVED; exit 0' TERM; echo ready; while true; do sleep 0.1; done"
        ]))
        .expect("spawn should succeed");

        let mut buf = [0u8; 4096];
        let deadline = Instant::now() + std::time::Duration::from_secs(3);
        let mut all = Vec::new();

        loop {
            if Instant::now() > deadline { break; }
            match pty.read_output(&mut buf) {
                Ok(n) if n > 0 => {
                    all.extend_from_slice(&buf[..n]);
                    if String::from_utf8_lossy(&all).contains("ready") { break; }
                }
                _ => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            }
        }

        pty.signal(nix::sys::signal::Signal::SIGTERM).expect("signal should succeed");

        let deadline = Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if Instant::now() > deadline { break; }
            match pty.read_output(&mut buf) {
                Ok(0) => {
                    if let Ok(Some(_)) = pty.try_wait() {
                        if let Ok(remaining) = pty.drain() {
                            all.extend_from_slice(&remaining);
                        }
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Ok(n) => all.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }

        let output = String::from_utf8_lossy(&all);
        assert!(
            output.contains("SIGTERM_RECEIVED"),
            "child should receive SIGTERM and print marker, got: {:?}",
            output
        );
    }

    // -----------------------------------------------------------------------
    // Test 31: Double SIGINT — child killed
    // -----------------------------------------------------------------------
    #[tokio::test]
    #[serial(pty)]
    async fn test_event_loop_double_sigint() {
        use crate::core::pty::PtyCapture;

        let pty = PtyCapture::spawn(&args(&[
            "/bin/sh", "-c", "sleep 60"
        ]))
        .expect("spawn should succeed");

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        pty.signal(nix::sys::signal::Signal::SIGINT).expect("first SIGINT");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let _ = nix::sys::signal::kill(pty.pid(), nix::sys::signal::Signal::SIGKILL);

        let status = pty.wait().expect("wait should succeed");
        assert!(
            status.signal.is_some() || status.code.is_some(),
            "child should have been killed, got: {:?}",
            status
        );
    }

    // -----------------------------------------------------------------------
    // Test 32: Event loop — empty command returns error
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_event_loop_empty_command_error() {
        let result = run_interactive_loop(&[]).await;
        assert!(result.is_err(), "empty command should return error");
    }

    // -----------------------------------------------------------------------
    // Test 33: query_terminal_size returns valid dimensions
    // -----------------------------------------------------------------------
    #[test]
    fn test_query_terminal_size() {
        let (cols, rows) = query_terminal_size();
        assert!(cols > 0, "cols should be positive, got: {}", cols);
        assert!(rows > 0, "rows should be positive, got: {}", rows);
    }

    // -----------------------------------------------------------------------
    // Test 34: try_wait detects child exit
    // -----------------------------------------------------------------------
    #[tokio::test]
    #[serial(pty)]
    async fn test_try_wait_detects_exit() {
        use crate::core::pty::PtyCapture;

        let pty = PtyCapture::spawn(&args(&["/bin/sh", "-c", "exit 7"]))
            .expect("spawn should succeed");

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let status = pty.try_wait().expect("try_wait should succeed");
        assert!(status.is_some(), "child should have exited by now");
        let status = status.unwrap();
        assert_eq!(status.code, Some(7), "exit code should be 7");
    }

    // -----------------------------------------------------------------------
    // Test 35: Event loop — SIGINT forwarding to child
    // -----------------------------------------------------------------------
    #[tokio::test]
    #[serial(pty)]
    async fn test_event_loop_sigint_forwarding() {
        use crate::core::pty::PtyCapture;

        let pty = PtyCapture::spawn(&args(&[
            "/bin/sh", "-c",
            "trap 'echo SIGINT_CAUGHT; exit 0' INT; echo ready; while true; do sleep 0.1; done"
        ]))
        .expect("spawn should succeed");

        let mut buf = [0u8; 4096];
        let mut all = Vec::new();
        let deadline = Instant::now() + std::time::Duration::from_secs(3);

        loop {
            if Instant::now() > deadline { break; }
            match pty.read_output(&mut buf) {
                Ok(n) if n > 0 => {
                    all.extend_from_slice(&buf[..n]);
                    if String::from_utf8_lossy(&all).contains("ready") { break; }
                }
                _ => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            }
        }

        pty.signal(nix::sys::signal::Signal::SIGINT).expect("SIGINT should succeed");

        let deadline = Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if Instant::now() > deadline { break; }
            match pty.read_output(&mut buf) {
                Ok(0) => {
                    if let Ok(Some(_)) = pty.try_wait() {
                        if let Ok(remaining) = pty.drain() {
                            all.extend_from_slice(&remaining);
                        }
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Ok(n) => all.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }

        let output = String::from_utf8_lossy(&all);
        assert!(
            output.contains("SIGINT_CAUGHT"),
            "child should receive SIGINT and print marker, got: {:?}",
            output
        );
    }

    // =======================================================================
    // Pipe detection tests
    // =======================================================================

    // -----------------------------------------------------------------------
    // Test 36: contains_pipe detects bare pipe tokens
    // -----------------------------------------------------------------------
    #[test]
    fn test_contains_pipe_detects_bare_pipe() {
        assert!(contains_pipe(&args(&["cat", "file.txt", "|", "grep", "error"])));
        assert!(contains_pipe(&args(&["echo", "hello", "|", "wc", "-l"])));
    }

    // -----------------------------------------------------------------------
    // Test 37: contains_pipe ignores non-pipe args
    // -----------------------------------------------------------------------
    #[test]
    fn test_contains_pipe_no_pipe() {
        assert!(!contains_pipe(&args(&["echo", "hello"])));
        assert!(!contains_pipe(&args(&["ls", "-la"])));
        assert!(!contains_pipe(&args(&["/bin/sh", "-c", "echo test"])));
    }

    // -----------------------------------------------------------------------
    // Test 38: contains_pipe ignores pipe inside tokens
    // -----------------------------------------------------------------------
    #[test]
    fn test_contains_pipe_inside_token_not_detected() {
        assert!(!contains_pipe(&args(&["echo", "hello|world"])));
        assert!(!contains_pipe(&args(&["grep", "a|b", "file.txt"])));
    }

    // -----------------------------------------------------------------------
    // Test 39: contains_pipe with multi-stage pipeline
    // -----------------------------------------------------------------------
    #[test]
    fn test_contains_pipe_multi_stage() {
        assert!(contains_pipe(&args(&[
            "cat", "file", "|", "grep", "foo", "|", "wc", "-l"
        ])));
    }

    // -----------------------------------------------------------------------
    // Test 40: pipe takes precedence over compound operators
    // -----------------------------------------------------------------------
    #[test]
    fn test_pipe_mixed_with_compound_ops() {
        let input = args(&["cat", "file", "|", "grep", "err", "&&", "echo", "done"]);
        assert!(
            contains_pipe(&input),
            "pipe should be detected even with compound ops present"
        );
    }

    // -----------------------------------------------------------------------
    // Test 41: run_pipeline produces output
    // -----------------------------------------------------------------------
    #[test]
    fn test_run_pipeline_basic() {
        let exit_code = run(&args(&["echo", "hello_world", "|", "cat"])).unwrap();
        assert_eq!(exit_code, 0, "echo | cat should succeed");
    }

    // -----------------------------------------------------------------------
    // Test 42: run_pipeline exit code from last command in pipe
    // -----------------------------------------------------------------------
    #[test]
    fn test_run_pipeline_exit_code_from_last() {
        let exit_code = run(&args(&[
            "echo", "foo", "|", "grep", "nonexistent_string_xyz"
        ])).unwrap();
        assert_ne!(exit_code, 0, "pipeline exit code should come from last command (grep no match = 1)");
    }
}
