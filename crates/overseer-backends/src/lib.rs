//! Model backends: the `Backend` trait and its implementations.
//!
//! A backend turns a `RunRequest` into a `RunOutcome`. Failures the
//! orchestrator can act on (quota, auth, unreachable) are reported inside the
//! outcome as a `FailureKind`; `BackendError` is reserved for problems that
//! prevent the backend from running at all.

mod command;
mod error;
pub mod health;
mod mock;
mod openai;
pub mod tools;

use async_trait::async_trait;
use overseer_core::{BackendKind, FailureKind, Profile, RunOutcome, RunRequest};

pub use command::CommandBackend;
pub use error::BackendError;
pub use health::{Health, check_profile};
pub use mock::MockBackend;
pub use openai::OpenAiBackend;

#[async_trait]
pub trait Backend: Send + Sync {
    async fn run(&self, req: &RunRequest) -> Result<RunOutcome, BackendError>;
}

/// Choose and construct the backend a profile asks for.
pub fn build_backend(profile: &Profile) -> Result<Box<dyn Backend>, BackendError> {
    match profile.backend {
        BackendKind::Openai => Ok(Box::new(OpenAiBackend::new(profile)?)),
        BackendKind::Command => Ok(Box::new(CommandBackend::new(profile)?)),
    }
}

/// Outcome for a run that failed before producing any output.
fn failed(kind: FailureKind) -> RunOutcome {
    RunOutcome {
        output: String::new(),
        usage: overseer_core::Usage::default(),
        failure: Some(kind),
    }
}

/// Keep at most `max` characters of `text`, marking the cut.
fn tail(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.trim().to_owned();
    }
    let skipped = count.saturating_sub(max);
    let kept: String = text.chars().skip(skipped).collect();
    format!("...{}", kept.trim())
}
