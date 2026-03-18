mod cli;
mod config;
mod fuse;
#[path = "log.rs"]
mod inner_log;
mod isolate;
mod shm;
mod syncing;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "cas",
    about = "Coding Agent Sandbox — filesystem isolation tool",
    disable_help_subcommand = true,
    allow_hyphen_values = true
)]
struct Cli {
    /// Project root directory (defaults to current directory)
    #[arg(short, long, default_value = ".")]
    root: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize or reset sandbox (creates if not exists, cleans if exists)
    Init,
    /// Clean data directory or initialize sandbox if not exists
    Clean,
    /// Delete entire .sandbox directory
    Purge,
    /// Run a command inside the sandbox (auto-initializes if not exists)
    #[command(name = "run")]
    Run {
        /// The command and arguments to run
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
}

fn main() {
    let cli = Cli::parse();

    let root = cli.root.canonicalize().unwrap_or(cli.root.clone());
    inner_log::init_logger(inner_log::log_level_from_config(&root));

    match cli.command {
        Commands::Init => {
            if let Err(e) = cli::cmd_init(&root) {
                log::error!("cas init: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Clean => {
            if let Err(e) = cli::cmd_clean(&root) {
                log::error!("cas clean: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Purge => {
            if let Err(e) = cli::cmd_purge(&root) {
                log::error!("cas purge: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Run { command } => {
            if let Err(e) = cli::cmd_run(&root, &command) {
                log::error!("cas run: {}", e);
                std::process::exit(1);
            }
        }
    }
}
