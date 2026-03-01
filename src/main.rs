use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "mish", version, about = "LLM-native shell")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start MCP server over stdio
    Serve,

    /// Attach to an operator handoff session
    Attach {
        /// Handoff ID
        handoff_id: String,
    },

    /// List running processes
    Ps,

    /// List active operator handoffs
    Handoffs {
        /// Watch for new handoffs (poll every 5s)
        #[arg(long)]
        watch: bool,
    },

    /// CLI proxy mode — run a command with category-aware output
    #[command(external_subcommand)]
    Proxy(Vec<String>),
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve => {
            eprintln!("mish serve: not yet implemented");
        }
        Commands::Attach { handoff_id } => {
            eprintln!("mish attach {handoff_id}: not yet implemented");
        }
        Commands::Ps => {
            eprintln!("mish ps: not yet implemented");
        }
        Commands::Handoffs { watch } => {
            let _ = watch;
            eprintln!("mish handoffs: not yet implemented");
        }
        Commands::Proxy(args) => {
            if args.is_empty() {
                eprintln!("usage: mish <command> [args...]");
                std::process::exit(1);
            }
            eprintln!("mish proxy {:?}: not yet implemented", args);
        }
    }
}
