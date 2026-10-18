use std::fmt;
use std::str::FromStr;

use crate::CoreError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskStatus {
    Backlog,
    Ready,
    Running,
    Review,
    Blocked,
    NeedsHuman,
    Done,
}

impl TaskStatus {
    pub const ALL: [Self; 7] = [
        Self::Backlog,
        Self::Ready,
        Self::Running,
        Self::Review,
        Self::Blocked,
        Self::NeedsHuman,
        Self::Done,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Review => "review",
            Self::Blocked => "blocked",
            Self::NeedsHuman => "needs-human",
            Self::Done => "done",
        }
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskStatus {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == s)
            .ok_or_else(|| CoreError::UnknownStatus {
                value: s.to_owned(),
            })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn status_strings_round_trip() {
        for status in TaskStatus::ALL {
            let text = status.to_string();
            assert_eq!(text.parse::<TaskStatus>().unwrap(), status);
        }
    }

    #[test]
    fn status_strings_are_kebab_case() {
        let expected = [
            (TaskStatus::Backlog, "backlog"),
            (TaskStatus::Ready, "ready"),
            (TaskStatus::Running, "running"),
            (TaskStatus::Review, "review"),
            (TaskStatus::Blocked, "blocked"),
            (TaskStatus::NeedsHuman, "needs-human"),
            (TaskStatus::Done, "done"),
        ];
        for (status, text) in expected {
            assert_eq!(status.to_string(), text);
            assert_eq!(text.parse::<TaskStatus>().unwrap(), status);
        }
    }

    #[test]
    fn unknown_status_is_rejected() {
        assert_eq!(
            "needs_human".parse::<TaskStatus>(),
            Err(CoreError::UnknownStatus {
                value: "needs_human".to_owned()
            })
        );
    }
}
