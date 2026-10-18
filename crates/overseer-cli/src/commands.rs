use std::path::Path;

use anyhow::{Context, Result, bail};
use overseer_core::{Config, NewTask, Run, Task, TaskId};
use overseer_orchestrator::RunSummary;

use crate::cli::{Command, TaskCommand};
use crate::state;

pub fn run(state_dir: Option<std::path::PathBuf>, command: Command) -> Result<()> {
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
            if state::write_starter_config(&dir, &Config::starter(repo))? {
                println!("wrote starter config {}", config_path.display());
            } else {
                println!("kept existing config {}", config_path.display());
            }
            Ok(())
        }
        Command::Task(task) => run_task(&dir, task),
        Command::Run { id } => run_one(&dir, id),
        Command::Runs { task_id } => list_runs(&dir, task_id),
    }
}

fn run_one(dir: &Path, id: i64) -> Result<()> {
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
                })
                .context("failed to add task")?;
            tracing::info!(task_id = task.id.get(), "task added");
            println!("{}", task.id);
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
