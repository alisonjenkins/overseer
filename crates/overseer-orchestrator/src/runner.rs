//! Executes one task: claim, worktree, backend call, transcript, commit, and
//! the final task status.

use std::path::{Path, PathBuf};

use overseer_backends::{Backend, BackendError, build_backend};
use overseer_core::{
    Config, FailureKind, Profile, RunFinish, RunId, RunOutcome, RunRequest, RunStatus, Task,
    TaskId, TaskStatus, Usage,
};
use overseer_store::{Store, StoreError};

use crate::worktree::{self, Worktree};

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("task {task_id} names profile {profile:?}, which is not in overseer.toml")]
    UnknownProfile { task_id: TaskId, profile: String },

    #[error("failed to build the backend for profile {profile:?}")]
    BuildBackend {
        profile: String,
        #[source]
        source: BackendError,
    },

    /// One variant for every store call; `step` names the call that failed.
    #[error("store step {step} failed")]
    Store {
        step: &'static str,
        #[source]
        source: StoreError,
    },
}

/// What a finished run looked like. `status` is the task status it left
/// behind: `Review` on success, `Blocked` on failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSummary {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub profile: String,
    pub status: TaskStatus,
    pub failure: Option<FailureKind>,
    pub branch: String,
    pub commit: Option<String>,
    pub usage: Usage,
    pub transcript_path: Option<PathBuf>,
}

/// Run a task on the backend its profile names (or `default_profile`).
pub async fn run_task(
    store: &mut Store,
    config: &Config,
    state_dir: &Path,
    task_id: TaskId,
) -> Result<RunSummary, RunnerError> {
    let task = store.get_task(task_id).map_err(store_step("load task"))?;
    let profile = select_profile(config, &task)?;
    let backend = build_backend(profile).map_err(|source| RunnerError::BuildBackend {
        profile: profile.name.clone(),
        source,
    })?;
    execute(store, config, state_dir, task_id, profile, backend.as_ref()).await
}

/// Like `run_task`, but with the backend supplied by the caller. Used by tests
/// to substitute a `MockBackend`.
pub async fn run_task_with_backend(
    store: &mut Store,
    config: &Config,
    state_dir: &Path,
    task_id: TaskId,
    backend: &dyn Backend,
) -> Result<RunSummary, RunnerError> {
    let task = store.get_task(task_id).map_err(store_step("load task"))?;
    let profile = select_profile(config, &task)?;
    execute(store, config, state_dir, task_id, profile, backend).await
}

fn select_profile<'a>(config: &'a Config, task: &Task) -> Result<&'a Profile, RunnerError> {
    let name = task.profile.as_deref().unwrap_or(&config.default_profile);
    config
        .profile(name)
        .ok_or_else(|| RunnerError::UnknownProfile {
            task_id: task.id,
            profile: name.to_owned(),
        })
}

fn store_step(step: &'static str) -> impl FnOnce(StoreError) -> RunnerError {
    move |source| RunnerError::Store { step, source }
}

/// The result of the backend call and the commit step, before the run is
/// written to the store.
struct Attempt {
    output: String,
    usage: Usage,
    failure: Option<FailureKind>,
    commit: Option<String>,
}

impl Attempt {
    fn failed(kind: FailureKind) -> Self {
        Self {
            output: String::new(),
            usage: Usage::default(),
            failure: Some(kind),
            commit: None,
        }
    }
}

async fn execute(
    store: &mut Store,
    config: &Config,
    state_dir: &Path,
    task_id: TaskId,
    profile: &Profile,
    backend: &dyn Backend,
) -> Result<RunSummary, RunnerError> {
    let task = store
        .claim_task(task_id)
        .map_err(store_step("claim task"))?;
    let run_id = store
        .start_run(task_id, &profile.name)
        .map_err(store_step("start run"))?;

    let (branch, mut attempt) = match worktree::create(&config.repo, state_dir, task_id).await {
        Ok(worktree) => {
            let attempt = run_in_worktree(backend, profile, &task, &worktree).await;
            (worktree.branch, attempt)
        }
        Err(err) => (
            worktree::branch_name(task_id),
            Attempt::failed(FailureKind::Other(format!("worktree: {err}"))),
        ),
    };

    let transcript_path =
        match write_transcript(state_dir, run_id, &task, profile, &branch, &attempt).await {
            Ok(path) => Some(path),
            Err(err) => {
                attempt
                    .failure
                    .get_or_insert(FailureKind::Other(format!("transcript: {err}")));
                None
            }
        };

    let status = if attempt.failure.is_none() {
        RunStatus::Succeeded
    } else {
        RunStatus::Failed
    };
    let task_status = if attempt.failure.is_none() {
        TaskStatus::Review
    } else {
        TaskStatus::Blocked
    };
    store
        .finish_run(
            run_id,
            RunFinish {
                status,
                usage: attempt.usage,
                failure: attempt.failure.as_ref().map(ToString::to_string),
                transcript_path: transcript_path.clone(),
                branch: Some(branch.clone()),
            },
        )
        .map_err(store_step("finish run"))?;
    store
        .set_status(task_id, task_status)
        .map_err(store_step("set task status"))?;

    Ok(RunSummary {
        run_id,
        task_id,
        profile: profile.name.clone(),
        status: task_status,
        failure: attempt.failure,
        branch,
        commit: attempt.commit,
        usage: attempt.usage,
        transcript_path,
    })
}

async fn run_in_worktree(
    backend: &dyn Backend,
    profile: &Profile,
    task: &Task,
    worktree: &Worktree,
) -> Attempt {
    let request = RunRequest {
        system: profile.role_prompt.clone(),
        prompt: build_prompt(task),
        workdir: worktree.path.clone(),
        permissions: profile.permissions,
    };
    let outcome = match backend.run(&request).await {
        Ok(outcome) => outcome,
        Err(err) => {
            return Attempt::failed(FailureKind::Other(format!("backend error: {err}")));
        }
    };
    let RunOutcome {
        output,
        usage,
        failure,
    } = outcome;
    let mut attempt = Attempt {
        output,
        usage,
        failure,
        commit: None,
    };
    // Changes from a failed run stay uncommitted in the worktree for
    // inspection; only a successful run is committed to the task branch.
    if attempt.failure.is_none() {
        match worktree::commit_all(&worktree.path, &commit_message(task)).await {
            Ok(sha) => attempt.commit = sha,
            Err(err) => attempt.failure = Some(FailureKind::Other(format!("commit: {err}"))),
        }
    }
    attempt
}

fn build_prompt(task: &Task) -> String {
    let heading = format!("Task {}: {}", task.id, task.title);
    if task.body.trim().is_empty() {
        heading
    } else {
        format!("{heading}\n\n{}", task.body)
    }
}

fn commit_message(task: &Task) -> String {
    format!("overseer: task {}: {}", task.id, task.title)
}

/// Write the transcript atomically: temp file, then rename.
async fn write_transcript(
    state_dir: &Path,
    run_id: RunId,
    task: &Task,
    profile: &Profile,
    branch: &str,
    attempt: &Attempt,
) -> Result<PathBuf, std::io::Error> {
    let dir = state_dir.join("runs");
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{run_id}.md"));
    let tmp = dir.join(format!("{run_id}.md.tmp"));
    let body = render_transcript(run_id, task, profile, branch, attempt);
    tokio::fs::write(&tmp, body).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(path)
}

fn render_transcript(
    run_id: RunId,
    task: &Task,
    profile: &Profile,
    branch: &str,
    attempt: &Attempt,
) -> String {
    let failure = attempt
        .failure
        .as_ref()
        .map_or_else(|| "none".to_owned(), ToString::to_string);
    let commit = attempt.commit.as_deref().unwrap_or("none");
    let system = profile.role_prompt.as_deref().unwrap_or("(none)");
    format!(
        "# Run {run_id}: task {task_id}\n\n\
         - profile: {profile_name}\n\
         - branch: {branch}\n\
         - commit: {commit}\n\
         - failure: {failure}\n\
         - input tokens: {input}\n\
         - output tokens: {output}\n\n\
         ## System prompt\n\n{system}\n\n\
         ## Prompt\n\n{prompt}\n\n\
         ## Output\n\n{output_text}\n",
        task_id = task.id,
        profile_name = profile.name,
        input = attempt.usage.input_tokens,
        output = attempt.usage.output_tokens,
        prompt = build_prompt(task),
        output_text = attempt.output,
    )
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]

    use async_trait::async_trait;
    use overseer_backends::MockBackend;
    use overseer_core::{BackendKind, NewTask, Permissions};
    use tempfile::TempDir;

    use super::*;
    use crate::test_support::{git_sync, init_repo};

    /// Writes one file into the worktree and reports fixed usage.
    struct WritingBackend {
        file: &'static str,
        content: &'static str,
    }

    #[async_trait]
    impl Backend for WritingBackend {
        async fn run(&self, req: &RunRequest) -> Result<RunOutcome, BackendError> {
            std::fs::write(req.workdir.join(self.file), self.content).unwrap();
            Ok(RunOutcome {
                output: "wrote the file".to_owned(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
                failure: None,
            })
        }
    }

    struct Fixture {
        _root: TempDir,
        repo: PathBuf,
        state: PathBuf,
        store: Store,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let state = root.path().join("state");
        Fixture {
            _root: root,
            repo,
            state,
            store: Store::open_in_memory().unwrap(),
        }
    }

    fn profile(name: &str) -> Profile {
        Profile {
            name: name.to_owned(),
            backend: BackendKind::Openai,
            model: Some("local".to_owned()),
            base_url: Some("http://127.0.0.1:1/v1".to_owned()),
            api_key_env: None,
            command: None,
            prompt_stdin: false,
            role_prompt: Some("You are careful.".to_owned()),
            fallback: Vec::new(),
            permissions: Permissions::WriteWorkdir,
            timeout_secs: 5,
            metered: false,
            permission_args: Default::default(),
        }
    }

    fn config(repo: &Path) -> Config {
        Config {
            repo: repo.to_path_buf(),
            default_profile: "local".to_owned(),
            profiles: vec![profile("local"), profile("alt")],
        }
    }

    fn add(store: &mut Store, title: &str, body: &str) -> TaskId {
        store
            .add_task(NewTask {
                title: title.to_owned(),
                body: body.to_owned(),
                ..NewTask::default()
            })
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn successful_run_commits_and_moves_task_to_review() {
        let mut fx = fixture();
        let config = config(&fx.repo);
        let task = add(&mut fx.store, "write parser", "handle quotes");
        let backend = WritingBackend {
            file: "parser.rs",
            content: "fn parse() {}\n",
        };

        let summary = run_task_with_backend(&mut fx.store, &config, &fx.state, task, &backend)
            .await
            .unwrap();

        assert_eq!(summary.status, TaskStatus::Review);
        assert_eq!(summary.failure, None);
        assert_eq!(summary.branch, "overseer/task-1");
        assert_eq!(summary.profile, "local");
        assert_eq!(
            summary.usage,
            Usage {
                input_tokens: 10,
                output_tokens: 5
            }
        );
        let sha = summary.commit.clone().expect("a commit was made");
        let head = git_sync(&fx.repo, &["rev-parse", "refs/heads/overseer/task-1"]);
        assert_eq!(sha, head.trim());
        let subject = git_sync(&fx.repo, &["log", "-1", "--format=%s", "overseer/task-1"]);
        assert_eq!(subject.trim(), "overseer: task 1: write parser");

        assert_eq!(fx.store.get_task(task).unwrap().status, TaskStatus::Review);
        let runs = fx.store.list_runs(task).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::Succeeded);
        assert_eq!(runs[0].branch.as_deref(), Some("overseer/task-1"));
        assert_eq!(runs[0].usage, summary.usage);

        let transcript_path = summary.transcript_path.clone().expect("transcript");
        assert_eq!(transcript_path, fx.state.join("runs").join("1.md"));
        let transcript = std::fs::read_to_string(&transcript_path).unwrap();
        assert!(transcript.contains("wrote the file"), "{transcript}");
        assert!(transcript.contains("Task 1: write parser"), "{transcript}");
        assert!(transcript.contains("handle quotes"), "{transcript}");
    }

    #[tokio::test]
    async fn prompt_carries_title_body_and_role_prompt() {
        let mut fx = fixture();
        let config = config(&fx.repo);
        let task = add(&mut fx.store, "write tests", "cover the lexer");
        let backend = MockBackend::answering("ok");

        run_task_with_backend(&mut fx.store, &config, &fx.state, task, &backend)
            .await
            .unwrap();

        let requests = backend.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].system.as_deref(), Some("You are careful."));
        assert_eq!(requests[0].prompt, "Task 1: write tests\n\ncover the lexer");
        assert_eq!(requests[0].permissions, Permissions::WriteWorkdir);
        assert!(requests[0].workdir.ends_with("worktrees/task-1"));
    }

    #[tokio::test]
    async fn no_changes_means_no_commit_but_still_review() {
        let mut fx = fixture();
        let config = config(&fx.repo);
        let task = add(&mut fx.store, "look only", "");
        let backend = MockBackend::answering("nothing to change");

        let summary = run_task_with_backend(&mut fx.store, &config, &fx.state, task, &backend)
            .await
            .unwrap();

        assert_eq!(summary.commit, None);
        assert_eq!(summary.status, TaskStatus::Review);
    }

    #[tokio::test]
    async fn quota_failure_blocks_task_and_records_kind() {
        let mut fx = fixture();
        let config = config(&fx.repo);
        let task = add(&mut fx.store, "big task", "");
        let backend = MockBackend::new(vec![Ok(RunOutcome {
            output: String::new(),
            usage: Usage::default(),
            failure: Some(FailureKind::QuotaExhausted),
        })]);

        let summary = run_task_with_backend(&mut fx.store, &config, &fx.state, task, &backend)
            .await
            .unwrap();

        assert_eq!(summary.status, TaskStatus::Blocked);
        assert_eq!(summary.failure, Some(FailureKind::QuotaExhausted));
        assert_eq!(summary.commit, None);
        assert_eq!(fx.store.get_task(task).unwrap().status, TaskStatus::Blocked);
        let runs = fx.store.list_runs(task).unwrap();
        assert_eq!(runs[0].status, RunStatus::Failed);
        assert_eq!(runs[0].failure.as_deref(), Some("quota-exhausted"));
        assert!(summary.transcript_path.is_some());
    }

    #[tokio::test]
    async fn backend_error_blocks_task_without_leaving_it_running() {
        let mut fx = fixture();
        let config = config(&fx.repo);
        let task = add(&mut fx.store, "broken", "");
        let backend = MockBackend::new(vec![Err("boom".to_owned())]);

        let summary = run_task_with_backend(&mut fx.store, &config, &fx.state, task, &backend)
            .await
            .unwrap();

        assert_eq!(summary.status, TaskStatus::Blocked);
        let Some(FailureKind::Other(detail)) = summary.failure else {
            panic!("expected Other failure");
        };
        assert!(detail.contains("backend error"), "{detail}");
        assert_eq!(fx.store.get_task(task).unwrap().status, TaskStatus::Blocked);
    }

    #[tokio::test]
    async fn claiming_a_task_twice_fails_the_second_time() {
        let mut fx = fixture();
        let config = config(&fx.repo);
        let task = add(&mut fx.store, "once", "");
        let backend = MockBackend::answering("first");
        run_task_with_backend(&mut fx.store, &config, &fx.state, task, &backend)
            .await
            .unwrap();

        let second = MockBackend::answering("second");
        let err = run_task_with_backend(&mut fx.store, &config, &fx.state, task, &second)
            .await
            .unwrap_err();

        assert!(
            matches!(
                err,
                RunnerError::Store {
                    step: "claim task",
                    ..
                }
            ),
            "unexpected error: {err:?}"
        );
        assert!(second.requests().is_empty());
        assert_eq!(fx.store.get_task(task).unwrap().status, TaskStatus::Review);
    }

    #[tokio::test]
    async fn ready_tasks_run_and_use_their_own_profile() {
        let mut fx = fixture();
        let config = config(&fx.repo);
        let task = fx
            .store
            .add_task(NewTask {
                title: "alt task".to_owned(),
                profile: Some("alt".to_owned()),
                ..NewTask::default()
            })
            .unwrap()
            .id;
        fx.store.set_status(task, TaskStatus::Ready).unwrap();
        let backend = MockBackend::answering("ok");

        let summary = run_task_with_backend(&mut fx.store, &config, &fx.state, task, &backend)
            .await
            .unwrap();

        assert_eq!(summary.profile, "alt");
        assert_eq!(fx.store.list_runs(task).unwrap()[0].profile, "alt");
    }

    #[tokio::test]
    async fn unknown_profile_fails_before_the_task_is_claimed() {
        let mut fx = fixture();
        let config = config(&fx.repo);
        let task = fx
            .store
            .add_task(NewTask {
                title: "lost".to_owned(),
                profile: Some("ghost".to_owned()),
                ..NewTask::default()
            })
            .unwrap()
            .id;
        let backend = MockBackend::answering("unused");

        let err = run_task_with_backend(&mut fx.store, &config, &fx.state, task, &backend)
            .await
            .unwrap_err();

        assert!(
            matches!(err, RunnerError::UnknownProfile { ref profile, .. } if profile == "ghost"),
            "unexpected error: {err:?}"
        );
        assert_eq!(fx.store.get_task(task).unwrap().status, TaskStatus::Backlog);
    }
}
