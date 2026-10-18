use std::path::PathBuf;

use clap::{Parser, Subcommand};
use overseer_core::TaskStatus;

/// Single-user agentic task orchestrator.
#[derive(Debug, Parser)]
#[command(name = "overseer", version, about)]
pub struct Cli {
    /// Directory holding the overseer state database.
    #[arg(long, global = true, env = "OVERSEER_STATE_DIR", value_name = "DIR")]
    pub state_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create the state directory, database and starter `overseer.toml`.
    Init {
        /// Repository that task worktrees are created from. Defaults to the
        /// current directory.
        #[arg(long, value_name = "PATH")]
        repo: Option<PathBuf>,
    },
    /// Manage tasks.
    #[command(subcommand)]
    Task(TaskCommand),
}

#[derive(Debug, Subcommand)]
pub enum TaskCommand {
    /// Add a task to the backlog and print its id.
    Add {
        title: String,
        #[arg(long, default_value = "")]
        body: String,
        /// Lower numbers are more urgent.
        #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
        priority: i32,
        /// Id of a task that must finish first; repeat for several.
        #[arg(long = "depends-on", value_name = "ID")]
        depends_on: Vec<i64>,
    },
    /// List tasks, most urgent first.
    List {
        #[arg(long)]
        status: Option<TaskStatus>,
    },
    /// Show one task in full.
    Show { id: i64 },
}
