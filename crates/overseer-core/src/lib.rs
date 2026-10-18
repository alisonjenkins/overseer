//! Domain types for the overseer task orchestrator.

mod config;
mod permissions;
mod run;
mod status;
mod task;

pub use config::{BackendKind, Config, Profile};
pub use permissions::Permissions;
pub use run::{FailureKind, Run, RunFinish, RunId, RunOutcome, RunRequest, RunStatus, Usage};
pub use status::TaskStatus;
pub use task::{NewTask, Task, TaskId};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CoreError {
    #[error(
        "unknown task status {value:?}; expected one of backlog, ready, running, review, blocked, needs-human, done"
    )]
    UnknownStatus { value: String },

    #[error("unknown permission level {value:?}; expected read, write-workdir or write-shell")]
    UnknownPermissions { value: String },

    #[error("failed to parse overseer.toml: {message}")]
    ConfigParse { message: String },

    #[error("failed to serialise overseer.toml: {message}")]
    ConfigSerialize { message: String },

    #[error("profile {name:?} is defined more than once")]
    DuplicateProfile { name: String },

    #[error("profile {profile:?} is missing required field {field:?}")]
    MissingProfileField {
        profile: String,
        field: &'static str,
    },

    #[error("default_profile {name:?} does not match any profile")]
    UnknownDefaultProfile { name: String },

    #[error("profile {profile:?} falls back to {fallback:?}, which is not a profile")]
    UnknownFallback { profile: String, fallback: String },

    #[error("fallback list of profile {profile:?} forms a cycle")]
    FallbackCycle { profile: String },

    #[error("profile {profile:?} sets prompt_flag together with prompt_stdin = true")]
    PromptFlagWithStdin { profile: String },

    #[error(
        "profile {profile:?} has invalid env key {key:?}; keys must be non-empty and contain no '='"
    )]
    InvalidEnvKey { profile: String, key: String },

    #[error("profile {profile:?} sets max_steps to {max_steps}; expected at least 1")]
    InvalidMaxSteps { profile: String, max_steps: u32 },
}
