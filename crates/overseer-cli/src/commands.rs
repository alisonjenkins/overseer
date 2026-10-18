use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use overseer_core::{
    BackendKind, Config, Lesson, LessonId, LessonStatus, NewTask, Run, Task, TaskId, TaskStatus,
};
use overseer_orchestrator::{
    RunSummary, ShutdownSignal, WorkerError, WorkerLockError, WorkerOptions, check_worker_status,
    doctor_report,
};

use crate::cli::{
    Command, ConfigCommand, LessonsCommand, ModeCommand, SkillsCommand, Switch, TaskCommand,
};
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

impl From<overseer_store::StoreError> for CliError {
    fn from(err: overseer_store::StoreError) -> Self {
        Self::Other(err.into())
    }
}

/// Exit code when another worker holds the lock.
const EXIT_LOCKED: u8 = 3;

pub fn run(state_dir: Option<std::path::PathBuf>, command: Command) -> Result<ExitCode, CliError> {
    if let Command::Config(ConfigCommand::Starter { repo }) = command {
        let repo = match repo {
            Some(path) => std::path::absolute(&path)
                .with_context(|| format!("cannot resolve repository path {}", path.display()))?,
            None => std::env::current_dir().context("cannot read the current directory")?,
        };
        print!("{}", Config::starter_toml(&repo));
        return Ok(ExitCode::SUCCESS);
    }

    let dir = state::resolve_state_dir(state_dir)?;
    match command {
        Command::Config(ConfigCommand::Path) => {
            let path = state::config_path(&dir);
            println!("{}", path.display());
            Ok(ExitCode::SUCCESS)
        }
        Command::Config(ConfigCommand::Starter { .. }) => Ok(ExitCode::SUCCESS),
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
        Command::Review { id } => run_review(&dir, id),
        Command::Approve { id } => run_approve(&dir, id),
        Command::Worker {
            once,
            max_tasks,
            poll_secs,
        } => run_worker_cmd(&dir, once, max_tasks, poll_secs).map_err(CliError::from),
        Command::Status => {
            run_status(&dir)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Doctor { deep } => run_doctor(&dir, deep).map_err(CliError::from),
        Command::Mode(mode) => {
            run_mode(&dir, mode)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Skills(skills) => {
            run_skills(&dir, skills)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Lessons(lessons) => run_lessons(&dir, lessons),
        Command::Retro { task_id } => run_retro(&dir, task_id),
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

fn reconcile(store: &mut overseer_store::Store, repo: &Path) -> Result<()> {
    let review_tasks = store
        .list_tasks(Some(TaskStatus::Review))
        .context("failed to list review tasks")?;
    let needs_human_tasks = store
        .list_tasks(Some(TaskStatus::NeedsHuman))
        .context("failed to list needs-human tasks")?;
    for task in review_tasks.into_iter().chain(needs_human_tasks) {
        let branch = overseer_orchestrator::branch_name(task.id);
        if let Ok(true) = overseer_orchestrator::branch_content_on_head(repo, &branch) {
            store
                .set_status(task.id, TaskStatus::Done)
                .with_context(|| format!("failed to set task {} status to done", task.id))?;
            println!("task {}: branch already on HEAD, marked done", task.id);
        }
    }
    Ok(())
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
    let reviews = store
        .list_reviews(task_id)
        .with_context(|| format!("failed to list reviews for task {id}"))?;
    if !reviews.is_empty() && !task.approved {
        eprintln!("warning: task {id} has reviews and is not approved");
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
    let task_id = TaskId::new(id);
    let summary = runtime
        .block_on(overseer_orchestrator::run_task(
            &mut store, &config, dir, task_id,
        ))
        .with_context(|| format!("failed to run task {id}"))?;
    print_run_summary(&summary);

    let final_status = if summary.status == TaskStatus::Review && !config.review.profiles.is_empty()
    {
        runtime
            .block_on(overseer_orchestrator::run_review_loop(
                &mut store, &config, dir, task_id,
            ))
            .with_context(|| format!("failed to review task {id}"))?
    } else {
        summary.status
    };

    Ok(if final_status == TaskStatus::Blocked {
        ExitCode::from(EXIT_BLOCKED)
    } else {
        ExitCode::SUCCESS
    })
}

fn run_review(dir: &Path, id: i64) -> Result<ExitCode, CliError> {
    let config = state::load_config(dir)?;
    let mut store = state::open_existing_store(dir)?;
    let task_id = TaskId::new(id);
    let task = store
        .get_task(task_id)
        .with_context(|| format!("failed to find task {id}"))?;
    if task.status != TaskStatus::Review {
        return Err(CliError::Refusal(format!(
            "task {id} has status {}; only tasks in review can be reviewed",
            task.status
        )));
    }
    if config.review.profiles.is_empty() {
        return Err(CliError::Refusal(
            "review is disabled: no reviewer profiles configured in overseer.toml".to_owned(),
        ));
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?;
    let final_status = runtime
        .block_on(overseer_orchestrator::run_review_loop(
            &mut store, &config, dir, task_id,
        ))
        .with_context(|| format!("failed to review task {id}"))?;

    Ok(if final_status == TaskStatus::Blocked {
        ExitCode::from(EXIT_BLOCKED)
    } else {
        ExitCode::SUCCESS
    })
}

fn run_approve(dir: &Path, id: i64) -> Result<ExitCode, CliError> {
    let mut store = state::open_existing_store(dir)?;
    let task_id = TaskId::new(id);
    let task = store
        .get_task(task_id)
        .with_context(|| format!("failed to find task {id}"))?;

    if task.status != TaskStatus::NeedsHuman {
        return Err(CliError::Refusal(format!(
            "task {id} has status {}; only tasks in needs-human can be approved",
            task.status
        )));
    }

    let Some(ref reason) = task.reason else {
        return Err(CliError::Refusal(format!(
            "task {id} does not have an escalation reason to approve"
        )));
    };

    if !reason.starts_with("escalation") {
        return Err(CliError::Refusal(format!(
            "task {id} reason {reason:?} is not an escalation reason"
        )));
    }

    let approved_reason = format!("approved: {reason}");
    store
        .set_approved(task_id, true)
        .with_context(|| format!("failed to approve task {id}"))?;
    store
        .set_status_with_reason(task_id, TaskStatus::Review, Some(&approved_reason))
        .with_context(|| format!("failed to move task {id} to review"))?;

    println!("task {id} approved");
    Ok(ExitCode::SUCCESS)
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

fn run_skills(dir: &Path, command: SkillsCommand) -> Result<()> {
    state::open_existing_store(dir)?;
    let skills_dir = dir.join("skills");
    match command {
        SkillsCommand::List => {
            let skills = overseer_store::skills::list_skills(&skills_dir)?;
            print_skills(&skills);
        }
        SkillsCommand::Show { name, version } => {
            let content = match version {
                Some(v) => overseer_store::skills::read_skill_version(&skills_dir, &name, v)?,
                None => match overseer_store::skills::read_current_skill(&skills_dir, &name)? {
                    Some((_, c)) => c,
                    None => bail!("skill '{name}' not found"),
                },
            };
            print!("{content}");
            if !content.ends_with('\n') {
                println!();
            }
        }
        SkillsCommand::Add { name, file } => {
            let content = std::fs::read_to_string(&file)
                .with_context(|| format!("failed to read skill file {}", file.display()))?;
            let version =
                overseer_store::skills::create_next_version(&skills_dir, &name, &content)?;
            println!("skill {name} v{version} created");
        }
        SkillsCommand::UseVersion { name, version } => {
            overseer_store::skills::set_current_version(&skills_dir, &name, version)?;
            println!("skill {name} set to v{version}");
        }
    }
    Ok(())
}

fn print_skills(skills: &[overseer_store::skills::SkillSummary]) {
    let name_width = skills
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(4)
        .max("NAME".len());
    println!(
        "{:<w$}  {:>7}  {:>8}",
        "NAME",
        "VERSION",
        "SIZE",
        w = name_width
    );
    for skill in skills {
        println!(
            "{:<w$}  {:>7}  {:>8}",
            skill.name,
            format!("v{}", skill.current_version),
            skill.size_bytes,
            w = name_width
        );
    }
}

fn run_lessons(dir: &Path, command: LessonsCommand) -> Result<ExitCode, CliError> {
    let mut store = state::open_existing_store(dir)?;
    match command {
        LessonsCommand::List { status } => {
            let lessons = store.list_lessons(status)?;
            print_lessons(&lessons);
        }
        LessonsCommand::Show { id } => {
            let lesson_id = LessonId::new(id);
            let _ = store.get_lesson(lesson_id)?;
            let diff_path = dir.join("lessons").join(format!("{id}.diff"));
            let diff = std::fs::read_to_string(&diff_path)
                .with_context(|| format!("diff file {} not found", diff_path.display()))?;
            print!("{diff}");
            if !diff.ends_with('\n') {
                println!();
            }
        }
        LessonsCommand::Apply { id } => {
            let lesson_id = LessonId::new(id);
            let lesson = store.get_lesson(lesson_id)?;
            if lesson.status != LessonStatus::Proposed {
                return Err(CliError::Refusal(format!(
                    "lesson {id} has status {}; only proposed lessons can be applied",
                    lesson.status.as_str()
                )));
            }
            let skills_dir = dir.join("skills");
            let current =
                overseer_store::skills::current_version(&skills_dir, &lesson.skill)?.unwrap_or(0);
            if current != lesson.base_version {
                return Err(CliError::Refusal(format!(
                    "skill '{}' current version is v{current}, but lesson {id} was based on v{}; refusing to apply",
                    lesson.skill, lesson.base_version
                )));
            }
            let proposed_path = if lesson.path.as_os_str().is_empty() {
                dir.join("lessons").join(format!("{id}.md"))
            } else {
                lesson.path.clone()
            };
            let proposed_text = std::fs::read_to_string(&proposed_path).with_context(|| {
                format!(
                    "failed to read proposed lesson from {}",
                    proposed_path.display()
                )
            })?;
            let new_version = overseer_store::skills::create_next_version(
                &skills_dir,
                &lesson.skill,
                &proposed_text,
            )?;
            store.set_lesson_status(lesson_id, LessonStatus::Applied)?;
            println!(
                "applied lesson {id}: skill '{}' updated to v{new_version}",
                lesson.skill
            );
        }
        LessonsCommand::Reject { id } => {
            let lesson_id = LessonId::new(id);
            let lesson = store.get_lesson(lesson_id)?;
            if lesson.status != LessonStatus::Proposed {
                return Err(CliError::Refusal(format!(
                    "lesson {id} has status {}; only proposed lessons can be rejected",
                    lesson.status.as_str()
                )));
            }
            store.set_lesson_status(lesson_id, LessonStatus::Rejected)?;
            println!("lesson {id} rejected");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_lessons(lessons: &[Lesson]) {
    println!(
        "{:>5}  {:>5}  {:<16}  {:>4}  {:<10}  CREATED",
        "ID", "TASK", "SKILL", "BASE", "STATUS"
    );
    for l in lessons {
        println!(
            "{:>5}  {:>5}  {:<16}  {:>4}  {:<10}  {}",
            l.id,
            l.task_id,
            l.skill,
            format!("v{}", l.base_version),
            l.status.as_str(),
            l.created_at
        );
    }
}

fn run_retro(dir: &Path, task_id: i64) -> Result<ExitCode, CliError> {
    let config = state::load_config(dir)?;
    if config.learning.profile.is_none() {
        return Err(CliError::Refusal(
            "no learning profile configured in overseer.toml".to_owned(),
        ));
    }
    let mut store = state::open_existing_store(dir)?;
    let id = TaskId::new(task_id);
    let _ = store.get_task(id)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?;
    let outcome = runtime
        .block_on(overseer_orchestrator::retrospect_task(
            &mut store, &config, dir, id,
        ))
        .map_err(anyhow::Error::from)?;
    match outcome {
        Some(lesson) => {
            println!("lesson {} proposed for skill {}", lesson.id, lesson.skill);
        }
        None => {
            println!("no lesson proposed (learning profile replied NO CHANGE)");
        }
    }
    Ok(ExitCode::SUCCESS)
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
    if let Some(ref reason) = summary.reason {
        println!("reason: {reason}");
    }
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
            let task_id = TaskId::new(id);
            let task = store
                .get_task(task_id)
                .with_context(|| format!("failed to show task {id}"))?;
            let reviews = store
                .list_reviews(task_id)
                .with_context(|| format!("failed to list reviews for task {id}"))?;
            print_task(&task, &reviews);
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

fn print_task(task: &Task, reviews: &[overseer_core::ReviewRecord]) {
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
    println!("approved: {}", task.approved);
    println!("depends_on: {depends_on}");
    println!("created_at: {}", task.created_at);
    if let Some(ref reason) = task.reason {
        println!("reason: {reason}");
    }
    if !reviews.is_empty() {
        println!();
        println!("reviews:");
        println!(
            "{:>5}  {:>5}  {:<12}  {:<8}  PATH",
            "ID", "ROUND", "PROFILE", "VERDICT"
        );
        for r in reviews {
            println!(
                "{:>5}  {:>5}  {:<12}  {:<8}  {}",
                r.id,
                r.round,
                r.profile,
                r.verdict.as_str(),
                r.path.display()
            );
        }
    }
    if !task.body.is_empty() {
        println!();
        println!("{}", task.body);
    }
}

fn run_worker_cmd(
    dir: &Path,
    once: bool,
    max_tasks: Option<usize>,
    poll_secs: u64,
) -> Result<ExitCode> {
    let config = state::load_config(dir)?;
    let mut store = state::open_existing_store(dir)?;

    if let Err(err) = reconcile(&mut store, &config.repo) {
        eprintln!("warning: reconcile failed: {err:#}");
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let shutdown_signal = ShutdownSignal::new(shutdown_rx);

    runtime.spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return,
        };

        tokio::select! {
            _ = sigint.recv() => {},
            _ = sigterm.recv() => {},
        }
        println!("stopping after current task");
        let _ = shutdown_tx.send(true);

        tokio::select! {
            _ = sigint.recv() => {},
            _ = sigterm.recv() => {},
        }
        std::process::exit(130);
    });

    let options = WorkerOptions {
        once,
        max_tasks,
        poll_interval: std::time::Duration::from_secs(poll_secs),
    };

    let result = runtime.block_on(overseer_orchestrator::run_worker(
        &mut store,
        &config,
        dir,
        options,
        shutdown_signal,
    ));

    match result {
        Ok(_) => Ok(ExitCode::SUCCESS),
        Err(WorkerError::Lock(WorkerLockError::AlreadyLocked { pid })) => {
            let pid_msg = match pid {
                Some(p) => format!(" (pid {p})"),
                None => String::new(),
            };
            eprintln!("overseer: another worker is already running{pid_msg}");
            Ok(ExitCode::from(EXIT_LOCKED))
        }
        Err(err) => bail!("worker failed: {err}"),
    }
}

fn run_status(dir: &Path) -> Result<()> {
    let mut store = state::open_existing_store(dir)?;
    match state::load_config(dir) {
        Ok(config) => {
            if let Err(err) = reconcile(&mut store, &config.repo) {
                eprintln!("warning: reconcile failed: {err:#}");
            }
        }
        Err(err) => {
            eprintln!("warning: reconcile failed: {err:#}");
        }
    }

    let worker_pid = check_worker_status(dir).context("failed to inspect worker status")?;
    match worker_pid {
        Some(pid) => println!("worker: running (pid {pid})"),
        None => println!("worker: not running"),
    }

    let credits_low = store
        .credits_low()
        .context("failed to read credits-low mode")?;
    println!("credits-low: {}", if credits_low { "on" } else { "off" });

    println!();
    println!("TASKS");
    println!("{:<12}  {:>5}", "STATUS", "COUNT");
    let counts = store
        .task_counts_by_status()
        .context("failed to count tasks")?;
    for (status, count) in counts {
        println!("{:<12}  {:>5}", status.as_str(), count);
    }

    println!();
    println!("RECENT RUNS");
    println!(
        "{:>5}  {:>5}  {:<12}  {:<10}  {:>12}",
        "ID", "TASK", "PROFILE", "STATUS", "TOKENS"
    );
    let runs = store
        .list_recent_runs(5)
        .context("failed to list recent runs")?;
    for run in runs {
        let tokens = format!("{}/{}", run.usage.input_tokens, run.usage.output_tokens);
        println!(
            "{:>5}  {:>5}  {:<12}  {:<10}  {:>12}",
            run.id,
            run.task_id,
            run.profile,
            run.status.as_str(),
            tokens
        );
    }

    Ok(())
}

fn run_doctor(dir: &Path, deep: bool) -> Result<ExitCode> {
    let config = state::load_config(dir)?;
    let credits_low = match state::open_existing_store(dir) {
        Ok(store) => store.credits_low().unwrap_or(false),
        Err(_) => false,
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?;

    let mut checks = std::collections::HashMap::new();
    for profile in &config.profiles {
        let health = runtime.block_on(overseer_backends::check_profile(profile, deep));
        checks.insert(profile.name.clone(), health);
    }

    let report = doctor_report(&config, credits_low, &checks);
    print_doctor_report(&report);

    if report.verdict.is_usable() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(2))
    }
}

fn print_doctor_report(report: &overseer_orchestrator::Report) {
    let profile_width = report
        .profiles
        .iter()
        .map(|p| p.name.len())
        .max()
        .unwrap_or(7)
        .max("PROFILE".len());
    let backend_width = report
        .profiles
        .iter()
        .map(|p| match p.backend {
            BackendKind::Openai => "openai".len(),
            BackendKind::Command => "command".len(),
        })
        .max()
        .unwrap_or(7)
        .max("BACKEND".len());
    let health_width = report
        .profiles
        .iter()
        .map(|p| {
            if p.health.ok {
                "ok".len()
            } else {
                "unhealthy".len()
            }
        })
        .max()
        .unwrap_or(7)
        .max("HEALTH".len());
    let metered_width = "METERED".len();

    println!(
        "{:<p_w$}  {:<b_w$}  {:<h_w$}  {:<m_w$}  DETAIL",
        "PROFILE",
        "BACKEND",
        "HEALTH",
        "METERED",
        p_w = profile_width,
        b_w = backend_width,
        h_w = health_width,
        m_w = metered_width,
    );

    for p in &report.profiles {
        let health_str = if p.health.ok { "ok" } else { "unhealthy" };
        let metered_str = if p.metered { "yes" } else { "no" };
        let detail_str = if p.health.detail.is_empty() {
            "-"
        } else {
            &p.health.detail
        };
        println!(
            "{:<p_w$}  {:<b_w$}  {:<h_w$}  {:<m_w$}  {}",
            p.name,
            match p.backend {
                BackendKind::Openai => "openai",
                BackendKind::Command => "command",
            },
            health_str,
            metered_str,
            detail_str,
            p_w = profile_width,
            b_w = backend_width,
            h_w = health_width,
            m_w = metered_width,
        );
    }

    println!();
    let plan_str = if report.default_plan.is_empty() {
        "(none)".to_owned()
    } else {
        report
            .default_plan
            .iter()
            .map(|entry| {
                if entry.health.ok {
                    entry.name.clone()
                } else {
                    format!("{} (unhealthy)", entry.name)
                }
            })
            .collect::<Vec<_>>()
            .join(" -> ")
    };
    println!("default plan: {plan_str}");
    println!(
        "credits-low: {}",
        if report.credits_low { "on" } else { "off" }
    );
    println!("verdict: {}", report.verdict);
}
