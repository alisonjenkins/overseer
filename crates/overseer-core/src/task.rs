use crate::TaskStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(i64);

impl TaskId {
    pub const fn new(raw: i64) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Input for creating a task. The store assigns the id, status, round and creation time.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewTask {
    pub title: String,
    pub body: String,
    pub priority: i32,
    pub profile: Option<String>,
    pub depends_on: Vec<TaskId>,
    pub ready: bool,
    pub source: Option<String>,
    pub external_id: Option<String>,
    pub parent_task_id: Option<TaskId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    pub body: String,
    pub status: TaskStatus,
    /// Lower value means more urgent.
    pub priority: i32,
    pub profile: Option<String>,
    pub round: u8,
    pub approved: bool,
    pub depends_on: Vec<TaskId>,
    /// ISO8601 UTC, e.g. `2026-10-18T08:00:00Z`.
    pub created_at: String,
    pub reason: Option<String>,
    pub source: Option<String>,
    pub external_id: Option<String>,
    pub parent_task_id: Option<TaskId>,
}
