use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use overseer_core::{Config, NewTask, Run, Task, TaskId, TaskStatus};
use overseer_orchestrator::RunSummary;

use crate::cli::{Command, ModeCommand, Switch, TaskCommand};
use crate::state;

/// Exit code for a run that ended with its task blocked, so scripts can tell it
/// from an overseer error (exit 1).
const EXIT_BLOCKED: u8 = 2;

#[derive(Debug)]
pub enum CliError {
    Refusal(String),
    MergeConflict(String),
    Other(anyhow::Error),
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refusal(msg) | Self::MergeConflict(msg) => write!(f, "{msg}"),
            Self::Other(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Other(err) => err.source(),
            _ => None,
        }
    }
}

impl From<anyhow::Error> for CliError {
    fn from(err: anyhow::Error) -> Self {
        Self::Other(err)
    }
}

pub fn run(state_dir: Option<std::path::PathBuf>, command: Command) -> Result<ExitCode, CliError> {
    let dir = state::resolve_state_dir(state_dir)?;
    match command {
        Command::Init { repo } => {
            state::create_store(&dir)?;
            println!("initialised overseer state at {}", dir.display());
            let repo = match repo {
                Some(path) => std::path::absolute(&path).with_context(|| {
                    format!("cannot resolve repository path {}", path.display())
                })?,
                None => std::env::current_dir().context("cannot read the current directory")?,
            };
            let config_path = state::config_path(&dir);
            if state::write_starter_config(&dir, &Config::starter_toml(&repo))? {
                println!("wrote starter config {}", config_path.display());
            } else {
                println!("kept existing config {}", config_path.display());
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Task(task) => {
            run_task(&dir, task)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Run { id } => run_one(&dir, id).map_err(CliError::from),
        Command::Runs { task_id } => {
            list_runs(&dir, task_id)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Diff { id, stat } => run_diff(&dir, id, stat),
        Command::Merge { id } => run_merge(&dir, id),
        Command::Discard { id } => run_discard(&dir, id),
        Command::Mode(mode) => {
            run_mode(&dir, mode)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn run_diff(dir: &Path, id: i64, stat: bool) -> Result<ExitCode, CliError> {
    let config = state::load_config(dir)?;
    let store = state::open_existing_store(dir)?;
    let task_id = TaskId::new(id);
    let _task = store
        .get_task(task_id)
        .with_context(|| format!("failed to find task {id}"))?;
    let branch = overseer_orchestrator::branch_name(task_id);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?;
    let output = if stat {
        runtime.block_on(overseer_orchestrator::diff_stat(&config.repo, &branch))
    } else {
        runtime.block_on(overseer_orchestrator::diff(&config.repo, &branch))
    }
    .with_context(|| format!("failed to diff task {id}"))?;
    print!("{output}");
    Ok(ExitCode::SUCCESS)
}

fn run_merge(dir: &Path, id: i64) -> Result<ExitCode, CliError> {
    let config = state::load_config(dir)?;
    let mut store = state::open_existing_store(dir)?;
    let task_id = TaskId::new(id);
    let task = store
        .get_task(task_id)
        .with_context(|| format!("failed to find task {id}"))?;
    if task.status != TaskStatus::Review {
        return Err(CliError::Refusal(format!(
            "task {id} has status {}; only tasks in review can be merged",
            task.status
        )));
    }
    let branch = overseer_orchestrator::branch_name(task_id);
    let wt_path = overseer_orchestrator::worktree_path(dir, task_id);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?;
    let sha = match runtime.block_on(overseer_orchestrator::merge(
        &config.repo,
        Some(&wt_path),
        &branch,
        task_id,
        &task.title,
    )) {
        Ok(sha) => sha,
        Err(overseer_orchestrator::MergeError::WorkingTreeDirty) => {
            return Err(CliError::Refusal(
                "repository working tree is not clean; commit or stash changes before merging"
                    .to_owned(),
            ));
        }
        Err(overseer_orchestrator::MergeError::WrongBranch {
            task_id,
            current,
            expected,
        }) => {
            return Err(CliError::Refusal(format!(
                "current branch is {current:?}, but task {task_id} branched from {expected:?}; switch to {expected:?} before merging"
            )));
        }
        Err(overseer_orchestrator::MergeError::MergeConflict { paths }) => {
            return Err(CliError::MergeConflict(format!(
                "merge conflict in {}",
                paths.join(", ")
            )));
        }
        Err(err) => {
            return Err(CliError::Other(
                anyhow::Error::from(err).context(format!("failed to merge task {id}")),
            ));
        }
    };
    store
        .set_status(task_id, TaskStatus::Done)
        .context("failed to set task status to done")?;
    println!("{sha}");
    Ok(ExitCode::SUCCESS)
}

fn run_discard(dir: &Path, id: i64) -> Result<ExitCode, CliError> {
    let config = state::load_config(dir)?;
    let mut store = state::open_existing_store(dir)?;
    let task_id = TaskId::new(id);
    let task = store
        .get_task(task_id)
        .with_context(|| format!("failed to find task {id}"))?;
    if task.status != TaskStatus::Review && task.status != TaskStatus::Blocked {
        return Err(CliError::Refusal(format!(
            "task {id} has status {}; only tasks in review or blocked can be discarded",
            task.status
        )));
    }
    let branch = overseer_orchestrator::branch_name(task_id);
    let wt_path = overseer_orchestrator::worktree_path(dir, task_id);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?;
    runtime
        .block_on(overseer_orchestrator::discard(
            &config.repo,
            Some(&wt_path),
            &branch,
        ))
        .with_context(|| format!("failed to discard task {id}"))?;
    store
        .set_status(task_id, TaskStatus::Backlog)
        .context("failed to reset task status to backlog")?;
    Ok(ExitCode::SUCCESS)
}

fn run_one(dir: &Path, id: i64) -> Result<ExitCode> {
    let config = state::load_config(dir)?;
    let mut store = state::open_existing_store(dir)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?;
    let summary = runtime
        .block_on(overseer_orchestrator::run_task(
            &mut store,
            &config,
            dir,
            TaskId::new(id),
        ))
        .with_context(|| format!("failed to run task {id}"))?;
    print_run_summary(&summary);
    Ok(if summary.status == TaskStatus::Blocked {
        ExitCode::from(EXIT_BLOCKED)
    } else {
        ExitCode::SUCCESS
    })
}

fn run_mode(dir: &Path, command: ModeCommand) -> Result<()> {
    let mut store = state::open_existing_store(dir)?;
    match command {
        ModeCommand::Show => {}
        ModeCommand::CreditsLow { state } => {
            store
                .set_credits_low(state == Switch::On)
                .context("failed to set credits-low mode")?;
        }
    }
    let credits_low = store
        .credits_low()
        .context("failed to read credits-low mode")?;
    println!("credits-low: {}", if credits_low { "on" } else { "off" });
    Ok(())
}

fn list_runs(dir: &Path, task_id: i64) -> Result<()> {
    let store = state::open_existing_store(dir)?;
    let id = TaskId::new(task_id);
    store
        .get_task(id)
        .with_context(|| format!("failed to list runs of task {task_id}"))?;
    let runs = store.list_runs(id).context("failed to list runs")?;
    print_runs(&runs);
    Ok(())
}

fn print_run_summary(summary: &RunSummary) {
    println!("run: {}", summary.run_id);
    println!("task: {} -> {}", summary.task_id, summary.status);
    match &summary.failure {
        None => println!("outcome: succeeded"),
        Some(kind) => println!("outcome: failed ({kind})"),
    }
    println!("profile: {}", summary.profile);
    println!("branch: {}", summary.branch);
    println!("commit: {}", summary.commit.as_deref().unwrap_or("-"));
    println!(
        "tokens: input {}, output {}",
        summary.usage.input_tokens, summary.usage.output_tokens
    );
    match &summary.transcript_path {
        Some(path) => println!("transcript: {}", path.display()),
        None => println!("transcript: -"),
    }
    println!("attempts:");
    for attempt in &summary.attempts {
        let outcome = attempt
            .failure
            .as_ref()
            .map_or_else(|| "succeeded".to_owned(), |kind| format!("failed ({kind})"));
        println!("  run {}: {} {outcome}", attempt.run_id, attempt.profile);
    }
}

fn print_runs(runs: &[Run]) {
    println!(
        "{:>5}  {:<12}  {:<10}  {:>8}  {:>8}  {:<20}  BRANCH",
        "ID", "PROFILE", "STATUS", "INPUT", "OUTPUT", "FAILURE"
    );
    for run in runs {
        println!(
            "{:>5}  {:<12}  {:<10}  {:>8}  {:>8}  {:<20}  {}",
            run.id,
            run.profile,
            run.status.as_str(),
            run.usage.input_tokens,
            run.usage.output_tokens,
            run.failure.as_deref().unwrap_or("-"),
            run.branch.as_deref().unwrap_or("-"),
        );
    }
}

fn run_task(dir: &Path, command: TaskCommand) -> Result<()> {
    let mut store = state::open_existing_store(dir)?;
    match command {
        TaskCommand::Add {
            title,
            body,
            priority,
            depends_on,
            profile,
            ready,
        } => {
            if let Some(name) = profile.as_deref() {
                let config = state::load_config(dir)?;
                if config.profile(name).is_none() {
                    bail!(
                        "profile {name:?} is not defined in {}",
                        state::config_path(dir).display()
                    );
                }
            }
            let task = store
                .add_task(NewTask {
                    title,
                    body,
                    priority,
                    profile,
                    depends_on: depends_on.into_iter().map(TaskId::new).collect(),
                    ready,
                })
                .context("failed to add task")?;
            tracing::info!(task_id = task.id.get(), "task added");
            println!("{}", task.id);
        }
        TaskCommand::Ready { id } => {
            let task = store
                .ready_task(TaskId::new(id))
                .with_context(|| format!("failed to mark task {id} ready"))?;
            tracing::info!(task_id = task.id.get(), "task marked ready");
            println!("task {} ready", task.id);
        }
        TaskCommand::List { status } => {
            let tasks = store.list_tasks(status).context("failed to list tasks")?;
            print_list(&tasks);
        }
        TaskCommand::Show { id } => {
            let task = store
                .get_task(TaskId::new(id))
                .with_context(|| format!("failed to show task {id}"))?;
            print_task(&task);
        }
    }
    Ok(())
}

fn print_list(tasks: &[Task]) {
    println!("{:>5}  {:<12}  {:>8}  TITLE", "ID", "STATUS", "PRIORITY");
    for task in tasks {
        println!(
            "{:>5}  {:<12}  {:>8}  {}",
            task.id,
            task.status.as_str(),
            task.priority,
            task.title
        );
    }
}

fn print_task(task: &Task) {
    let depends_on = if task.depends_on.is_empty() {
        "-".to_owned()
    } else {
        task.depends_on
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    println!("id: {}", task.id);
    println!("title: {}", task.title);
    println!("status: {}", task.status.as_str());
    println!("priority: {}", task.priority);
    println!("profile: {}", task.profile.as_deref().unwrap_or("-"));
    println!("round: {}", task.round);
    println!("depends_on: {depends_on}");
    println!("created_at: {}", task.created_at);
    if !task.body.is_empty() {
        println!();
        println!("{}", task.body);
    }
}
