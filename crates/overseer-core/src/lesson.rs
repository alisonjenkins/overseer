use std::fmt;
use std::path::PathBuf;

use crate::TaskId;

/// Strongly-typed identifier for a lesson row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LessonId(i64);

impl LessonId {
    pub const fn new(raw: i64) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

impl fmt::Display for LessonId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// Lifecycle status of a proposed lesson.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LessonStatus {
    Proposed,
    Applied,
    Rejected,
}

impl LessonStatus {
    pub const ALL: [Self; 3] = [Self::Proposed, Self::Applied, Self::Rejected];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Applied => "applied",
            Self::Rejected => "rejected",
        }
    }

    pub fn from_db_text(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|status| status.as_str() == text)
    }
}

/// Inputs to create a new proposed lesson row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewLesson {
    pub task_id: TaskId,
    pub skill: String,
    pub base_version: u32,
    pub path: PathBuf,
}

/// A proposed, applied, or rejected lesson record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lesson {
    pub id: LessonId,
    pub task_id: TaskId,
    pub skill: String,
    pub base_version: u32,
    pub status: LessonStatus,
    pub path: PathBuf,
    /// ISO8601 UTC timestamp.
    pub created_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lesson_status_strings_round_trip() {
        for status in LessonStatus::ALL {
            assert_eq!(LessonStatus::from_db_text(status.as_str()), Some(status));
        }
        assert_eq!(LessonStatus::from_db_text("unknown"), None);
    }

    #[test]
    fn lesson_id_display() {
        assert_eq!(LessonId::new(42).to_string(), "42");
        assert_eq!(LessonId::new(42).get(), 42);
    }
}
