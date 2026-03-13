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
    about = "Coding Agent Sandbox — filesystem isolation tool"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new sandbox in the current directory
    Init {
        /// Project root directory (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
    },

    /// Run a command inside the sandbox
    Run {
        /// Project root directory (defaults to current directory)
        #[arg(long, default_value = ".")]
        root: PathBuf,

        /// The command and arguments to run
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },

    /// Remove FUSE data and reset SHM
    Clean {
        /// Project root directory (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

impl Commands {
    fn get_path(&self) -> PathBuf {
        let path = match self {
            Commands::Init { path } => path,
            Commands::Run { root, .. } => root,
            Commands::Clean { path } => path,
        }
        .to_path_buf();

        path.canonicalize().unwrap_or(path)
    }
}

fn main() {
    let cli = Cli::parse();

    inner_log::init_logger(inner_log::log_level_from_config(&cli.command.get_path()));

    match cli.command {
        Commands::Init { path } => {
            let root = path.canonicalize().unwrap_or(path);
            if let Err(e) = cli::cmd_init(&root) {
                ::log::error!("cas init: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Run { root, command } => {
            let root = root.canonicalize().unwrap_or(root);
            if let Err(e) = cli::cmd_run(&root, &command) {
                ::log::error!("cas run: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Clean { path } => {
            let root = path.canonicalize().unwrap_or(path);
            if let Err(e) = cli::cmd_clean(&root) {
                ::log::error!("cas clean: {}", e);
                std::process::exit(1);
            }
        }
    }
}
