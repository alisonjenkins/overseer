//! Domain types for the overseer task orchestrator.

mod config;
mod lesson;
mod permissions;
mod review;
mod run;
mod source;
mod status;
mod task;

pub use config::{
    BackendKind, Config, EscalationConfig, GpuConfig, LearningConfig, Profile, ReviewConfig,
    SourceConfig, is_valid_skill_name,
};
pub use lesson::{Lesson, LessonId, LessonStatus, NewLesson};
pub use permissions::Permissions;
pub use review::{
    NewReview, ParsedReview, ReviewParseError, ReviewRecord, Verdict, parse_review, parse_verdict,
};
pub use run::{FailureKind, Run, RunFinish, RunId, RunOutcome, RunRequest, RunStatus, Usage};
pub use source::{SourceError, SourcedTask, TaskSource};
pub use status::TaskStatus;
pub use task::{NewTask, Task, TaskId};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CoreError {
    #[error(
        "unknown task status {value:?}; expected one of backlog, ready, running, review, blocked, needs-human, done, cancelled"
    )]
    UnknownStatus { value: String },

    #[error("unknown permission level {value:?}; expected read, write-workdir or write-shell")]
    UnknownPermissions { value: String },

    #[error("unknown review verdict {value:?}; expected PASS, CHANGES or ESCALATE")]
    UnknownVerdict { value: String },

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

    #[error("review profile {profile:?} does not match any profile")]
    UnknownReviewProfile { profile: String },

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

    #[error("profile {profile:?} references invalid skill name {skill:?}; must match [a-z0-9-]+")]
    InvalidSkillName { profile: String, skill: String },

    #[error("learning config references invalid skill name {skill:?}; must match [a-z0-9-]+")]
    InvalidLearningSkill { skill: String },

    #[error("learning profile {profile:?} does not match any profile")]
    UnknownLearningProfile { profile: String },

    #[error("escalation path glob {glob:?} is invalid: {message}")]
    InvalidEscalationGlob { glob: String, message: String },

    #[error("source kind {kind:?} is invalid; expected 'speckit'")]
    InvalidSourceKind { kind: String },

    #[error("source path must not be empty")]
    EmptySourcePath,
}
