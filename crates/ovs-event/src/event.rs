use std::fmt;

use serde::{Deserialize, Serialize};

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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

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
}
