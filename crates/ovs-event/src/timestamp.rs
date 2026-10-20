use std::fmt;
use std::time::SystemTime;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

/// A UTC instant, written as RFC 3339 with a `Z` offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(OffsetDateTime);

#[derive(Debug, thiserror::Error)]
pub enum TimestampError {
    #[error("invalid RFC 3339 timestamp {value:?}")]
    Parse {
        value: String,
        #[source]
        source: time::error::Parse,
    },
    #[error("epoch milliseconds {value} are out of range")]
    EpochRange { value: i64 },
}

impl Timestamp {
    pub fn now() -> Self {
        Self(OffsetDateTime::now_utc())
    }

    pub fn parse(value: &str) -> Result<Self, TimestampError> {
        OffsetDateTime::parse(value, &Rfc3339)
            .map(|instant| Self(instant.to_offset(UtcOffset::UTC)))
            .map_err(|source| TimestampError::Parse {
                value: value.to_owned(),
                source,
            })
    }

    pub fn from_epoch_millis(value: i64) -> Result<Self, TimestampError> {
        let nanos = i128::from(value)
            .checked_mul(1_000_000)
            .ok_or(TimestampError::EpochRange { value })?;
        OffsetDateTime::from_unix_timestamp_nanos(nanos)
            .map(Self)
            .map_err(|_| TimestampError::EpochRange { value })
    }

    pub fn to_system_time(self) -> SystemTime {
        self.0.into()
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = self.0.format(&Rfc3339).map_err(|_| fmt::Error)?;
        f.write_str(&text)
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn writes_utc_with_millis() {
        let ts = Timestamp::parse("2026-10-20T05:33:12.123Z").unwrap();
        assert_eq!(ts.to_string(), "2026-10-20T05:33:12.123Z");
    }

    #[test]
    fn normalises_offsets_to_utc() {
        let ts = Timestamp::parse("2026-10-20T07:33:12+02:00").unwrap();
        assert_eq!(ts.to_string(), "2026-10-20T05:33:12Z");
    }

    #[test]
    fn converts_epoch_millis() {
        let ts = Timestamp::from_epoch_millis(1_792_474_392_123).unwrap();
        assert_eq!(ts.to_string(), "2026-10-20T05:33:12.123Z");
    }

    #[test]
    fn rejects_text_that_is_not_rfc3339() {
        assert!(Timestamp::parse("yesterday").is_err());
    }

    #[test]
    fn round_trips_through_json() {
        let ts = Timestamp::parse("2026-10-20T05:33:12.123Z").unwrap();
        let json = serde_json::to_string(&ts).unwrap();
        assert_eq!(json, "\"2026-10-20T05:33:12.123Z\"");
        assert_eq!(serde_json::from_str::<Timestamp>(&json).unwrap(), ts);
    }
}
