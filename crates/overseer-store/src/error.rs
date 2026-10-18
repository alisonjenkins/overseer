use std::path::PathBuf;

use overseer_core::{RunId, TaskId, TaskStatus};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("failed to open database at {path}")]
    OpenDatabase {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("failed to open in-memory database")]
    OpenInMemory {
        #[source]
        source: rusqlite::Error,
    },

    #[error("failed to apply schema migration to version {version}")]
    Migrate {
        version: i64,
        #[source]
        source: rusqlite::Error,
    },

    #[error("failed to format creation timestamp")]
    FormatTimestamp {
        #[source]
        source: time::error::Format,
    },

    #[error("failed to insert task {title:?}")]
    InsertTask {
        title: String,
        #[source]
        source: rusqlite::Error,
    },

    #[error("failed to check whether dependency {depends_on} exists")]
    CheckDependency {
        depends_on: TaskId,
        #[source]
        source: rusqlite::Error,
    },

    #[error("failed to insert dependency on task {depends_on}")]
    InsertDependency {
        depends_on: TaskId,
        #[source]
        source: rusqlite::Error,
    },

    #[error("failed to load task {id}")]
    LoadTask {
        id: TaskId,
        #[source]
        source: rusqlite::Error,
    },

    #[error("failed to list tasks")]
    ListTasks {
        #[source]
        source: rusqlite::Error,
    },

    #[error("failed to update status of task {id}")]
    SetStatus {
        id: TaskId,
        #[source]
        source: rusqlite::Error,
    },

    #[error("no task with id {id}")]
    UnknownTask { id: TaskId },

    #[error("failed to claim task {id}")]
    ClaimTask {
        id: TaskId,
        #[source]
        source: rusqlite::Error,
    },

    #[error("task {id} cannot be claimed while it is {status}")]
    TaskNotClaimable { id: TaskId, status: TaskStatus },

    #[error("failed to record a run for task {task_id}")]
    InsertRun {
        task_id: TaskId,
        #[source]
        source: rusqlite::Error,
    },

    #[error("run {id} cannot be finished with status running")]
    InvalidRunFinish { id: RunId },

    #[error("failed to update run {id}")]
    UpdateRun {
        id: RunId,
        #[source]
        source: rusqlite::Error,
    },

    #[error("no run with id {id}")]
    UnknownRun { id: RunId },

    #[error("failed to list runs of task {task_id}")]
    ListRuns {
        task_id: TaskId,
        #[source]
        source: rusqlite::Error,
    },

    #[error("cannot depend on task {depends_on}: no such task")]
    UnknownDependency { depends_on: TaskId },
}
