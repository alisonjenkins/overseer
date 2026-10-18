use std::fmt;
use std::path::PathBuf;

use crate::{Permissions, TaskId};

/// Everything a backend needs to execute one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    pub system: Option<String>,
    pub prompt: String,
    pub workdir: PathBuf,
    pub permissions: Permissions,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Why a run failed. The first three kinds are routable: the orchestrator may
/// retry the task on the next profile in the fallback list. `Other` never is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureKind {
    QuotaExhausted,
    AuthRequired,
    Unreachable,
    Other(String),
}

impl FailureKind {
    /// True when the next profile in the fallback list should be tried.
    pub fn is_routable(&self) -> bool {
        matches!(
            self,
            Self::QuotaExhausted | Self::AuthRequired | Self::Unreachable
        )
    }
}

impl fmt::Display for FailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QuotaExhausted => f.write_str("quota-exhausted"),
            Self::AuthRequired => f.write_str("auth-required"),
            Self::Unreachable => f.write_str("unreachable"),
            Self::Other(detail) => write!(f, "other: {detail}"),
        }
    }
}

/// The result of a backend call that got as far as producing an answer or a
/// classified failure. Transport-level breakage is a `BackendError` instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    pub output: String,
    pub usage: Usage,
    pub failure: Option<FailureKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunId(i64);

impl RunId {
    pub const fn new(raw: i64) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// Lifecycle of one run row. A run is `Running` from claim until it is
/// finished as `Succeeded` or `Failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
}

impl RunStatus {
    pub const ALL: [Self; 3] = [Self::Running, Self::Succeeded, Self::Failed];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    pub fn from_db_text(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|status| status.as_str() == text)
    }
}

/// A stored run, as read back from the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub id: RunId,
    pub task_id: TaskId,
    pub profile: String,
    pub status: RunStatus,
    /// ISO8601 UTC.
    pub started_at: String,
    pub finished_at: Option<String>,
    pub usage: Usage,
    pub failure: Option<String>,
    pub transcript_path: Option<PathBuf>,
    pub branch: Option<String>,
    pub skills: Option<String>,
}

/// Values written when a run finishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFinish {
    /// Must be `Succeeded` or `Failed`.
    pub status: RunStatus,
    pub usage: Usage,
    pub failure: Option<String>,
    pub transcript_path: Option<PathBuf>,
    pub branch: Option<String>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn run_status_strings_round_trip() {
        for status in RunStatus::ALL {
            assert_eq!(RunStatus::from_db_text(status.as_str()), Some(status));
        }
        assert_eq!(RunStatus::from_db_text("queued"), None);
    }

    #[test]
    fn only_quota_auth_and_unreachable_are_routable() {
        assert!(FailureKind::QuotaExhausted.is_routable());
        assert!(FailureKind::AuthRequired.is_routable());
        assert!(FailureKind::Unreachable.is_routable());
        assert!(!FailureKind::Other("exit 2".to_owned()).is_routable());
    }

    #[test]
    fn failure_kind_display_is_kebab_case() {
        assert_eq!(FailureKind::QuotaExhausted.to_string(), "quota-exhausted");
        assert_eq!(FailureKind::AuthRequired.to_string(), "auth-required");
        assert_eq!(FailureKind::Unreachable.to_string(), "unreachable");
        assert_eq!(
            FailureKind::Other("status 500".to_owned()).to_string(),
            "other: status 500"
        );
    }

    #[test]
    fn usage_defaults_to_zero() {
        assert_eq!(
            Usage::default(),
            Usage {
                input_tokens: 0,
                output_tokens: 0
            }
        );
    }

    #[test]
    fn run_outcome_compares_whole_values() {
        let ok = RunOutcome {
            output: "done".to_owned(),
            usage: Usage {
                input_tokens: 10,
                output_tokens: 3,
            },
            failure: None,
        };
        assert_eq!(ok.clone(), ok);
        assert!(ok.failure.is_none());
    }
}
