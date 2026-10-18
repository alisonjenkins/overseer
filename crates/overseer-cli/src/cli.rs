use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
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
    /// Run a task on its profile's backend, in its own worktree and branch.
    Run {
        /// Id of the task to run.
        id: i64,
    },
    /// List the runs of a task, oldest first.
    Runs {
        /// Id of the task whose runs to list.
        task_id: i64,
    },
    /// Print the diff of a task branch against its base.
    Diff {
        /// Id of the task to diff.
        id: i64,
        /// Show diffstat instead of full diff.
        #[arg(long)]
        stat: bool,
    },
    /// Merge a reviewed task branch into the current branch.
    Merge {
        /// Id of the task to merge.
        id: i64,
    },
    /// Discard a task's worktree and branch, returning it to backlog.
    Discard {
        /// Id of the task to discard.
        id: i64,
    },
    /// Run the review loop manually on a task in review.
    Review {
        /// Id of the task to review.
        id: i64,
    },
    /// Drain the ready task queue.
    Worker {
        /// Exit immediately if no ready tasks are eligible.
        #[arg(long)]
        once: bool,
        /// Maximum number of tasks to run before stopping.
        #[arg(long, value_name = "N")]
        max_tasks: Option<usize>,
        /// Seconds to wait before polling for new tasks when idle.
        #[arg(long, default_value_t = 5, value_name = "SECS")]
        poll_secs: u64,
    },
    /// Print status of the task queue, worker, and recent runs.
    Status,
    /// Check whether configured profiles can run right now.
    Doctor {
        /// Additionally run each healthy backend once with a test prompt.
        #[arg(long)]
        deep: bool,
    },
    /// Inspect or display configuration.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Show or change the routing mode.
    #[command(subcommand)]
    Mode(ModeCommand),
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the starter overseer.toml to stdout.
    Starter {
        /// Repository path for the starter configuration. Defaults to the
        /// current directory.
        #[arg(long, value_name = "PATH")]
        repo: Option<PathBuf>,
    },
    /// Print the path to the configuration file.
    Path,
}

#[derive(Debug, Subcommand)]
pub enum ModeCommand {
    /// Print the current routing mode.
    Show,
    /// Turn credits-low mode on or off. While it is on, metered profiles are
    /// skipped and a quota failure on one of them does not retry it.
    CreditsLow {
        #[arg(value_enum)]
        state: Switch,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Switch {
    On,
    Off,
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
        /// Profile from overseer.toml to run this task on. Defaults to the
        /// config's default_profile.
        #[arg(long, value_name = "NAME")]
        profile: Option<String>,
        /// Make the task ready immediately instead of leaving it in backlog.
        #[arg(long)]
        ready: bool,
    },
    /// Mark a backlog or blocked task as ready.
    Ready {
        /// Id of the task to mark ready.
        id: i64,
    },
    /// List tasks, most urgent first.
    List {
        #[arg(long)]
        status: Option<TaskStatus>,
    },
    /// Show one task in full.
    Show { id: i64 },
}
