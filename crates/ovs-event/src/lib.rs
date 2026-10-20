//! Event schema shared by every `ovs` subcommand.

mod event;
mod id;
mod snippet;
mod timestamp;

pub use event::{CallId, Host, SchemaVersion, SessionKey, Source, SourceRef, UnsupportedVersion};
pub use id::{EventId, EventIdError};
pub use snippet::{SNIPPET_MAX_BYTES, Snippet};
pub use timestamp::{Timestamp, TimestampError};
