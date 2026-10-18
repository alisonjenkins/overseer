//! Executes one task: claim, worktree, the attempts along its fallback chain,
//! transcripts, commit, and the final task status.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use overseer_backends::{Backend, BackendError, build_backend};
use overseer_core::{
    Config, FailureKind, Profile, RunFinish, RunId, RunOutcome, RunRequest, RunStatus, Task,
    TaskId, TaskStatus, Usage,
};
use overseer_store::{Store, StoreError};

use crate::router::{RouterError, plan_attempts};
use crate::worktree::{self, Worktree};

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("task {task_id} names profile {profile:?}, which is not in overseer.toml")]
    UnknownProfile { task_id: TaskId, profile: String },

    #[error("task {task_id} has no profile left to attempt")]
    Route {
        task_id: TaskId,
        #[source]
        source: RouterError,
    },

    #[error("failed to build the backend for profile {profile:?}")]
    BuildBackend {
        profile: String,
        #[source]
        source: BackendError,
    },

    #[error("no backend was supplied for profile {profile:?}")]
    MissingBackend { profile: String },

    /// The router never returns an empty plan and the first attempt is never
    /// skipped, so this means a broken invariant rather than a bad input.
    #[error("task {task_id} finished without a single attempt")]
    NoAttempt { task_id: TaskId },

    #[error("failed to get changed paths for branch {branch:?}")]
    ChangedPaths {
        branch: String,
        #[source]
        source: crate::worktree::WorktreeError,
    },

    /// One variant for every store call; `step` names the call that failed.
    #[error("store step {step} failed")]
    Store {
        step: &'static str,
        #[source]
        source: StoreError,
    },
}

/// One attempt along a task's fallback chain. `failure` is `None` when the
/// attempt succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptSummary {
    pub run_id: RunId,
    pub profile: String,
    pub failure: Option<FailureKind>,
    pub usage: Usage,
    pub transcript_path: Option<PathBuf>,
}

/// What a finished task execution looked like. `status` is the task status it
/// left behind: `Review` on success, `Blocked` when every attempt failed. The
/// top-level run fields describe the last attempt; `attempts` lists all of
/// them in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSummary {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub profile: String,
    pub status: TaskStatus,
    pub reason: Option<String>,
    pub failure: Option<FailureKind>,
    pub branch: String,
    pub commit: Option<String>,
    pub usage: Usage,
    pub transcript_path: Option<PathBuf>,
    pub attempts: Vec<AttemptSummary>,
}

/// Run a task along its fallback chain, building each backend from its
/// profile.
pub async fn run_task(
    store: &mut Store,
    config: &Config,
    state_dir: &Path,
    task_id: TaskId,
) -> Result<RunSummary, RunnerError> {
    let task = store.get_task(task_id).map_err(store_step("load task"))?;
    let plan = plan_for(store, config, &task)?;
    let built = plan
        .iter()
        .map(|profile| {
            build_backend(profile)
                .map(|backend| (profile.name.as_str(), backend))
                .map_err(|source| RunnerError::BuildBackend {
                    profile: profile.name.clone(),
                    source,
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let backends: HashMap<&str, &dyn Backend> = built
        .iter()
        .map(|(name, backend)| (*name, backend.as_ref()))
        .collect();
    execute(
        store, config, state_dir, task_id, &plan, &backends, None, None,
    )
    .await
}

/// Like `run_task`, but with the backends supplied by the caller, keyed by
/// profile name. Used by tests to substitute `MockBackend`s.
pub async fn run_task_with_backends(
    store: &mut Store,
    config: &Config,
    state_dir: &Path,
    task_id: TaskId,
    backends: &HashMap<&str, &dyn Backend>,
) -> Result<RunSummary, RunnerError> {
    let task = store.get_task(task_id).map_err(store_step("load task"))?;
    let plan = plan_for(store, config, &task)?;
    execute(
        store, config, state_dir, task_id, &plan, backends, None, None,
    )
    .await
}

/// Like `run_task_with_backends`, with one backend answering for every
/// profile in the config.
pub async fn run_task_with_backend(
    store: &mut Store,
    config: &Config,
    state_dir: &Path,
    task_id: TaskId,
    backend: &dyn Backend,
) -> Result<RunSummary, RunnerError> {
    let backends: HashMap<&str, &dyn Backend> = config
        .profiles
        .iter()
        .map(|profile| (profile.name.as_str(), backend))
        .collect();
    run_task_with_backends(store, config, state_dir, task_id, &backends).await
}

/// Continue an existing task worktree and branch with a new prompt and
/// optionally an overridden profile.
pub async fn continue_task(
    store: &mut Store,
    config: &Config,
    state_dir: &Path,
    task_id: TaskId,
    profile_override: Option<&str>,
    prompt: &str,
) -> Result<RunSummary, RunnerError> {
    let task = store.get_task(task_id).map_err(store_step("load task"))?;
    let plan = plan_for_continue(store, config, &task, profile_override)?;
    let built = plan
        .iter()
        .map(|profile| {
            build_backend(profile)
                .map(|backend| (profile.name.as_str(), backend))
                .map_err(|source| RunnerError::BuildBackend {
                    profile: profile.name.clone(),
                    source,
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let backends: HashMap<&str, &dyn Backend> = built
        .iter()
        .map(|(name, backend)| (*name, backend.as_ref()))
        .collect();
    let wt = Worktree {
        path: worktree::worktree_path(state_dir, task_id),
        branch: worktree::branch_name(task_id),
    };
    execute(
        store,
        config,
        state_dir,
        task_id,
        &plan,
        &backends,
        Some(wt),
        Some(prompt),
    )
    .await
}

/// Like `continue_task`, but with the backends supplied by the caller, keyed by
/// profile name.
pub async fn continue_task_with_backends(
    store: &mut Store,
    config: &Config,
    state_dir: &Path,
    task_id: TaskId,
    profile_override: Option<&str>,
    prompt: &str,
    backends: &HashMap<&str, &dyn Backend>,
) -> Result<RunSummary, RunnerError> {
    let task = store.get_task(task_id).map_err(store_step("load task"))?;
    let plan = plan_for_continue(store, config, &task, profile_override)?;
    let wt = Worktree {
        path: worktree::worktree_path(state_dir, task_id),
        branch: worktree::branch_name(task_id),
    };
    execute(
        store,
        config,
        state_dir,
        task_id,
        &plan,
        backends,
        Some(wt),
        Some(prompt),
    )
    .await
}

/// Like `continue_task_with_backends`, with one backend answering for every
/// profile in the config.
pub async fn continue_task_with_backend(
    store: &mut Store,
    config: &Config,
    state_dir: &Path,
    task_id: TaskId,
    profile_override: Option<&str>,
    prompt: &str,
    backend: &dyn Backend,
) -> Result<RunSummary, RunnerError> {
    let backends: HashMap<&str, &dyn Backend> = config
        .profiles
        .iter()
        .map(|profile| (profile.name.as_str(), backend))
        .collect();
    continue_task_with_backends(
        store,
        config,
        state_dir,
        task_id,
        profile_override,
        prompt,
        &backends,
    )
    .await
}

fn plan_for_continue<'a>(
    store: &Store,
    config: &'a Config,
    task: &Task,
    profile_override: Option<&str>,
) -> Result<Vec<&'a Profile>, RunnerError> {
    let name = profile_override
        .or(task.profile.as_deref())
        .unwrap_or(&config.default_profile);
    let start = config
        .profile(name)
        .ok_or_else(|| RunnerError::UnknownProfile {
            task_id: task.id,
            profile: name.to_owned(),
        })?;
    let credits_low = store
        .credits_low()
        .map_err(store_step("read credits_low"))?;
    plan_attempts(config, start, credits_low).map_err(|source| RunnerError::Route {
        task_id: task.id,
        source,
    })
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

/// The profiles to attempt, in order, for `task`, given the current
/// `credits_low` mode.
fn plan_for<'a>(
    store: &Store,
    config: &'a Config,
    task: &Task,
) -> Result<Vec<&'a Profile>, RunnerError> {
    let start = select_profile(config, task)?;
    let credits_low = store
        .credits_low()
        .map_err(store_step("read credits_low"))?;
    plan_attempts(config, start, credits_low).map_err(|source| RunnerError::Route {
        task_id: task.id,
        source,
    })
}

fn store_step(step: &'static str) -> impl FnOnce(StoreError) -> RunnerError {
    move |source| RunnerError::Store { step, source }
}

/// The result of the backend call and the commit step, before the run is
/// written to the store.
pub(crate) struct Attempt {
    pub(crate) output: String,
    pub(crate) usage: Usage,
    pub(crate) failure: Option<FailureKind>,
    pub(crate) commit: Option<String>,
}

impl Attempt {
    pub(crate) fn failed(kind: FailureKind) -> Self {
        Self {
            output: String::new(),
            usage: Usage::default(),
            failure: Some(kind),
            commit: None,
        }
    }
}

/// Walk `plan` in order until an attempt succeeds, fails with `Other`, or the
/// plan runs out. Every attempt is its own run row with its own transcript,
/// all on the task's one worktree and branch.
#[expect(clippy::too_many_arguments)]
async fn execute(
    store: &mut Store,
    config: &Config,
    state_dir: &Path,
    task_id: TaskId,
    plan: &[&Profile],
    backends: &HashMap<&str, &dyn Backend>,
    existing_worktree: Option<Worktree>,
    prompt_override: Option<&str>,
) -> Result<RunSummary, RunnerError> {
    let steps = plan
        .iter()
        .map(|profile| {
            backends
                .get(profile.name.as_str())
                .map(|backend| (*profile, *backend))
                .ok_or_else(|| RunnerError::MissingBackend {
                    profile: profile.name.clone(),
                })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let (task, worktree, branch) = match existing_worktree {
        Some(wt) => {
            let task = store.get_task(task_id).map_err(store_step("load task"))?;
            store
                .set_status(task_id, TaskStatus::Running)
                .map_err(store_step("set task status to running"))?;
            let branch = wt.branch.clone();
            (task, Ok(wt), branch)
        }
        None => {
            let task = store
                .claim_task(task_id)
                .map_err(store_step("claim task"))?;
            let worktree = worktree::create(&config.repo, state_dir, task_id).await;
            let branch = match &worktree {
                Ok(worktree) => worktree.branch.clone(),
                Err(_) => worktree::branch_name(task_id),
            };
            (task, worktree, branch)
        }
    };

    let prompt = prompt_override
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| build_prompt(&task));

    let mut attempts: Vec<AttemptSummary> = Vec::new();
    let mut commit = None;
    let mut credits_exhausted = false;
    for (profile, backend) in steps {
        if credits_exhausted && profile.metered {
            tracing::info!(
                task_id = task_id.get(),
                profile = %profile.name,
                "skipping metered profile after credits ran out during this task"
            );
            continue;
        }
        let (system_prompt, skills_record) = build_system_prompt(state_dir, profile);
        let run_id = store
            .start_run(task_id, &profile.name, skills_record.as_deref())
            .map_err(store_step("start run"))?;

        let mut attempt = match &worktree {
            Ok(worktree) => {
                let ready = if attempts.is_empty() {
                    Ok(())
                } else {
                    worktree::discard_changes(&worktree.path).await
                };
                match ready {
                    Ok(()) => {
                        run_in_worktree(
                            backend,
                            profile,
                            &task,
                            worktree,
                            &prompt,
                            system_prompt.clone(),
                        )
                        .await
                    }
                    Err(err) => Attempt::failed(FailureKind::Other(format!("discard: {err}"))),
                }
            }
            Err(err) => Attempt::failed(FailureKind::Other(format!("worktree: {err}"))),
        };

        let transcript_path = match write_transcript(
            state_dir,
            run_id,
            &task,
            profile,
            &branch,
            &prompt,
            &attempt,
            system_prompt.as_deref(),
        )
        .await
        {
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

        if attempt.failure.is_some()
            && profile.metered
            && attempt.failure == Some(FailureKind::QuotaExhausted)
        {
            store
                .set_credits_low(true)
                .map_err(store_step("set credits_low"))?;
            credits_exhausted = true;
            tracing::warn!(
                task_id = task_id.get(),
                profile = %profile.name,
                "metered profile is out of credits; credits_low is now on"
            );
        }

        let failure = attempt.failure.clone();
        let succeeded = failure.is_none();
        if succeeded {
            commit = attempt.commit.clone();
        }
        attempts.push(AttemptSummary {
            run_id,
            profile: profile.name.clone(),
            failure: failure.clone(),
            usage: attempt.usage,
            transcript_path,
        });
        match failure {
            None => break,
            Some(kind) if !kind.is_routable() => break,
            Some(_) => {}
        }
    }

    let Some(last) = attempts.last().cloned() else {
        return Err(RunnerError::NoAttempt { task_id });
    };
    let (task_status, reason) = if last.failure.is_none() {
        let changed = worktree::changed_paths(&config.repo, &branch)
            .await
            .map_err(|source| RunnerError::ChangedPaths {
                branch: branch.clone(),
                source,
            })?;
        let hits = crate::escalation::escalation_hits(&changed, &config.escalation);
        if hits.is_empty() {
            (TaskStatus::Review, None)
        } else {
            (
                TaskStatus::NeedsHuman,
                Some(format!("escalation: {}", hits.join(", "))),
            )
        }
    } else {
        (
            TaskStatus::Blocked,
            last.failure.as_ref().map(ToString::to_string),
        )
    };
    store
        .set_status_with_reason(task_id, task_status, reason.as_deref())
        .map_err(store_step("set task status"))?;

    Ok(RunSummary {
        run_id: last.run_id,
        task_id,
        profile: last.profile,
        status: task_status,
        reason,
        failure: last.failure,
        branch,
        commit,
        usage: last.usage,
        transcript_path: last.transcript_path,
        attempts,
    })
}

async fn run_in_worktree(
    backend: &dyn Backend,
    profile: &Profile,
    task: &Task,
    worktree: &Worktree,
    prompt: &str,
    system: Option<String>,
) -> Attempt {
    let request = RunRequest {
        system,
        prompt: prompt.to_owned(),
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

/// Build the effective system prompt for a profile by appending its configured
/// skills, and format the active skill versions for the run row.
pub fn build_system_prompt(
    state_dir: &Path,
    profile: &Profile,
) -> (Option<String>, Option<String>) {
    let skills_dir = state_dir.join("skills");
    let mut system = profile.role_prompt.clone();
    let mut active_versions = Vec::new();

    for skill_name in &profile.skills {
        match overseer_store::skills::read_current_skill(&skills_dir, skill_name) {
            Ok(Some((version, content))) => {
                active_versions.push(format!("{skill_name}:v{version}"));
                match &mut system {
                    Some(prev) => {
                        prev.push_str(&format!("\n\n## Skill: {skill_name}\n{content}"));
                    }
                    None => {
                        system = Some(format!("## Skill: {skill_name}\n{content}"));
                    }
                }
            }
            Ok(None) => {
                tracing::warn!(skill = %skill_name, "skill not found on disk");
            }
            Err(err) => {
                tracing::warn!(skill = %skill_name, %err, "failed to read skill");
            }
        }
    }

    let skills_record = if active_versions.is_empty() {
        None
    } else {
        Some(active_versions.join(","))
    };

    (system, skills_record)
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
#[allow(clippy::too_many_arguments)]
pub(crate) async fn write_transcript(
    state_dir: &Path,
    run_id: RunId,
    task: &Task,
    profile: &Profile,
    branch: &str,
    prompt: &str,
    attempt: &Attempt,
    system: Option<&str>,
) -> Result<PathBuf, std::io::Error> {
    let dir = state_dir.join("runs");
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{run_id}.md"));
    let tmp = dir.join(format!("{run_id}.md.tmp"));
    let body = render_transcript(run_id, task, profile, branch, prompt, attempt, system);
    tokio::fs::write(&tmp, body).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(path)
}

fn render_transcript(
    run_id: RunId,
    task: &Task,
    profile: &Profile,
    branch: &str,
    prompt: &str,
    attempt: &Attempt,
    system: Option<&str>,
) -> String {
    let failure = attempt
        .failure
        .as_ref()
        .map_or_else(|| "none".to_owned(), ToString::to_string);
    let commit = attempt.commit.as_deref().unwrap_or("none");
    let system = system.unwrap_or("(none)");
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
        prompt = prompt,
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
            prompt_flag: None,
            prompt_stdin: false,
            role_prompt: Some("You are careful.".to_owned()),
            fallback: Vec::new(),
            permissions: Permissions::WriteWorkdir,
            timeout_secs: 5,
            metered: false,
            permission_args: Default::default(),
            env: Default::default(),
            agent_tools: false,
            max_steps: 40,
            skills: Vec::new(),
        }
    }

    fn config(repo: &Path) -> Config {
        Config {
            repo: repo.to_path_buf(),
            default_profile: "local".to_owned(),
            review: overseer_core::ReviewConfig::default(),
            escalation: Default::default(),
            profiles: vec![profile("local"), profile("alt")],
            learning: Default::default(),
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
    async fn continue_task_modifies_existing_worktree() {
        let mut fx = fixture();
        let config = config(&fx.repo);
        let task = add(&mut fx.store, "feature", "build it");
        let backend1 = WritingBackend {
            file: "out.txt",
            content: "version 1\n",
        };

        let summary1 = run_task_with_backend(&mut fx.store, &config, &fx.state, task, &backend1)
            .await
            .unwrap();
        assert_eq!(summary1.status, TaskStatus::Review);

        let backend2 = WritingBackend {
            file: "out.txt",
            content: "version 2\n",
        };
        let summary2 = continue_task_with_backend(
            &mut fx.store,
            &config,
            &fx.state,
            task,
            None,
            "Rework: update version",
            &backend2,
        )
        .await
        .unwrap();

        assert_eq!(summary2.status, TaskStatus::Review);
        let head = git_sync(&fx.repo, &["rev-parse", "refs/heads/overseer/task-1"]);
        assert_eq!(summary2.commit.unwrap(), head.trim());

        // Check file content in the worktree
        let wt_file = fx.state.join("worktrees/task-1/out.txt");
        assert_eq!(std::fs::read_to_string(wt_file).unwrap(), "version 2\n");

        let runs = fx.store.list_runs(task).unwrap();
        assert_eq!(runs.len(), 2);
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
    async fn prompt_carries_injected_skills_and_records_on_run_row() {
        let mut fx = fixture();
        overseer_store::skills::create_next_version(
            &fx.state.join("skills"),
            "rust-guidelines",
            "Prefer iterators over loops.",
        )
        .unwrap();

        let mut cfg = config(&fx.repo);
        cfg.profiles[0].skills = vec!["rust-guidelines".to_owned(), "nonexistent-skill".to_owned()];
        let task = add(&mut fx.store, "write tests", "cover the lexer");
        let backend = MockBackend::answering("ok");

        let _summary = run_task_with_backend(&mut fx.store, &cfg, &fx.state, task, &backend)
            .await
            .unwrap();

        let requests = backend.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].system.as_deref(),
            Some("You are careful.\n\n## Skill: rust-guidelines\nPrefer iterators over loops.")
        );

        let runs = fx.store.list_runs(task).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].skills.as_deref(), Some("rust-guidelines:v1"));
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

    fn with_fallback(mut profile: Profile, fallback: &[&str]) -> Profile {
        profile.fallback = fallback.iter().map(|name| (*name).to_owned()).collect();
        profile
    }

    fn metered(mut profile: Profile) -> Profile {
        profile.metered = true;
        profile
    }

    fn chain_config(repo: &Path, profiles: Vec<Profile>) -> Config {
        Config {
            repo: repo.to_path_buf(),
            default_profile: profiles[0].name.clone(),
            review: overseer_core::ReviewConfig::default(),
            escalation: Default::default(),
            profiles,
            learning: Default::default(),
        }
    }

    fn failing(kind: FailureKind) -> Result<RunOutcome, String> {
        Ok(RunOutcome {
            output: String::new(),
            usage: Usage::default(),
            failure: Some(kind),
        })
    }

    fn attempt_log(summary: &RunSummary) -> Vec<(String, Option<FailureKind>)> {
        summary
            .attempts
            .iter()
            .map(|attempt| (attempt.profile.clone(), attempt.failure.clone()))
            .collect()
    }

    #[tokio::test]
    async fn unreachable_local_falls_back_to_gemini() {
        let mut fx = fixture();
        let config = chain_config(
            &fx.repo,
            vec![
                with_fallback(profile("local"), &["gemini"]),
                profile("gemini"),
            ],
        );
        let task = add(&mut fx.store, "chain", "");
        let local = MockBackend::new(vec![failing(FailureKind::Unreachable)]);
        let gemini = MockBackend::answering("done");
        let backends: HashMap<&str, &dyn Backend> =
            HashMap::from([("local", &local as &dyn Backend), ("gemini", &gemini)]);

        let summary = run_task_with_backends(&mut fx.store, &config, &fx.state, task, &backends)
            .await
            .unwrap();

        assert_eq!(summary.status, TaskStatus::Review);
        assert_eq!(summary.profile, "gemini");
        assert_eq!(
            attempt_log(&summary),
            vec![
                ("local".to_owned(), Some(FailureKind::Unreachable)),
                ("gemini".to_owned(), None),
            ]
        );
        let runs = fx.store.list_runs(task).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].status, RunStatus::Failed);
        assert_eq!(runs[1].status, RunStatus::Succeeded);
        assert_ne!(runs[0].transcript_path, runs[1].transcript_path);
        assert!(fx.state.join("runs").join("1.md").exists());
        assert!(fx.state.join("runs").join("2.md").exists());
    }

    #[tokio::test]
    async fn metered_quota_failure_turns_credits_low_on_and_moves_on() {
        let mut fx = fixture();
        let config = chain_config(
            &fx.repo,
            vec![
                metered(with_fallback(profile("claude"), &["local"])),
                profile("local"),
            ],
        );
        let task = add(&mut fx.store, "metered", "");
        let claude = MockBackend::new(vec![failing(FailureKind::QuotaExhausted)]);
        let local = MockBackend::answering("done");
        let backends: HashMap<&str, &dyn Backend> =
            HashMap::from([("claude", &claude as &dyn Backend), ("local", &local)]);

        let summary = run_task_with_backends(&mut fx.store, &config, &fx.state, task, &backends)
            .await
            .unwrap();

        assert_eq!(summary.status, TaskStatus::Review);
        assert_eq!(
            attempt_log(&summary),
            vec![
                ("claude".to_owned(), Some(FailureKind::QuotaExhausted)),
                ("local".to_owned(), None),
            ]
        );
        assert!(fx.store.credits_low().unwrap());
    }

    #[tokio::test]
    async fn other_failure_stops_the_chain_and_blocks_the_task() {
        let mut fx = fixture();
        let config = chain_config(
            &fx.repo,
            vec![
                with_fallback(profile("local"), &["gemini"]),
                profile("gemini"),
            ],
        );
        let task = add(&mut fx.store, "broken", "");
        let local = MockBackend::new(vec![failing(FailureKind::Other("exit 2".to_owned()))]);
        let gemini = MockBackend::answering("unused");
        let backends: HashMap<&str, &dyn Backend> =
            HashMap::from([("local", &local as &dyn Backend), ("gemini", &gemini)]);

        let summary = run_task_with_backends(&mut fx.store, &config, &fx.state, task, &backends)
            .await
            .unwrap();

        assert_eq!(summary.status, TaskStatus::Blocked);
        assert_eq!(
            attempt_log(&summary),
            vec![(
                "local".to_owned(),
                Some(FailureKind::Other("exit 2".to_owned()))
            )]
        );
        assert!(gemini.requests().is_empty());
        assert_eq!(fx.store.get_task(task).unwrap().status, TaskStatus::Blocked);
    }

    #[tokio::test]
    async fn credits_low_skips_metered_profiles() {
        let mut fx = fixture();
        fx.store.set_credits_low(true).unwrap();
        let config = chain_config(
            &fx.repo,
            vec![
                metered(with_fallback(profile("claude"), &["local"])),
                profile("local"),
            ],
        );
        let task = add(&mut fx.store, "cheap", "");
        let claude = MockBackend::answering("should not run");
        let local = MockBackend::answering("done");
        let backends: HashMap<&str, &dyn Backend> =
            HashMap::from([("claude", &claude as &dyn Backend), ("local", &local)]);

        let summary = run_task_with_backends(&mut fx.store, &config, &fx.state, task, &backends)
            .await
            .unwrap();

        assert!(claude.requests().is_empty());
        assert_eq!(attempt_log(&summary), vec![("local".to_owned(), None)]);
        assert_eq!(summary.status, TaskStatus::Review);
    }

    #[tokio::test]
    async fn every_attempt_failing_blocks_with_the_last_failure() {
        let mut fx = fixture();
        let config = chain_config(
            &fx.repo,
            vec![
                with_fallback(profile("local"), &["gemini"]),
                profile("gemini"),
            ],
        );
        let task = add(&mut fx.store, "hopeless", "");
        let local = MockBackend::new(vec![failing(FailureKind::Unreachable)]);
        let gemini = MockBackend::new(vec![failing(FailureKind::AuthRequired)]);
        let backends: HashMap<&str, &dyn Backend> =
            HashMap::from([("local", &local as &dyn Backend), ("gemini", &gemini)]);

        let summary = run_task_with_backends(&mut fx.store, &config, &fx.state, task, &backends)
            .await
            .unwrap();

        assert_eq!(summary.status, TaskStatus::Blocked);
        assert_eq!(summary.failure, Some(FailureKind::AuthRequired));
        assert_eq!(summary.attempts.len(), 2);
        assert_eq!(fx.store.get_task(task).unwrap().status, TaskStatus::Blocked);
        let statuses: Vec<RunStatus> = fx
            .store
            .list_runs(task)
            .unwrap()
            .iter()
            .map(|run| run.status)
            .collect();
        assert_eq!(statuses, vec![RunStatus::Failed, RunStatus::Failed]);
    }

    /// Edits a tracked file and leaves an untracked one, then fails.
    struct DirtyThenFail;

    #[async_trait]
    impl Backend for DirtyThenFail {
        async fn run(&self, req: &RunRequest) -> Result<RunOutcome, BackendError> {
            std::fs::write(req.workdir.join("junk.txt"), "half done\n").unwrap();
            std::fs::write(req.workdir.join("README.md"), "half edited\n").unwrap();
            Ok(RunOutcome {
                output: "partial".to_owned(),
                usage: Usage::default(),
                failure: Some(FailureKind::Unreachable),
            })
        }
    }

    /// Records whether the worktree still holds the previous attempt's edits.
    struct CheckForLeftovers {
        leftovers: std::sync::Mutex<Option<bool>>,
    }

    #[async_trait]
    impl Backend for CheckForLeftovers {
        async fn run(&self, req: &RunRequest) -> Result<RunOutcome, BackendError> {
            let readme = std::fs::read_to_string(req.workdir.join("README.md")).unwrap();
            let dirty = req.workdir.join("junk.txt").exists() || readme != "base\n";
            *self.leftovers.lock().unwrap() = Some(dirty);
            Ok(RunOutcome {
                output: "clean start".to_owned(),
                usage: Usage::default(),
                failure: None,
            })
        }
    }

    #[tokio::test]
    async fn changes_from_a_failed_attempt_are_discarded_before_the_next() {
        let mut fx = fixture();
        let config = chain_config(
            &fx.repo,
            vec![
                with_fallback(profile("local"), &["gemini"]),
                profile("gemini"),
            ],
        );
        let task = add(&mut fx.store, "retry", "");
        let gemini = CheckForLeftovers {
            leftovers: std::sync::Mutex::new(None),
        };
        let backends: HashMap<&str, &dyn Backend> = HashMap::from([
            ("local", &DirtyThenFail as &dyn Backend),
            ("gemini", &gemini),
        ]);

        let summary = run_task_with_backends(&mut fx.store, &config, &fx.state, task, &backends)
            .await
            .unwrap();

        assert_eq!(summary.status, TaskStatus::Review);
        assert_eq!(*gemini.leftovers.lock().unwrap(), Some(false));
    }

    #[tokio::test]
    async fn successful_run_touching_sensitive_paths_escalates_to_needs_human() {
        let mut fx = fixture();
        let config = config(&fx.repo);
        let task = add(&mut fx.store, "touch Cargo.toml", "modify dependency");
        let backend = WritingBackend {
            file: "Cargo.toml",
            content: "# sensitive\n",
        };

        let summary = run_task_with_backend(&mut fx.store, &config, &fx.state, task, &backend)
            .await
            .unwrap();

        assert_eq!(summary.status, TaskStatus::NeedsHuman);
        assert_eq!(summary.reason.as_deref(), Some("escalation: Cargo.toml"));
        let stored = fx.store.get_task(task).unwrap();
        assert_eq!(stored.status, TaskStatus::NeedsHuman);
        assert_eq!(stored.reason.as_deref(), Some("escalation: Cargo.toml"));
    }

    #[tokio::test]
    async fn blocked_run_records_failure_reason() {
        let mut fx = fixture();
        let config = config(&fx.repo);
        let task = add(&mut fx.store, "failing task", "");
        let backend = MockBackend::new(vec![Ok(RunOutcome {
            output: String::new(),
            usage: Usage::default(),
            failure: Some(FailureKind::Other("disk full".to_owned())),
        })]);

        let summary = run_task_with_backend(&mut fx.store, &config, &fx.state, task, &backend)
            .await
            .unwrap();

        assert_eq!(summary.status, TaskStatus::Blocked);
        assert_eq!(summary.reason.as_deref(), Some("other: disk full"));
        let stored = fx.store.get_task(task).unwrap();
        assert_eq!(stored.status, TaskStatus::Blocked);
        assert_eq!(stored.reason.as_deref(), Some("other: disk full"));
    }

    #[tokio::test]
    async fn continue_task_touching_sensitive_paths_escalates_to_needs_human() {
        let mut fx = fixture();
        let config = config(&fx.repo);
        let task = add(&mut fx.store, "feature", "build it");
        let backend1 = WritingBackend {
            file: "out.txt",
            content: "version 1\n",
        };

        let summary1 = run_task_with_backend(&mut fx.store, &config, &fx.state, task, &backend1)
            .await
            .unwrap();
        assert_eq!(summary1.status, TaskStatus::Review);

        let backend2 = WritingBackend {
            file: "Cargo.toml",
            content: "# sensitive\n",
        };
        let summary2 = continue_task_with_backend(
            &mut fx.store,
            &config,
            &fx.state,
            task,
            None,
            "Rework: touch sensitive file",
            &backend2,
        )
        .await
        .unwrap();

        assert_eq!(summary2.status, TaskStatus::NeedsHuman);
        assert_eq!(summary2.reason.as_deref(), Some("escalation: Cargo.toml"));
        let stored = fx.store.get_task(task).unwrap();
        assert_eq!(stored.status, TaskStatus::NeedsHuman);
        assert_eq!(stored.reason.as_deref(), Some("escalation: Cargo.toml"));
    }
}
