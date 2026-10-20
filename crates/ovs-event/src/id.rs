use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Host, Source};

const ID_HEX_LEN: usize = 32;
const FIELD_SEPARATOR: u8 = 0x1f;

/// Deterministic event id: re-reading a source yields the same id, so resends deduplicate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EventId(String);

#[derive(Debug, thiserror::Error)]
#[error("event id {value:?} is not 32 lowercase hex characters")]
pub struct EventIdError {
    value: String,
}

impl EventId {
    pub fn derive(host: &Host, source: Source, native_session: &str, position: &str) -> Self {
        let mut hasher = Sha256::new();
        for part in [host.as_str(), source.as_str(), native_session, position] {
            hasher.update(part.as_bytes());
            hasher.update([FIELD_SEPARATOR]);
        }
        let digest = hasher.finalize();
        Self(
            digest
                .iter()
                .take(ID_HEX_LEN / 2)
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        )
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for EventId {
    type Error = EventIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let valid = value.len() == ID_HEX_LEN
            && value
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c));
        if valid {
            Ok(Self(value))
        } else {
            Err(EventIdError { value })
        }
    }
}

impl From<EventId> for String {
    fn from(id: EventId) -> Self {
        id.0
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;
    use crate::{Host, Source};

    #[test]
    fn derive_is_stable_and_32_hex() {
        let host = Host::new("ali-desktop");
        let a = EventId::derive(&host, Source::ClaudeCode, "s1", "0:0");
        let b = EventId::derive(&host, Source::ClaudeCode, "s1", "0:0");
        assert_eq!(a, b);
        assert_eq!(a.as_str().len(), 32);
        assert!(
            a.as_str()
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        );
    }

    #[test]
    fn derive_matches_known_vector() {
        let id = EventId::derive(&Host::new("ali-desktop"), Source::ClaudeCode, "s1", "0:0");
        assert_eq!(id.as_str(), "e5b55fb92b09f5148a091eb5453192ab");
    }

    #[test]
    fn derive_differs_by_position_and_host() {
        let host = Host::new("ali-desktop");
        let base = EventId::derive(&host, Source::ClaudeCode, "s1", "0:0");
        assert_ne!(
            base,
            EventId::derive(&host, Source::ClaudeCode, "s1", "0:1")
        );
        assert_ne!(
            base,
            EventId::derive(&Host::new("laptop"), Source::ClaudeCode, "s1", "0:0")
        );
    }

    #[test]
    fn rejects_malformed_ids() {
        assert!(EventId::try_from("XYZ".to_owned()).is_err());
        assert!(EventId::try_from("0123456789ABCDEF0123456789ABCDEF".to_owned()).is_err());
        assert!(EventId::try_from("0123456789abcdef0123456789abcdef".to_owned()).is_ok());
    }
}
