use std::fmt;
use std::path::PathBuf;

use crate::Permissions;

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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

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
