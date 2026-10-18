//! Worker loop: continuously picks eligible ready tasks and runs them one by one.

use std::path::Path;
use std::time::Duration;

use overseer_core::{Config, TaskStatus};
use overseer_store::{Store, StoreError};
use tokio::sync::watch;

use crate::lock::{WorkerLock, WorkerLockError};
use crate::recovery::{RecoveryError, recover_interrupted};
use crate::runner::{RunnerError, run_task};

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error(transparent)]
    Lock(#[from] WorkerLockError),

    #[error("failed during crash recovery")]
    Recovery(#[from] RecoveryError),

    #[error("store query failed")]
    Store(#[source] StoreError),

    #[error("task execution failed")]
    Runner(#[from] RunnerError),

    #[error("task review failed")]
    Review(#[from] crate::review::ReviewError),
}

/// Options controlling the worker loop behavior.
#[derive(Debug, Clone)]
pub struct WorkerOptions {
    /// If true, stop as soon as no eligible ready tasks remain.
    pub once: bool,
    /// Stop after executing this many tasks.
    pub max_tasks: Option<usize>,
    /// How long to wait between polls when the queue has no eligible tasks.
    pub poll_interval: Duration,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        Self {
            once: false,
            max_tasks: None,
            poll_interval: Duration::from_secs(5),
        }
    }
}

/// A signal to request graceful shutdown of the worker loop.
#[derive(Clone, Debug)]
pub struct ShutdownSignal {
    receiver: watch::Receiver<bool>,
}

impl ShutdownSignal {
    pub fn new(receiver: watch::Receiver<bool>) -> Self {
        Self { receiver }
    }

    pub fn is_shutting_down(&self) -> bool {
        *self.receiver.borrow()
    }

    pub async fn wait_for_shutdown(&mut self) {
        if self.is_shutting_down() {
            return;
        }
        let _ = self.receiver.wait_for(|&val| val).await;
    }
}

/// Statistics reported by the worker loop.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkerStats {
    pub tasks_executed: usize,
    pub tasks_succeeded: usize,
    pub tasks_blocked: usize,
}

/// Drain the ready task queue according to `options`, running each task along
/// its fallback chain and stopping cleanly on `shutdown`.
pub async fn run_worker(
    store: &mut Store,
    config: &Config,
    state_dir: &Path,
    options: WorkerOptions,
    mut shutdown: ShutdownSignal,
) -> Result<WorkerStats, WorkerError> {
    let _lock = WorkerLock::acquire(state_dir)?;
    let _recovered = recover_interrupted(store)?;

    let mut stats = WorkerStats::default();

    loop {
        if shutdown.is_shutting_down() {
            break;
        }
        if let Some(max) = options.max_tasks
            && stats.tasks_executed >= max
        {
            break;
        }

        let task = store.pick_next_ready_task().map_err(WorkerError::Store)?;
        match task {
            None => {
                if options.once {
                    break;
                }
                tokio::select! {
                    () = shutdown.wait_for_shutdown() => {
                        break;
                    }
                    () = tokio::time::sleep(options.poll_interval) => {}
                }
            }
            Some(task) => {
                let summary = run_task(store, config, state_dir, task.id).await?;
                println!(
                    "task {} {}: {} ({}, {} attempt(s))",
                    task.id,
                    task.title,
                    summary.status.as_str(),
                    summary.profile,
                    summary.attempts.len()
                );
                stats.tasks_executed = stats.tasks_executed.saturating_add(1);
                let final_status =
                    if summary.status == TaskStatus::Review && !config.review.profiles.is_empty() {
                        crate::review::run_review_loop(store, config, state_dir, task.id).await?
                    } else {
                        summary.status
                    };
                if final_status == TaskStatus::Review {
                    stats.tasks_succeeded = stats.tasks_succeeded.saturating_add(1);
                } else {
                    stats.tasks_blocked = stats.tasks_blocked.saturating_add(1);
                }

                if config.learning.profile.is_some()
                    && (summary.status == TaskStatus::Review
                        || summary.status == TaskStatus::Blocked
                        || summary.status == TaskStatus::NeedsHuman)
                {
                    let runs = store.list_runs(task.id).map_err(WorkerError::Store)?;
                    let current_task = store.get_task(task.id).map_err(WorkerError::Store)?;
                    if !config.learning.min_signal
                        || crate::learning::has_learning_signal(&current_task, &runs)
                    {
                        match crate::learning::retrospect_task(store, config, state_dir, task.id)
                            .await
                        {
                            Ok(Some(lesson)) => {
                                println!(
                                    "lesson {} proposed for skill {}",
                                    lesson.id, lesson.skill
                                );
                            }
                            Ok(None) => {}
                            Err(err) => {
                                tracing::warn!(task_id = task.id.get(), %err, "retrospective failed");
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]

    use overseer_core::{BackendKind, NewTask, Permissions, Profile};
    use tempfile::TempDir;

    use super::*;
    use crate::test_support::init_repo;

    struct Fixture {
        _root: TempDir,
        repo: std::path::PathBuf,
        state: std::path::PathBuf,
        store: Store,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let state = root.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let store = Store::open(&state.join("overseer.db")).unwrap();
        Fixture {
            _root: root,
            repo,
            state,
            store,
        }
    }

    fn fake_command_profile(name: &str, sh_command: &str) -> Profile {
        Profile {
            name: name.to_owned(),
            backend: BackendKind::Command,
            model: None,
            base_url: None,
            api_key_env: None,
            command: Some(vec![
                "sh".to_owned(),
                "-c".to_owned(),
                sh_command.to_owned(),
            ]),
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
        }
    }

    fn test_config(repo: &Path) -> Config {
        Config {
            repo: repo.to_path_buf(),
            default_profile: "succeed".to_owned(),
            review: overseer_core::ReviewConfig::default(),
            profiles: vec![
                fake_command_profile("succeed", "echo ok > file.txt && echo done"),
                fake_command_profile("fail", "exit 1"),
            ],
            learning: Default::default(),
        }
    }

    fn shutdown_pair() -> (watch::Sender<bool>, ShutdownSignal) {
        let (tx, rx) = watch::channel(false);
        (tx, ShutdownSignal::new(rx))
    }

    #[tokio::test]
    async fn worker_once_exits_when_no_eligible_tasks() {
        let mut fx = fixture();
        let config = test_config(&fx.repo);
        let (_tx, shutdown) = shutdown_pair();

        let options = WorkerOptions {
            once: true,
            max_tasks: None,
            poll_interval: Duration::from_millis(10),
        };

        let stats = run_worker(&mut fx.store, &config, &fx.state, options, shutdown)
            .await
            .unwrap();

        assert_eq!(stats.tasks_executed, 0);
    }

    #[tokio::test]
    async fn worker_max_tasks_stops_after_n_runs() {
        let mut fx = fixture();
        let config = test_config(&fx.repo);
        let (_tx, shutdown) = shutdown_pair();

        let t1 = fx
            .store
            .add_task(NewTask {
                title: "task 1".to_owned(),
                ready: true,
                ..NewTask::default()
            })
            .unwrap();
        let t2 = fx
            .store
            .add_task(NewTask {
                title: "task 2".to_owned(),
                ready: true,
                ..NewTask::default()
            })
            .unwrap();
        let t3 = fx
            .store
            .add_task(NewTask {
                title: "task 3".to_owned(),
                ready: true,
                ..NewTask::default()
            })
            .unwrap();

        let options = WorkerOptions {
            once: false,
            max_tasks: Some(2),
            poll_interval: Duration::from_millis(10),
        };

        let stats = run_worker(&mut fx.store, &config, &fx.state, options, shutdown)
            .await
            .unwrap();

        assert_eq!(stats.tasks_executed, 2);
        assert_eq!(stats.tasks_succeeded, 2);
        assert_eq!(fx.store.get_task(t1.id).unwrap().status, TaskStatus::Review);
        assert_eq!(fx.store.get_task(t2.id).unwrap().status, TaskStatus::Review);
        // t3 was not reached and remains ready
        assert_eq!(fx.store.get_task(t3.id).unwrap().status, TaskStatus::Ready);
    }

    #[tokio::test]
    async fn worker_dependency_ordering() {
        let mut fx = fixture();
        let config = test_config(&fx.repo);

        let t1 = fx
            .store
            .add_task(NewTask {
                title: "dep task".to_owned(),
                ready: true,
                ..NewTask::default()
            })
            .unwrap();
        let t2 = fx
            .store
            .add_task(NewTask {
                title: "dependent task".to_owned(),
                ready: true,
                depends_on: vec![t1.id],
                ..NewTask::default()
            })
            .unwrap();

        let (_tx, shutdown) = shutdown_pair();
        // Run with once = true. Since t2 depends on t1 and t1 is not Done, only t1 should run.
        let stats = run_worker(
            &mut fx.store,
            &config,
            &fx.state,
            WorkerOptions {
                once: true,
                max_tasks: Some(1),
                poll_interval: Duration::from_millis(10),
            },
            shutdown,
        )
        .await
        .unwrap();

        assert_eq!(stats.tasks_executed, 1);
        assert_eq!(fx.store.get_task(t1.id).unwrap().status, TaskStatus::Review);
        assert_eq!(fx.store.get_task(t2.id).unwrap().status, TaskStatus::Ready);

        // Mark t1 done so t2 becomes eligible
        fx.store.set_status(t1.id, TaskStatus::Done).unwrap();

        let (_tx, shutdown) = shutdown_pair();
        let stats2 = run_worker(
            &mut fx.store,
            &config,
            &fx.state,
            WorkerOptions {
                once: true,
                max_tasks: Some(1),
                poll_interval: Duration::from_millis(10),
            },
            shutdown,
        )
        .await
        .unwrap();

        assert_eq!(stats2.tasks_executed, 1);
        assert_eq!(fx.store.get_task(t2.id).unwrap().status, TaskStatus::Review);
    }

    #[tokio::test]
    async fn worker_blocked_task_does_not_stop_the_loop() {
        let mut fx = fixture();
        let config = test_config(&fx.repo);
        let (_tx, shutdown) = shutdown_pair();

        let t1 = fx
            .store
            .add_task(NewTask {
                title: "failing task".to_owned(),
                profile: Some("fail".to_owned()),
                ready: true,
                priority: 1,
                ..NewTask::default()
            })
            .unwrap();
        let t2 = fx
            .store
            .add_task(NewTask {
                title: "succeeding task".to_owned(),
                profile: Some("succeed".to_owned()),
                ready: true,
                priority: 2,
                ..NewTask::default()
            })
            .unwrap();

        let stats = run_worker(
            &mut fx.store,
            &config,
            &fx.state,
            WorkerOptions {
                once: true,
                max_tasks: None,
                poll_interval: Duration::from_millis(10),
            },
            shutdown,
        )
        .await
        .unwrap();

        assert_eq!(stats.tasks_executed, 2);
        assert_eq!(stats.tasks_blocked, 1);
        assert_eq!(stats.tasks_succeeded, 1);

        assert_eq!(
            fx.store.get_task(t1.id).unwrap().status,
            TaskStatus::Blocked
        );
        assert_eq!(fx.store.get_task(t2.id).unwrap().status, TaskStatus::Review);
    }

    #[tokio::test]
    async fn worker_shutdown_signal_stops_idle_sleep() {
        let mut fx = fixture();
        let config = test_config(&fx.repo);
        let (tx, shutdown) = shutdown_pair();

        // Spawn a background task to trigger shutdown after a short delay
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = tx.send(true);
        });

        let stats = run_worker(
            &mut fx.store,
            &config,
            &fx.state,
            WorkerOptions {
                once: false,
                max_tasks: None,
                poll_interval: Duration::from_secs(60), // would hang if not interruptible
            },
            shutdown,
        )
        .await
        .unwrap();

        assert_eq!(stats.tasks_executed, 0);
    }

    #[tokio::test]
    async fn worker_executes_review_loop_when_configured() {
        let mut fx = fixture();
        let mut config = test_config(&fx.repo);
        config.review = overseer_core::ReviewConfig {
            profiles: vec!["reviewer".to_owned()],
            max_reworks: 2,
            rework_profile: None,
        };
        config.profiles.push(fake_command_profile(
            "reviewer",
            "echo 'VERDICT: PASS' && echo 'Good job'",
        ));

        let (_, shutdown) = shutdown_pair();
        let t = fx
            .store
            .add_task(NewTask {
                title: "task with review".to_owned(),
                profile: Some("succeed".to_owned()),
                ready: true,
                ..NewTask::default()
            })
            .unwrap();

        let stats = run_worker(
            &mut fx.store,
            &config,
            &fx.state,
            WorkerOptions {
                once: true,
                max_tasks: None,
                poll_interval: Duration::from_millis(10),
            },
            shutdown,
        )
        .await
        .unwrap();

        assert_eq!(stats.tasks_executed, 1);
        assert_eq!(stats.tasks_succeeded, 1);

        let task = fx.store.get_task(t.id).unwrap();
        assert_eq!(task.status, TaskStatus::Review);
        assert!(task.approved);

        let reviews = fx.store.list_reviews(t.id).unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].verdict, overseer_core::Verdict::Pass);
    }

    #[tokio::test]
    async fn worker_triggers_retrospective_when_signal_passes() {
        let mut fx = fixture();
        let mut config = test_config(&fx.repo);
        config
            .profiles
            .push(fake_command_profile("learner", "echo 'Always test first.'"));
        config.learning.profile = Some("learner".to_owned());
        config.learning.skill = Some("general".to_owned());
        config.learning.min_signal = true;

        let (_tx, shutdown) = shutdown_pair();

        let t1 = fx
            .store
            .add_task(NewTask {
                title: "failing task".to_owned(),
                profile: Some("fail".to_owned()),
                ready: true,
                priority: 1,
                ..NewTask::default()
            })
            .unwrap();

        let stats = run_worker(
            &mut fx.store,
            &config,
            &fx.state,
            WorkerOptions {
                once: true,
                max_tasks: Some(1),
                poll_interval: Duration::from_millis(10),
            },
            shutdown,
        )
        .await
        .unwrap();

        assert_eq!(stats.tasks_executed, 1);
        assert_eq!(stats.tasks_blocked, 1);
        assert_eq!(
            fx.store.get_task(t1.id).unwrap().status,
            TaskStatus::Blocked
        );

        let lessons = fx.store.list_lessons(None).unwrap();
        assert_eq!(lessons.len(), 1);
        assert_eq!(lessons[0].task_id, t1.id);
        assert_eq!(lessons[0].skill, "general");
        assert_eq!(lessons[0].status, overseer_core::LessonStatus::Proposed);

        let md_file = fx
            .state
            .join("lessons")
            .join(format!("{}.md", lessons[0].id));
        let diff_file = fx
            .state
            .join("lessons")
            .join(format!("{}.diff", lessons[0].id));
        assert!(md_file.is_file());
        assert!(diff_file.is_file());
    }
}
