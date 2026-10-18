use std::collections::HashMap;
use std::path::{Path, PathBuf};

use overseer_backends::{Backend, BackendError};
use overseer_core::{
    Config, Lesson, NewLesson, Permissions, Profile, Run, RunRequest, Task, TaskId, TaskStatus,
};
use overseer_store::StoreError;
use tokio::process::Command;

use crate::router::{RouterError, plan_attempts};

const MAX_DIFF_BYTES: usize = 40 * 1024; // 40 KB

#[derive(Debug, thiserror::Error)]
pub enum RetrospectError {
    #[error("store query failed: {0}")]
    Store(#[from] StoreError),

    #[error("profile {profile} is not defined in overseer.toml")]
    MissingProfile { profile: String },

    #[error("failed to build backend for profile {profile}: {source}")]
    BackendBuild {
        profile: String,
        #[source]
        source: BackendError,
    },

    #[error("failed to route attempts for profile {profile}: {source}")]
    Route {
        profile: String,
        #[source]
        source: RouterError,
    },

    #[error("backend for profile {profile} not found")]
    MissingBackend { profile: String },

    #[error("backend error on profile {profile}: {source}")]
    Backend {
        profile: String,
        #[source]
        source: BackendError,
    },

    #[error("all learning attempts failed for task {task_id}")]
    AllAttemptsFailed { task_id: TaskId },

    #[error("I/O error while writing lesson files: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to run git diff: {0}")]
    GitDiff(String),
}

/// Checks whether a task produced enough signal to warrant a retrospective.
/// Clean first-pass PASS tasks (round 0, single run, no failures, status Review or Done)
/// teach nothing.
pub fn has_learning_signal(task: &Task, runs: &[Run]) -> bool {
    if task.status == TaskStatus::Blocked || task.status == TaskStatus::NeedsHuman {
        return true;
    }
    task.round > 0 || runs.len() > 1 || runs.iter().any(|r| r.failure.is_some())
}

/// Run a retrospective for `task_id`, proposing skill improvements if any.
pub async fn retrospect_task(
    store: &mut overseer_store::Store,
    config: &Config,
    state_dir: &Path,
    task_id: TaskId,
) -> Result<Option<Lesson>, RetrospectError> {
    let built = config
        .profiles
        .iter()
        .map(|profile| {
            overseer_backends::build_backend(profile)
                .map(|backend| (profile.name.as_str(), backend))
                .map_err(|source| RetrospectError::BackendBuild {
                    profile: profile.name.clone(),
                    source,
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let backends: HashMap<&str, &dyn Backend> = built
        .iter()
        .map(|(name, backend)| (*name, backend.as_ref()))
        .collect();
    retrospect_task_with_backends(store, config, state_dir, task_id, &backends).await
}

/// Like `retrospect_task`, but with a single backend answering for every profile.
pub async fn retrospect_task_with_backend(
    store: &mut overseer_store::Store,
    config: &Config,
    state_dir: &Path,
    task_id: TaskId,
    backend: &dyn Backend,
) -> Result<Option<Lesson>, RetrospectError> {
    let backends: HashMap<&str, &dyn Backend> = config
        .profiles
        .iter()
        .map(|profile| (profile.name.as_str(), backend))
        .collect();
    retrospect_task_with_backends(store, config, state_dir, task_id, &backends).await
}

/// Run a retrospective with backends provided by the caller.
pub async fn retrospect_task_with_backends(
    store: &mut overseer_store::Store,
    config: &Config,
    state_dir: &Path,
    task_id: TaskId,
    backends: &HashMap<&str, &dyn Backend>,
) -> Result<Option<Lesson>, RetrospectError> {
    let learning_profile = match &config.learning.profile {
        Some(name) => name,
        None => return Ok(None),
    };

    let start_profile =
        config
            .profile(learning_profile)
            .ok_or_else(|| RetrospectError::MissingProfile {
                profile: learning_profile.clone(),
            })?;

    let task = store.get_task(task_id)?;
    let runs = store.list_runs(task_id)?;

    let target_skill = config.learning.skill.as_deref().unwrap_or("general");
    let skills_dir = state_dir.join("skills");
    let (base_version, skill_content) =
        match overseer_store::skills::read_current_skill(&skills_dir, target_skill)? {
            Some((v, content)) => (v, content),
            None => (0, "(new skill, currently empty)".to_owned()),
        };

    let branch = crate::worktree::branch_name(task_id);
    let full_diff = crate::worktree::diff(&config.repo, &branch)
        .await
        .unwrap_or_default();
    let truncated_diff = truncate_diff(&full_diff, MAX_DIFF_BYTES);

    let failures: Vec<String> = runs.iter().filter_map(|r| r.failure.clone()).collect();

    let prompt = build_retrospect_prompt(
        &task,
        truncated_diff,
        &failures,
        target_skill,
        &skill_content,
    );

    let credits_low = store.credits_low().unwrap_or(false);
    let plan = plan_attempts(config, start_profile, credits_low).map_err(|source| {
        RetrospectError::Route {
            profile: learning_profile.clone(),
            source,
        }
    })?;

    let output = execute_learning_prompt(config, &plan, backends, &prompt, task_id).await?;

    let trimmed = output.trim();
    if trimmed == "NO CHANGE" {
        return Ok(None);
    }

    let revised_markdown = unwrap_code_fences(trimmed);

    let lessons_dir = state_dir.join("lessons");
    tokio::fs::create_dir_all(&lessons_dir).await?;

    let new_lesson = NewLesson {
        task_id,
        skill: target_skill.to_owned(),
        base_version,
        path: PathBuf::new(),
    };
    let mut lesson = store.add_lesson(new_lesson)?;

    let lesson_id = lesson.id;
    let md_path = lessons_dir.join(format!("{lesson_id}.md"));
    let diff_path = lessons_dir.join(format!("{lesson_id}.diff"));

    let file_content = if revised_markdown.ends_with('\n') {
        revised_markdown.to_owned()
    } else {
        format!("{revised_markdown}\n")
    };
    tokio::fs::write(&md_path, &file_content).await?;

    store.update_lesson_path(lesson_id, &md_path)?;
    lesson.path = md_path.clone();

    let old_path = if base_version > 0 {
        skills_dir
            .join(target_skill)
            .join(format!("v{base_version}.md"))
    } else {
        PathBuf::from("/dev/null")
    };

    let diff_output = compute_unified_diff(&old_path, &md_path).await?;
    tokio::fs::write(&diff_path, diff_output).await?;

    Ok(Some(lesson))
}

async fn execute_learning_prompt(
    config: &Config,
    plan: &[&Profile],
    backends: &HashMap<&str, &dyn Backend>,
    prompt: &str,
    task_id: TaskId,
) -> Result<String, RetrospectError> {
    for profile in plan {
        let backend = match backends.get(profile.name.as_str()) {
            Some(b) => *b,
            None => {
                return Err(RetrospectError::MissingBackend {
                    profile: profile.name.clone(),
                });
            }
        };

        let request = RunRequest {
            system: profile.role_prompt.clone(),
            prompt: prompt.to_owned(),
            workdir: config.repo.clone(),
            permissions: Permissions::Read,
        };

        match backend.run(&request).await {
            Ok(outcome) => {
                if let Some(err) = outcome.failure {
                    tracing::warn!(profile = %profile.name, %err, "learning profile attempt failed");
                    continue;
                }
                return Ok(outcome.output);
            }
            Err(err) => {
                tracing::warn!(profile = %profile.name, %err, "learning backend call error");
                continue;
            }
        }
    }

    Err(RetrospectError::AllAttemptsFailed { task_id })
}

fn build_retrospect_prompt(
    task: &Task,
    diff: &str,
    failures: &[String],
    skill_name: &str,
    skill_content: &str,
) -> String {
    let failure_section = if failures.is_empty() {
        "None".to_owned()
    } else {
        failures
            .iter()
            .map(|f| format!("- {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let diff_section = if diff.trim().is_empty() {
        "(no changes in task branch)".to_owned()
    } else {
        diff.to_owned()
    };

    format!(
        "# Retrospective Analysis for Task {task_id}\n\n\
         ## Task\n\
         - Title: {title}\n\
         - Body: {body}\n\n\
         ## Failures Encountered\n\
         {failure_section}\n\n\
         ## Final Diff\n\
         ```diff\n\
         {diff_section}\n\
         ```\n\n\
         ## Target Skill: {skill_name}\n\
         Current version content:\n\
         ```markdown\n\
         {skill_content}\n\
         ```\n\n\
         ## Instructions\n\
         Review this task outcome against the target skill. Reply EITHER with exactly `NO CHANGE` \
         if the current skill was adequate or no general rule can be drawn, OR with the complete \
         revised skill markdown.\n\n\
         Rules for revised skill markdown:\n\
         - Short, imperative, general rules.\n\
         - At most 60 lines.\n\
         - Never include task-specific details, names, or file paths.\n\
         - No secrets, credentials, or private information.\n\
         - Output ONLY the markdown text or exactly `NO CHANGE`.",
        task_id = task.id,
        title = task.title,
        body = task.body,
    )
}

fn truncate_diff(diff: &str, max_bytes: usize) -> &str {
    if diff.len() <= max_bytes {
        diff
    } else {
        let mut end = max_bytes;
        while end > 0 && !diff.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        diff.get(..end).unwrap_or("")
    }
}

fn unwrap_code_fences(text: &str) -> &str {
    if let Some(stripped) = text.strip_prefix("```markdown\n") {
        stripped.strip_suffix("\n```").unwrap_or(stripped)
    } else if let Some(stripped) = text.strip_prefix("```\n") {
        stripped.strip_suffix("\n```").unwrap_or(stripped)
    } else {
        text
    }
}

async fn compute_unified_diff(old_path: &Path, new_path: &Path) -> Result<String, RetrospectError> {
    let output = Command::new("git")
        .args([
            "--no-pager",
            "diff",
            "--no-ext-diff",
            "--no-index",
            &old_path.to_string_lossy(),
            &new_path.to_string_lossy(),
        ])
        .output()
        .await
        .map_err(RetrospectError::Io)?;

    let code = output.status.code().unwrap_or(2);
    if code > 1 {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(RetrospectError::GitDiff(stderr.trim().to_owned()));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use overseer_backends::MockBackend;
    use overseer_core::{
        BackendKind, LearningConfig, LessonStatus, NewTask, Permissions, RunFinish, RunStatus,
        Usage,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::test_support::init_repo;

    struct Fixture {
        _root: TempDir,
        repo: PathBuf,
        state: PathBuf,
        store: overseer_store::Store,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let state = root.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let store = overseer_store::Store::open(&state.join("overseer.db")).unwrap();
        Fixture {
            _root: root,
            repo,
            state,
            store,
        }
    }

    fn config(repo: &Path) -> Config {
        Config {
            repo: repo.to_path_buf(),
            default_profile: "agent".to_owned(),
            profiles: vec![Profile {
                name: "agent".to_owned(),
                backend: BackendKind::Command,
                model: None,
                base_url: None,
                api_key_env: None,
                command: None,
                prompt_flag: None,
                prompt_stdin: false,
                role_prompt: None,
                fallback: Vec::new(),
                permissions: Permissions::WriteWorkdir,
                timeout_secs: 10,
                metered: false,
                permission_args: Default::default(),
                env: Default::default(),
                agent_tools: false,
                max_steps: 40,
                skills: Vec::new(),
            }],
            learning: LearningConfig {
                profile: Some("agent".to_owned()),
                skill: Some("general".to_owned()),
                min_signal: true,
            },
            review: Default::default(),
            escalation: Default::default(),
            landing: Default::default(),
        }
    }

    #[test]
    fn signal_detected_for_blocked_and_reworked_tasks_only() {
        let mut fx = fixture();
        let task = fx
            .store
            .add_task(NewTask {
                title: "task".to_owned(),
                ready: true,
                ..NewTask::default()
            })
            .unwrap();

        // Single run, no failure, status ready -> not clean pass yet, but not blocked
        let runs = vec![];
        assert!(!has_learning_signal(&task, &runs));

        // Blocked task
        fx.store.set_status(task.id, TaskStatus::Blocked).unwrap();
        let blocked = fx.store.get_task(task.id).unwrap();
        assert!(has_learning_signal(&blocked, &runs));

        // Needs human task
        fx.store
            .set_status(task.id, TaskStatus::NeedsHuman)
            .unwrap();
        let human = fx.store.get_task(task.id).unwrap();
        assert!(has_learning_signal(&human, &runs));

        // Review task with single clean run
        fx.store.set_status(task.id, TaskStatus::Review).unwrap();
        let review = fx.store.get_task(task.id).unwrap();
        let run_id = fx.store.start_run(task.id, "agent", None).unwrap();
        fx.store
            .finish_run(
                run_id,
                RunFinish {
                    status: RunStatus::Succeeded,
                    usage: Usage::default(),
                    failure: None,
                    transcript_path: None,
                    branch: None,
                },
            )
            .unwrap();
        let runs = fx.store.list_runs(task.id).unwrap();
        assert!(!has_learning_signal(&review, &runs));

        // Review task with a failed run in history
        let fail_id = fx.store.start_run(task.id, "agent", None).unwrap();
        fx.store
            .finish_run(
                fail_id,
                RunFinish {
                    status: RunStatus::Failed,
                    usage: Usage::default(),
                    failure: Some("failed".to_owned()),
                    transcript_path: None,
                    branch: None,
                },
            )
            .unwrap();
        let runs = fx.store.list_runs(task.id).unwrap();
        assert!(has_learning_signal(&review, &runs));
    }

    #[tokio::test]
    async fn retrospect_no_change_creates_no_lesson() {
        let mut fx = fixture();
        let cfg = config(&fx.repo);
        let task = fx
            .store
            .add_task(NewTask {
                title: "failing task".to_owned(),
                ready: true,
                ..NewTask::default()
            })
            .unwrap();
        fx.store.set_status(task.id, TaskStatus::Blocked).unwrap();

        let backend = MockBackend::answering("NO CHANGE");
        let result =
            retrospect_task_with_backend(&mut fx.store, &cfg, &fx.state, task.id, &backend)
                .await
                .unwrap();

        assert!(result.is_none());
        assert!(fx.store.list_lessons(None).unwrap().is_empty());
    }

    #[tokio::test]
    async fn retrospect_proposes_lesson_and_writes_files_and_diff() {
        let mut fx = fixture();
        let cfg = config(&fx.repo);

        // Pre-create general v1 skill
        overseer_store::skills::create_next_version(
            &fx.state.join("skills"),
            "general",
            "Initial general guideline.",
        )
        .unwrap();

        let task = fx
            .store
            .add_task(NewTask {
                title: "failing task".to_owned(),
                ready: true,
                ..NewTask::default()
            })
            .unwrap();
        fx.store.set_status(task.id, TaskStatus::Blocked).unwrap();

        let backend =
            MockBackend::answering("Initial general guideline.\nAlways write tests first.");
        let lesson =
            retrospect_task_with_backend(&mut fx.store, &cfg, &fx.state, task.id, &backend)
                .await
                .unwrap()
                .expect("lesson proposed");

        assert_eq!(lesson.skill, "general");
        assert_eq!(lesson.base_version, 1);
        assert_eq!(lesson.status, LessonStatus::Proposed);

        let md_file = fx.state.join("lessons").join(format!("{}.md", lesson.id));
        let diff_file = fx.state.join("lessons").join(format!("{}.diff", lesson.id));

        assert!(md_file.is_file());
        assert!(diff_file.is_file());

        let md_content = std::fs::read_to_string(&md_file).unwrap();
        assert_eq!(
            md_content,
            "Initial general guideline.\nAlways write tests first.\n"
        );

        let diff_content = std::fs::read_to_string(&diff_file).unwrap();
        assert!(
            diff_content.contains("+Always write tests first."),
            "{diff_content}"
        );
    }

    #[tokio::test]
    async fn retrospect_disabled_when_profile_none() {
        let mut fx = fixture();
        let mut cfg = config(&fx.repo);
        cfg.learning.profile = None;

        let task = fx
            .store
            .add_task(NewTask {
                title: "failing task".to_owned(),
                ready: true,
                ..NewTask::default()
            })
            .unwrap();

        let backend = MockBackend::answering("proposal");
        let result =
            retrospect_task_with_backend(&mut fx.store, &cfg, &fx.state, task.id, &backend)
                .await
                .unwrap();

        assert!(result.is_none());
    }
}
