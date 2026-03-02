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
        /// Handoff ID
        handoff_id: String,
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
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve { config } => {
            if let Err(e) = mish::mcp::server::run_server(config.as_deref()).await {
                eprintln!("mish serve: {e}");
                std::process::exit(1);
            }
        }
        Commands::Attach { handoff_id } => {
            eprintln!("mish attach {handoff_id}: not yet implemented");
            std::process::exit(1);
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
            let _ = watch;
            eprintln!("mish handoffs: not yet implemented");
            std::process::exit(1);
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
