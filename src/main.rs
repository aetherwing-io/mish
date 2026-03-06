use clap::{Parser, Subcommand};
use mish::config::load_config;
use mish::core::format::OutputMode;

#[derive(Parser)]
#[command(name = "mish", version, about = "LLM-native shell")]
struct Cli {
    /// Output as JSON
    #[arg(long)]
    json: bool,

    /// Passthrough mode: full output + summary footer
    #[arg(long)]
    passthrough: bool,

    /// Ultra-compressed context mode
    #[arg(long)]
    context: bool,

    /// Print agent usage guide (patterns for LLM tool use)
    #[arg(long)]
    agents: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start MCP server over stdio
    Serve {
        /// Path to config file
        #[arg(long)]
        config: Option<String>,
    },

    /// Attach to an operator handoff session
    Attach {
        /// Handoff ID (format: hf_<hex>)
        handoff_id: String,

        /// Share process output with the LLM on detach (default: credential-blind)
        #[arg(long)]
        share_output: bool,

        /// Server PID to connect to (auto-discovered if omitted)
        #[arg(long)]
        pid: Option<u32>,
    },

    /// List running mish server instances
    Ps,

    /// View audit log entries
    Logs {
        /// Number of lines to show
        #[arg(long, default_value = "20")]
        lines: usize,
    },

    /// List active operator handoffs
    Handoffs {
        /// Watch for new handoffs (poll every 5s)
        #[arg(long)]
        watch: bool,
    },

    /// Configuration management
    Config {
        #[command(subcommand)]
        subcommand: ConfigCommands,
    },

    /// Persistent interpreter sessions
    Session {
        #[command(subcommand)]
        subcommand: SessionCommands,
    },

    /// Send input to the sole active session (shorthand)
    Send {
        /// Input to send
        input: String,

        /// Timeout in seconds
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// CLI proxy mode — run a command with category-aware output
    #[command(external_subcommand)]
    Proxy(Vec<String>),
}

#[derive(Subcommand)]
enum SessionCommands {
    /// Start a persistent interpreter session
    Start {
        /// Session alias (e.g. "py")
        alias: String,

        /// Command to run (e.g. "python3")
        #[arg(long)]
        cmd: String,

        /// Run in foreground (for debugging)
        #[arg(long)]
        fg: bool,
    },

    /// Send input to a named session
    Send {
        /// Session alias
        alias: String,

        /// Input to send
        input: String,

        /// Timeout in seconds
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// List active sessions
    List,

    /// Close a session
    Close {
        /// Session alias
        alias: String,
    },

    /// Internal host process (hidden)
    #[command(hide = true)]
    Host {
        /// Session alias
        alias: String,

        /// Command to run
        #[arg(long)]
        cmd: String,
    },
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Validate config file
    Check {
        /// Path to config file
        #[arg(long)]
        config: Option<String>,
    },
}

/// Detect mish subcommands that are pathological inside a PTY wrapper.
///
/// Returns `Some(error_message)` if the command should be rejected.
///
/// Hazardous patterns:
/// - `mish serve` — stdout is JSON-RPC transport, PTY corrupts framing
/// - `mish attach` — needs direct terminal access for operator handoff
/// - `mish session start --fg` — blocks forever inside captured PTY
/// - `mish session host` — same as --fg (internal, but guard anyway)
fn check_nested_mish_hazard(cmd: &str) -> Option<&'static str> {
    // Tokenize: split on whitespace, pipes, semicolons, && to find mish invocations.
    // We scan for "mish" followed by a hazardous subcommand anywhere in the string,
    // since the command may be "cd /foo && mish serve" or similar.
    let words: Vec<&str> = cmd.split_whitespace().collect();
    let mut i = 0;
    while i < words.len() {
        // Match "mish" or a path ending in "/mish"
        let w = words[i];
        let is_mish = w == "mish" || w.ends_with("/mish");
        if !is_mish {
            i += 1;
            continue;
        }

        // Look at the subcommand (next non-flag word)
        let sub = words.get(i + 1).copied().unwrap_or("");
        match sub {
            "serve" => {
                return Some(
                    "refusing to run `mish serve` inside -c (stdout is JSON-RPC transport; \
                     PTY capture would corrupt framing)",
                );
            }
            "attach" => {
                return Some(
                    "refusing to run `mish attach` inside -c (needs direct terminal access \
                     for operator handoff)",
                );
            }
            "session" => {
                let sub2 = words.get(i + 2).copied().unwrap_or("");
                match sub2 {
                    "host" => {
                        return Some(
                            "refusing to run `mish session host` inside -c (blocks forever \
                             inside captured PTY)",
                        );
                    }
                    "start" => {
                        // Only --fg is hazardous; detached start is fine
                        let rest = &words[i + 3..];
                        if rest.iter().any(|w| *w == "--fg") {
                            return Some(
                                "refusing to run `mish session start --fg` inside -c \
                                 (blocks forever inside captured PTY; remove --fg to detach)",
                            );
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Returns true if argv[0] indicates mish is being invoked as a shell symlink (bash/sh).
fn invoked_as_shell() -> bool {
    std::env::args()
        .next()
        .and_then(|a| {
            std::path::Path::new(&a)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
        })
        .map(|name| matches!(name.as_str(), "bash" | "sh" | "zsh"))
        .unwrap_or(false)
}

/// Check if this is the first shim invocation; if so, print interstitial and
/// return `true` (caller should exit 2). Subsequent calls return `false`.
fn shim_interstitial() -> bool {
    if std::env::var("MISH_QUIET").is_ok() {
        return false;
    }
    if !invoked_as_shell() {
        return false;
    }

    // First invocation only — one-shot interstitial, then silent permanently
    let counter_path = std::path::PathBuf::from("/tmp/.mish_shim_hint_count");
    let count = std::fs::read_to_string(&counter_path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);
    if count >= 1 {
        return false;
    }
    let _ = std::fs::write(&counter_path, (count + 1).to_string());

    println!(
        "\u{26a0} bash compatibility mode. Run `mish --agents` to fix."
    );
    true
}

/// Pre-clap intercept for `mish -c "cmd"` and `mish -lc "cmd"`.
///
/// Returns `Some(exit_code)` if `-c`/`-lc` was found, `None` otherwise.
/// Also scans for `--json`, `--passthrough`, `--context` before `-c` so
/// output mode flags still work (e.g. `mish --json -c "echo hi"`).
fn try_shell_dash_c() -> Option<i32> {
    let args: Vec<String> = std::env::args().collect();

    // First-ever shim invocation: show interstitial warning and bail.
    // The LLM sees the warning instead of command output and retries,
    // at which point the counter file exists and execution proceeds normally.
    if shim_interstitial() {
        return Some(2);
    }

    // Find the position of -c or -lc (skip argv[0])
    let mut c_pos = None;
    let mut json = false;
    let mut passthrough = false;
    let mut context = false;

    for (i, arg) in args.iter().enumerate().skip(1) {
        match arg.as_str() {
            "-c" | "-lc" => {
                c_pos = Some(i);
                break;
            }
            "--json" => json = true,
            "--passthrough" => passthrough = true,
            "--context" => context = true,
            _ => {
                // Any other arg before -c means this isn't a shell-compat invocation
                return None;
            }
        }
    }

    let c_pos = c_pos?;

    // The command string is the next argument after -c
    let cmd_str = args.get(c_pos + 1).cloned().unwrap_or_default();
    if cmd_str.is_empty() {
        eprintln!("mish: -c: option requires an argument");
        return Some(2);
    }

    // Reject commands that are pathological inside a PTY wrapper.
    // These need direct terminal access or use stdout as a protocol transport.
    if let Some(msg) = check_nested_mish_hazard(&cmd_str) {
        eprintln!("mish: -c: {msg}");
        return Some(1);
    }

    // Any remaining args after the command string are positional params ($0, $1, ...)
    // passed through to /bin/sh -c
    // Use /bin/bash for bashism support (process substitution, [[ ]], arrays).
    // Agent frameworks call `bash -c`; if we route through /bin/sh, bashisms
    // break on Linux where /bin/sh is dash. Fall back to /bin/sh if no bash.
    let shell = if std::path::Path::new("/bin/bash").exists() {
        "/bin/bash"
    } else {
        "/bin/sh"
    };
    let mut proxy_args = vec![
        shell.to_string(),
        "-c".to_string(),
        cmd_str.clone(),
    ];
    if args.len() > c_pos + 2 {
        proxy_args.extend_from_slice(&args[c_pos + 2..]);
    }

    // Non-TTY stdout → byte-exact passthrough by default. The squasher can't
    // distinguish file content (cat, head, sed) from build output, so dedup
    // corrupts source code reads (blank lines become "(x62)", repeated patterns
    // collapse). MISH_COMPRESS=1 opts in to squasher pipeline for environments
    // where all non-TTY output is known to be build/test output.
    let stdout_is_tty = unsafe { libc::isatty(libc::STDOUT_FILENO) } != 0;
    if !stdout_is_tty {
        if std::env::var("MISH_COMPRESS").map_or(false, |v| v == "1") {
            let code = run_dash_c_content_only(&proxy_args);

            return Some(code);
        }
        return Some(exec_real_shell(&proxy_args));
    }

    tracing_subscriber::fmt::init();

    let mode = if json {
        OutputMode::Json
    } else if passthrough {
        OutputMode::Passthrough
    } else if context {
        OutputMode::Context
    } else {
        OutputMode::Human
    };

    // When stdin is a pipe (agent frameworks, test harnesses), bypass the PTY
    // entirely and use Command with inherited stdin. PTYs can't forward piped
    // stdin reliably (no clean EOF signaling). Command handles it natively —
    // the child inherits the parent's pipe fd and the kernel delivers EOF when
    // the write end closes.
    let stdin_is_pipe = unsafe { libc::isatty(libc::STDIN_FILENO) } == 0;
    if stdin_is_pipe {
        let code = run_dash_c_piped(&proxy_args, &cmd_str, mode);
        return Some(code);
    }

    match mish::cli::proxy::run_with_mode(&proxy_args, mode) {
        Ok(exit_code) => {
            Some(exit_code)
        }
        Err(e) => {
            eprintln!("mish: {e}");
            Some(1)
        }
    }
}

/// Run a `-c` command in content-only mode (non-TTY stdout).
///
/// Captures stdout, runs it through the squasher pipeline (dedup, truncation),
/// then emits compressed body + a single-line footer with exit code, elapsed
/// time, and compression ratio. No mish headers — the body comes first so
/// harness consumers that check `lines[0]` see real output.
///
/// Special cases:
/// - Empty stdout → just the footer
/// - `COMPLETE_TASK` sentinel on first line → byte-exact passthrough (no squasher)
fn run_dash_c_content_only(args: &[String]) -> i32 {
    use std::process::{Command, Stdio};
    use std::time::Instant;

    let start = Instant::now();

    let child = Command::new(&args[0])
        .args(&args[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn();

    let child = match child {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mish: {e}");
            return 1;
        }
    };

    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("mish: {e}");
            return 1;
        }
    };

    let elapsed = start.elapsed().as_secs_f64();
    let exit_code = output.status.code().unwrap_or(1);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Empty output → just footer
    if stdout.is_empty() {
        let elapsed_str = mish::core::format::format_elapsed(Some(elapsed));
        let symbol = if exit_code == 0 { "+" } else { "!" };
        println!("\u{2500}\u{2500} {symbol} exit:{exit_code} {elapsed_str} \u{2500}\u{2500}");
        return exit_code;
    }

    // Sentinel: COMPLETE_TASK submission → byte-exact passthrough
    if stdout.trim_start().starts_with("COMPLETE_TASK") {
        print!("{}", stdout);
        return exit_code;
    }

    // Squasher pipeline
    let lines: Vec<mish::core::line_buffer::Line> = stdout
        .lines()
        .map(|l| mish::core::line_buffer::Line::Complete(l.to_string()))
        .collect();
    let total_lines = lines.len();
    let (processed, _metrics) = mish::handlers::condense::post_process(lines, None);
    let body = processed.join("\n");

    // Emit body
    if !body.is_empty() {
        println!("{}", body);
    }

    // Footer: ── {symbol} exit:{code} {elapsed} ({total}→{shown}) ──
    let elapsed_str = mish::core::format::format_elapsed(Some(elapsed));
    let symbol = if exit_code == 0 { "+" } else { "!" };
    let shown = if body.is_empty() {
        0
    } else {
        body.lines().count()
    };

    let mut footer = format!("\u{2500}\u{2500} {symbol} exit:{exit_code} {elapsed_str}");
    if total_lines != shown {
        footer.push_str(&format!(" ({total_lines}\u{2192}{shown})"));
    }
    footer.push_str(" \u{2500}\u{2500}");
    println!("{}", footer);

    exit_code
}

/// Exec the real shell with no mish processing (non-TTY stdout passthrough).
///
/// Uses `execvp` so this process is replaced entirely — no headers, no dedup,
/// no compression ratio, no timing. Byte-for-byte identical to the real shell.
fn exec_real_shell(args: &[String]) -> i32 {
    use std::ffi::CString;
    use std::process::{Command, Stdio};

    // Fast path: try execvp to replace this process entirely.
    // If it fails (shouldn't happen), fall back to spawn + wait.
    let c_args: Vec<CString> = args
        .iter()
        .map(|a| CString::new(a.as_str()).unwrap())
        .collect();
    let c_ptrs: Vec<*const libc::c_char> = c_args
        .iter()
        .map(|a| a.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();

    unsafe {
        libc::execvp(c_ptrs[0], c_ptrs.as_ptr());
    }

    // execvp only returns on error — fall back to Command
    eprintln!("mish: execvp failed, falling back to spawn");
    let status = Command::new(&args[0])
        .args(&args[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();

    match status {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("mish: {e}");
            1
        }
    }
}

/// Run a `-c` command when stdin is a pipe.
///
/// Bypasses the PTY (which can't forward piped stdin) and uses
/// `std::process::Command` with `Stdio::inherit()` for stdin.
/// Output is still processed through the squasher pipeline.
fn run_dash_c_piped(args: &[String], cmd_str: &str, mode: OutputMode) -> i32 {
    use std::process::{Command, Stdio};
    use std::time::Instant;

    let start = Instant::now();

    let child = Command::new(&args[0])
        .args(&args[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let child = match child {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mish: {e}");
            return 1;
        }
    };

    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("mish: {e}");
            return 1;
        }
    };

    let elapsed = start.elapsed().as_secs_f64();
    let exit_code = output.status.code().unwrap_or(1);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let combined = if stderr.is_empty() {
        stdout.into_owned()
    } else {
        format!("{}{}", stdout, stderr)
    };

    // Run through squasher pipeline for condensed output
    let lines: Vec<mish::core::line_buffer::Line> = combined
        .lines()
        .map(|l| mish::core::line_buffer::Line::Complete(l.to_string()))
        .collect();
    let total_lines = lines.len() as u64;
    let (processed, _metrics) = mish::handlers::condense::post_process(lines, None);
    let body = processed.join("\n");

    let result = mish::core::format::FormatInput {
        command: cmd_str.to_string(),
        exit_code,
        category: "condense".to_string(),
        body,
        raw_output: Some(combined),
        total_lines: Some(total_lines),
        elapsed_secs: Some(elapsed),
        outcomes: vec![],
        hazards: vec![],
        enrichment: vec![],
        recommendations: vec![],
    };

    let formatted = mish::core::format::format_result(&result, mode);
    println!("{}", formatted);

    exit_code
}

#[tokio::main]
async fn main() {
    // Pre-clap intercept: handle `-c` and `-lc` for shell-compatible invocation.
    // Agents invoke `mish -c "command"` or `mish -lc "command"` (Docker login shell).
    // Clap can't handle these because `-c` collides with top-level flag parsing
    // before reaching the external_subcommand variant.
    if let Some(result) = try_shell_dash_c() {
        std::process::exit(result);
    }

    let cli = Cli::parse();

    // --agents: print agent usage guide and exit.
    if cli.agents {
        print!("{}", mish::cli::agents::AGENT_GUIDE);
        std::process::exit(0);
    }

    let command = match cli.command {
        Some(cmd) => cmd,
        None => {
            eprintln!("usage: mish <command> [args...] or mish serve");
            std::process::exit(1);
        }
    };

    // In MCP serve mode, stdout is the JSON-RPC transport — tracing must
    // not write there.  Disable tracing entirely so nothing leaks to
    // stdout or stderr.  Other subcommands keep the default subscriber.
    match &command {
        Commands::Serve { .. } => {
            // No tracing subscriber installed — all tracing macros become
            // no-ops.  This guarantees zero output on both stdout and stderr.
        }
        _ => {
            tracing_subscriber::fmt::init();
        }
    }

    match command {
        Commands::Serve { config } => {
            if let Err(e) = mish::mcp::server::run_server(config.as_deref()).await {
                eprintln!("mish serve: {e}");
                std::process::exit(1);
            }
        }
        Commands::Attach {
            handoff_id,
            share_output,
            pid,
        } => {
            std::process::exit(
                mish::cli::management::cmd_attach(&handoff_id, share_output, pid).await,
            );
        }
        Commands::Ps => {
            std::process::exit(mish::cli::management::cmd_ps());
        }
        Commands::Logs { lines } => {
            let config = load_config("~/.config/mish/mish.toml")
                .unwrap_or_else(|_| mish::config::default_config());
            std::process::exit(mish::cli::management::cmd_logs(lines, &config));
        }
        Commands::Handoffs { watch } => {
            std::process::exit(
                mish::cli::management::cmd_handoffs(watch).await,
            );
        }
        Commands::Config { subcommand } => match subcommand {
            ConfigCommands::Check { config } => {
                std::process::exit(mish::cli::management::cmd_config_check(config.as_deref()));
            }
        },
        Commands::Session { subcommand } => match subcommand {
            SessionCommands::Start { alias, cmd, fg } => {
                std::process::exit(mish::cli::session::cmd_session_start(&alias, &cmd, fg));
            }
            SessionCommands::Send {
                alias,
                input,
                timeout,
            } => {
                std::process::exit(mish::cli::session::cmd_session_send(&alias, &input, timeout));
            }
            SessionCommands::List => {
                std::process::exit(mish::cli::session::cmd_session_list());
            }
            SessionCommands::Close { alias } => {
                std::process::exit(mish::cli::session::cmd_session_close(&alias));
            }
            SessionCommands::Host { alias, cmd } => {
                std::process::exit(mish::cli::session::cmd_session_host(&alias, &cmd));
            }
        },
        Commands::Send { input, timeout } => {
            std::process::exit(mish::cli::session::cmd_send(&input, timeout));
        }
        Commands::Proxy(args) => {
            if args.is_empty() {
                eprintln!("usage: mish <command> [args...]");
                std::process::exit(1);
            }
            let mode = if cli.json {
                OutputMode::Json
            } else if cli.passthrough {
                OutputMode::Passthrough
            } else if cli.context {
                OutputMode::Context
            } else {
                OutputMode::Human
            };
            match mish::cli::proxy::run_with_mode(&args, mode) {
                Ok(exit_code) => std::process::exit(exit_code),
                Err(e) => {
                    eprintln!("mish: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
}
