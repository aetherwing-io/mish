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

    #[command(subcommand)]
    command: Commands,
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

    /// CLI proxy mode — run a command with category-aware output
    #[command(external_subcommand)]
    Proxy(Vec<String>),
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

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // In MCP serve mode, stdout is the JSON-RPC transport — tracing must
    // not write there.  Disable tracing entirely so nothing leaks to
    // stdout or stderr.  Other subcommands keep the default subscriber.
    match &cli.command {
        Commands::Serve { .. } => {
            // No tracing subscriber installed — all tracing macros become
            // no-ops.  This guarantees zero output on both stdout and stderr.
        }
        _ => {
            tracing_subscriber::fmt::init();
        }
    }

    match cli.command {
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
