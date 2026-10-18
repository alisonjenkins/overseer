//! Domain types for the overseer task orchestrator.

mod status;
mod task;

pub use status::TaskStatus;
pub use task::{NewTask, Task, TaskId};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CoreError {
    #[error(
        "unknown task status {value:?}; expected one of backlog, ready, running, review, blocked, needs-human, done"
    )]
    UnknownStatus { value: String },
}
