//! Event schema shared by every `ovs` subcommand.

mod body;
mod event;
mod id;
mod snippet;
mod timestamp;

pub use body::{
    ActivityState, Body, CollectorStatus, EndReason, ErrorClass, PromptBy, SessionStarted,
    Severity, Usage,
};
pub use event::{
    CallId, Event, EventError, Host, SchemaVersion, SessionKey, Source, SourceRef,
    UnsupportedVersion,
};
pub use id::{EventId, EventIdError};
pub use snippet::{SNIPPET_MAX_BYTES, Snippet};
pub use timestamp::{Timestamp, TimestampError};
