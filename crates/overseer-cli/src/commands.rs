use std::path::Path;

use anyhow::{Context, Result};
use overseer_core::{Config, NewTask, Task, TaskId};

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
        } => {
            let task = store
                .add_task(NewTask {
                    title,
                    body,
                    priority,
                    profile: None,
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
