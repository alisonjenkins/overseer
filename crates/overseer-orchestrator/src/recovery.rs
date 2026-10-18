//! Crash recovery: cleans up running tasks and runs left behind by a dead worker.

use overseer_core::{RunFinish, RunId, RunStatus, TaskId, TaskStatus};
use overseer_store::{Store, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("failed to query running tasks for crash recovery")]
    ListTasks(#[source] StoreError),

    #[error("failed to load runs of task {task_id} during crash recovery")]
    ListRuns {
        task_id: TaskId,
        #[source]
        source: StoreError,
    },

    #[error("failed to mark interrupted task {task_id} as blocked")]
    SetTaskStatus {
        task_id: TaskId,
        #[source]
        source: StoreError,
    },

    #[error("failed to mark interrupted run {run_id} as failed")]
    FinishRun {
        run_id: RunId,
        #[source]
        source: StoreError,
    },
}

/// Reset any task left in `running` status to `blocked`, and finish its newest
/// `running` run row as failed with failure text `interrupted`.
pub fn recover_interrupted(store: &mut Store) -> Result<Vec<TaskId>, RecoveryError> {
    let running_tasks = store
        .list_tasks(Some(TaskStatus::Running))
        .map_err(RecoveryError::ListTasks)?;
    let mut recovered = Vec::new();

    for task in running_tasks {
        store
            .set_status(task.id, TaskStatus::Blocked)
            .map_err(|source| RecoveryError::SetTaskStatus {
                task_id: task.id,
                source,
            })?;

        let runs = store
            .list_runs(task.id)
            .map_err(|source| RecoveryError::ListRuns {
                task_id: task.id,
                source,
            })?;

        if let Some(run) = runs.iter().rev().find(|r| r.status == RunStatus::Running) {
            store
                .finish_run(
                    run.id,
                    RunFinish {
                        status: RunStatus::Failed,
                        failure: Some("interrupted".to_owned()),
                        usage: run.usage,
                        transcript_path: run.transcript_path.clone(),
                        branch: run.branch.clone(),
                    },
                )
                .map_err(|source| RecoveryError::FinishRun {
                    run_id: run.id,
                    source,
                })?;
        }

        tracing::warn!(
            task_id = task.id.get(),
            "recovered task left running by dead worker: marked blocked"
        );
        recovered.push(task.id);
    }

    Ok(recovered)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]

    use overseer_core::NewTask;

    use super::*;

    fn new_task(title: &str) -> NewTask {
        NewTask {
            title: title.to_owned(),
            ..NewTask::default()
        }
    }

    #[test]
    fn recover_with_no_running_tasks_does_nothing() {
        let mut store = Store::open_in_memory().unwrap();
        let t1 = store.add_task(new_task("backlog")).unwrap();
        let t2 = store
            .add_task(NewTask {
                ready: true,
                ..new_task("ready")
            })
            .unwrap();

        let recovered = recover_interrupted(&mut store).unwrap();
        assert!(recovered.is_empty());
        assert_eq!(store.get_task(t1.id).unwrap().status, TaskStatus::Backlog);
        assert_eq!(store.get_task(t2.id).unwrap().status, TaskStatus::Ready);
    }

    #[test]
    fn recover_single_running_task_with_running_run() {
        let mut store = Store::open_in_memory().unwrap();
        let task = store.add_task(new_task("interrupted task")).unwrap();
        store.claim_task(task.id).unwrap();
        let run_id = store.start_run(task.id, "local", None).unwrap();

        let recovered = recover_interrupted(&mut store).unwrap();
        assert_eq!(recovered, vec![task.id]);

        let updated_task = store.get_task(task.id).unwrap();
        assert_eq!(updated_task.status, TaskStatus::Blocked);

        let runs = store.list_runs(task.id).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, run_id);
        assert_eq!(runs[0].status, RunStatus::Failed);
        assert_eq!(runs[0].failure.as_deref(), Some("interrupted"));
    }

    #[test]
    fn recover_running_task_with_multiple_runs_picks_newest_running() {
        let mut store = Store::open_in_memory().unwrap();
        let task = store.add_task(new_task("multi-run task")).unwrap();
        store.claim_task(task.id).unwrap();
        let r1 = store.start_run(task.id, "local", None).unwrap();
        store
            .finish_run(
                r1,
                RunFinish {
                    status: RunStatus::Failed,
                    failure: Some("unreachable".to_owned()),
                    usage: Default::default(),
                    transcript_path: None,
                    branch: None,
                },
            )
            .unwrap();
        let r2 = store.start_run(task.id, "alt", None).unwrap();

        let recovered = recover_interrupted(&mut store).unwrap();
        assert_eq!(recovered, vec![task.id]);

        let runs = store.list_runs(task.id).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, r1);
        assert_eq!(runs[0].status, RunStatus::Failed);
        assert_eq!(runs[0].failure.as_deref(), Some("unreachable"));

        assert_eq!(runs[1].id, r2);
        assert_eq!(runs[1].status, RunStatus::Failed);
        assert_eq!(runs[1].failure.as_deref(), Some("interrupted"));
    }

    #[test]
    fn recover_multiple_running_tasks() {
        let mut store = Store::open_in_memory().unwrap();
        let t1 = store.add_task(new_task("task 1")).unwrap();
        store.claim_task(t1.id).unwrap();
        let r1 = store.start_run(t1.id, "local", None).unwrap();

        let t2 = store.add_task(new_task("task 2")).unwrap();
        store.claim_task(t2.id).unwrap();
        let r2 = store.start_run(t2.id, "alt", None).unwrap();

        let recovered = recover_interrupted(&mut store).unwrap();
        assert_eq!(recovered, vec![t1.id, t2.id]);

        assert_eq!(store.get_task(t1.id).unwrap().status, TaskStatus::Blocked);
        assert_eq!(store.get_task(t2.id).unwrap().status, TaskStatus::Blocked);

        let runs1 = store.list_runs(t1.id).unwrap();
        assert_eq!(runs1[0].id, r1);
        assert_eq!(runs1[0].status, RunStatus::Failed);
        assert_eq!(runs1[0].failure.as_deref(), Some("interrupted"));

        let runs2 = store.list_runs(t2.id).unwrap();
        assert_eq!(runs2[0].id, r2);
        assert_eq!(runs2[0].status, RunStatus::Failed);
        assert_eq!(runs2[0].failure.as_deref(), Some("interrupted"));
    }

    #[test]
    fn recover_running_task_without_runs() {
        let mut store = Store::open_in_memory().unwrap();
        let task = store.add_task(new_task("no runs")).unwrap();
        store.claim_task(task.id).unwrap();

        let recovered = recover_interrupted(&mut store).unwrap();
        assert_eq!(recovered, vec![task.id]);
        assert_eq!(store.get_task(task.id).unwrap().status, TaskStatus::Blocked);
    }
}
