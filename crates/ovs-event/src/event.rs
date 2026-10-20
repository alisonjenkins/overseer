use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{Body, EventId, Timestamp};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    ClaudeCode,
    Opencode,
    Agy,
    Ovs,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Opencode => "opencode",
            Self::Agy => "agy",
            Self::Ovs => "ovs",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Host(String);

impl Host {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `<source>:<native session id>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionKey(String);

impl SessionKey {
    pub fn new(source: Source, native_id: &str) -> Self {
        Self(format!("{}:{native_id}", source.as_str()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CallId(String);

impl CallId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Where the full record lives on the origin machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    pub host: Host,
    pub path: String,
    pub offset: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct SchemaVersion;

impl SchemaVersion {
    pub const CURRENT: u8 = 1;
}

#[derive(Debug, thiserror::Error)]
#[error("unsupported schema version {found}; this build reads version 1")]
pub struct UnsupportedVersion {
    pub found: u8,
}

impl TryFrom<u8> for SchemaVersion {
    type Error = UnsupportedVersion;

    fn try_from(found: u8) -> Result<Self, Self::Error> {
        if found == Self::CURRENT {
            Ok(Self)
        } else {
            Err(UnsupportedVersion { found })
        }
    }
}

impl From<SchemaVersion> for u8 {
    fn from(_: SchemaVersion) -> Self {
        SchemaVersion::CURRENT
    }
}

/// One line of the event stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub v: SchemaVersion,
    pub id: EventId,
    pub ts: Timestamp,
    pub host: Host,
    pub source: Source,
    pub session: SessionKey,
    pub parent: Option<SessionKey>,
    #[serde(flatten)]
    pub body: Body,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<SourceRef>,
}

#[derive(Debug, thiserror::Error)]
pub enum EventError {
    #[error("failed to encode event")]
    Encode {
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to decode event")]
    Decode {
        #[source]
        source: serde_json::Error,
    },
}

impl Event {
    pub fn to_line(&self) -> Result<String, EventError> {
        serde_json::to_string(self).map_err(|source| EventError::Encode { source })
    }

    pub fn from_line(line: &str) -> Result<Self, EventError> {
        serde_json::from_str(line).map_err(|source| EventError::Decode { source })
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::unreachable
    )]

    use super::*;

    #[test]
    fn session_key_prefixes_source() {
        assert_eq!(
            SessionKey::new(Source::ClaudeCode, "6b3b").as_str(),
            "claude-code:6b3b"
        );
        assert_eq!(
            SessionKey::new(Source::Opencode, "ses_1").as_str(),
            "opencode:ses_1"
        );
    }

    #[test]
    fn source_serialises_kebab_case() {
        assert_eq!(
            serde_json::to_string(&Source::ClaudeCode).unwrap(),
            "\"claude-code\""
        );
        assert_eq!(
            serde_json::from_str::<Source>("\"agy\"").unwrap(),
            Source::Agy
        );
    }

    #[test]
    fn schema_version_accepts_only_one() {
        assert_eq!(serde_json::to_string(&SchemaVersion).unwrap(), "1");
        assert!(serde_json::from_str::<SchemaVersion>("1").is_ok());
        let err = serde_json::from_str::<SchemaVersion>("2").unwrap_err();
        assert!(err.to_string().contains("unsupported schema version 2"));
    }

    const SPEC_EXAMPLE: &str = r#"{"v":1,"id":"0123456789abcdef0123456789abcdef","ts":"2026-10-20T05:33:12.123Z","host":"ali-desktop","source":"claude-code","session":"claude-code:6b3bd7d6","parent":null,"kind":"tool.called","data":{"call_id":"toolu_01","name":"Bash","input":"cargo test"}}"#;

    #[test]
    fn spec_example_round_trips_byte_for_byte() {
        let event = Event::from_line(SPEC_EXAMPLE).unwrap();
        assert_eq!(event.body.kind(), "tool.called");
        assert_eq!(event.to_line().unwrap(), SPEC_EXAMPLE);
    }

    #[test]
    fn ref_is_written_only_when_present() {
        let mut event = Event::from_line(SPEC_EXAMPLE).unwrap();
        event.source_ref = Some(SourceRef {
            host: Host::new("ali-desktop"),
            path: "/home/ali/.claude/projects/p/s.jsonl".to_owned(),
            offset: 42,
        });
        let line = event.to_line().unwrap();
        assert!(line.ends_with(r#""ref":{"host":"ali-desktop","path":"/home/ali/.claude/projects/p/s.jsonl","offset":42}}"#));
        assert_eq!(Event::from_line(&line).unwrap(), event);
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let line = SPEC_EXAMPLE.replace("tool.called", "tool.exploded");
        assert!(Event::from_line(&line).is_err());
    }

    #[test]
    fn other_schema_versions_are_rejected() {
        let line = SPEC_EXAMPLE.replace(r#""v":1"#, r#""v":2"#);
        let err = Event::from_line(&line).unwrap_err();
        let cause = std::error::Error::source(&err).unwrap().to_string();
        assert!(cause.contains("unsupported schema version 2"), "{cause}");
    }

    #[test]
    fn decoded_text_obeys_the_snippet_policy() {
        let line = SPEC_EXAMPLE.replace("cargo test", &"a".repeat(3000));
        let event = Event::from_line(&line).unwrap();
        let Body::ToolCalled { input, .. } = event.body else {
            unreachable!("fixture is a tool.called event")
        };
        assert!(input.as_str().len() <= crate::SNIPPET_MAX_BYTES);
    }
}
