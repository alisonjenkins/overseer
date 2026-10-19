use std::path::PathBuf;

use overseer_core::{LessonId, RunId, TaskId, TaskStatus};

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

    #[error("failed to mark task {id} ready")]
    ReadyTask {
        id: TaskId,
        #[source]
        source: rusqlite::Error,
    },

    #[error("task {id} cannot be marked ready while it is {status}; must be backlog or blocked")]
    TaskNotReadyable { id: TaskId, status: TaskStatus },

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

    #[error("failed to read setting {key:?}")]
    GetSetting {
        key: String,
        #[source]
        source: rusqlite::Error,
    },

    #[error("failed to write setting {key:?}")]
    SetSetting {
        key: String,
        #[source]
        source: rusqlite::Error,
    },

    #[error("failed to pick the next ready task")]
    PickNextReadyTask {
        #[source]
        source: rusqlite::Error,
    },

    #[error("failed to count tasks by status")]
    TaskCounts {
        #[source]
        source: rusqlite::Error,
    },

    #[error("failed to list recent runs")]
    ListRecentRuns {
        #[source]
        source: rusqlite::Error,
    },

    #[error("failed to insert review for task {task_id}")]
    InsertReview {
        task_id: TaskId,
        #[source]
        source: rusqlite::Error,
    },

    #[error("failed to list reviews for task {task_id}")]
    ListReviews {
        task_id: TaskId,
        #[source]
        source: rusqlite::Error,
    },

    #[error("failed to set approval on task {task_id}")]
    SetApproved {
        task_id: TaskId,
        #[source]
        source: rusqlite::Error,
    },

    #[error("failed to increment round on task {task_id}")]
    IncrementRound {
        task_id: TaskId,
        #[source]
        source: rusqlite::Error,
    },

    #[error("failed to insert lesson for task {task_id}")]
    InsertLesson {
        task_id: TaskId,
        #[source]
        source: rusqlite::Error,
    },

    #[error("failed to update path of lesson {id}")]
    UpdateLessonPath {
        id: LessonId,
        #[source]
        source: rusqlite::Error,
    },

    #[error("failed to load lesson {id}")]
    LoadLesson {
        id: LessonId,
        #[source]
        source: rusqlite::Error,
    },

    #[error("no lesson with id {id}")]
    UnknownLesson { id: LessonId },

    #[error("failed to list lessons")]
    ListLessons {
        #[source]
        source: rusqlite::Error,
    },

    #[error("failed to update status of lesson {id}")]
    SetLessonStatus {
        id: LessonId,
        #[source]
        source: rusqlite::Error,
    },

    #[error("no skill named {name:?}")]
    SkillNotFound { name: String },

    #[error("skill {name:?} has no version {version}")]
    SkillVersionNotFound { name: String, version: u32 },

    #[error("failed to parse version file at {path}")]
    InvalidSkillVersionFile { path: PathBuf },

    #[error("skill I/O failed while {action} on {path}")]
    SkillIo {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid skill name {name:?}; must match [a-z0-9-]+")]
    InvalidSkillName { name: String },

    #[error("failed to find task with external id {external_id:?} from source {task_source:?}")]
    FindTaskByExternalId {
        task_source: Box<str>,
        external_id: Box<str>,
        #[source]
        source: Box<rusqlite::Error>,
    },

    #[error("failed to list tasks for source {task_source:?}")]
    ListTasksBySource {
        task_source: Box<str>,
        #[source]
        source: Box<rusqlite::Error>,
    },

    #[error("failed to update title and body of task {id}")]
    SetTaskTitleAndBody {
        id: TaskId,
        #[source]
        source: Box<rusqlite::Error>,
    },

    #[error("failed to get progress for source {task_source:?}")]
    ProgressBySource {
        task_source: Box<str>,
        #[source]
        source: Box<rusqlite::Error>,
    },

    #[error("failed to list child tasks for parent {parent}")]
    ListChildTasks {
        parent: TaskId,
        #[source]
        source: Box<rusqlite::Error>,
    },

    #[error("failed to set parent of task {id}")]
    SetTaskParent {
        id: TaskId,
        #[source]
        source: Box<rusqlite::Error>,
    },
}
