# Agent monitor M1: event schema, fold, `ovs collect claude` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `ovs collect claude | jq` prints every Claude Code session on this machine as normalised NDJSON events and resumes from a cursor after a restart.

**Architecture:** Four new crates in the existing workspace. `ovs-event` is the schema (the contract for every later milestone), `ovs-state` folds events into per-session state, `ovs-collect-claude` reads `~/.claude/projects/**.jsonl` and `~/.claude/sessions/*.json`, and `ovs-cli` builds the `ovs` binary. The existing `overseer-*` crates are not touched.

**Tech Stack:** Rust 2024 edition, serde/serde_json, sha2, regex, time, thiserror, clap, anyhow, tracing; tests with tempfile and assert_cmd. Toolchain from `nix develop`.

**Spec:** `docs/superpowers/specs/2026-10-20-agent-monitor-design.md` (sections 3, 4, 5, 8, 9; milestone 1 of section 10).

## Global Constraints

- Run cargo through the dev shell: `nix develop -c cargo …`.
- Workspace lints apply to every new crate (`[lints] workspace = true`): `unwrap_used`, `expect_used`, `panic`, `indexing_slicing`, `arithmetic_side_effects`, `unreachable`, `todo` and others are `deny`. Use `.get()`, `checked_*`/`saturating_*`. Test modules start with `#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]`.
- Done bar for every task: `nix develop -c cargo test --workspace`, `nix develop -c cargo clippy --workspace --all-targets -- -D warnings`, `nix develop -c cargo fmt --check` all pass.
- Event schema version is `1`. Text fields are truncated to 2048 bytes (`SNIPPET_MAX_BYTES`), cut on a char boundary, ending in `…`.
- Times are ISO8601 UTC (`2026-10-20T05:33:12.123Z`).
- `source` values: `claude-code`, `opencode`, `agy`, `ovs`. Session keys: `<source>:<native id>`.
- Event `id`: first 32 lowercase hex characters of SHA-256 over `host`, `source`, native session id, position, each followed by byte `0x1f`.
- Collectors redact: `Authorization:` header values, `sk-…` keys, GitHub tokens (`ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_`), AWS `AKIA…` ids, Google `AIza…` keys, and values of `*TOKEN*`, `*KEY*`, `*SECRET*`, `*PASSWORD*` uppercase assignments.
- Commits: Conventional Commits, one task per commit, no AI attribution lines.
- No comments that restate code; one short line only where the why is not obvious.

## File structure

```
Cargo.toml                                   (modify: add 4 members)
flake.nix                                    (modify: build ovs-cli too)
README.md                                    (modify: ovs section)
crates/ovs-event/
  Cargo.toml
  src/lib.rs          re-exports
  src/timestamp.rs    Timestamp (UTC, RFC 3339)
  src/id.rs           EventId (derive + validate)
  src/event.rs        Source, Host, SessionKey, CallId, SourceRef, SchemaVersion, Event
  src/snippet.rs      Snippet (redact + truncate)
  src/body.rs         Body (kind + data) and its payload types
crates/ovs-state/
  Cargo.toml
  src/lib.rs          State, SessionState, fold, apply_all
crates/ovs-collect-claude/
  Cargo.toml
  src/lib.rs          re-exports
  src/record.rs       one transcript line -> events
  src/registry.rs     ~/.claude/sessions/*.json -> status/ended events
  src/cursor.rs       persisted offsets + live sessions
  src/collector.rs    discovery, tailing, poll loop body
crates/ovs-cli/
  Cargo.toml
  src/main.rs         `ovs collect claude`
  tests/collect_claude.rs
```

---

### Task 1: `ovs-event` identifiers and timestamps

**Files:**
- Modify: `Cargo.toml` (workspace members)
- Create: `crates/ovs-event/Cargo.toml`, `crates/ovs-event/src/lib.rs`, `crates/ovs-event/src/timestamp.rs`, `crates/ovs-event/src/id.rs`, `crates/ovs-event/src/event.rs`

**Interfaces:**
- Produces:
  - `Timestamp::{now() -> Timestamp, parse(&str) -> Result<Timestamp, TimestampError>, from_epoch_millis(i64) -> Result<Timestamp, TimestampError>, to_system_time(self) -> SystemTime}`, `Display`, serde as RFC 3339 string, `Ord`.
  - `Source::{ClaudeCode, Opencode, Agy, Ovs}`, `Source::as_str(self) -> &'static str`, serde kebab-case.
  - `Host::new(impl Into<String>)`, `Host::as_str(&self) -> &str`.
  - `SessionKey::new(Source, &str)`, `SessionKey::as_str(&self) -> &str`.
  - `CallId::new(impl Into<String>)`, `CallId::as_str(&self) -> &str`.
  - `SourceRef { host: Host, path: String, offset: u64 }`.
  - `SchemaVersion` (unit struct, serialises as `1`, rejects other numbers with `UnsupportedVersion { found: u8 }`).
  - `EventId::derive(&Host, Source, &str, &str) -> EventId`, `EventId::as_str`, `TryFrom<String>` with `EventIdError`.

- [ ] **Step 1: Add the crate to the workspace**

In `Cargo.toml`, change the `members` list to:

```toml
members = [
    "crates/overseer-core",
    "crates/overseer-store",
    "crates/overseer-backends",
    "crates/overseer-orchestrator",
    "crates/overseer-cli",
    "crates/ovs-event",
]
```

Create `crates/ovs-event/Cargo.toml`:

```toml
[package]
name = "ovs-event"
description = "Event schema shared by every ovs subcommand"
version.workspace = true
edition.workspace = true
publish.workspace = true
license.workspace = true

[dependencies]
regex = "1.13.1"
serde = { version = "1.0.228", features = ["derive"] }
serde_json = "1.0.154"
sha2 = "0.10.9"
thiserror = "2"
time = { version = "0.3.55", features = ["formatting", "parsing"] }

[lints]
workspace = true
```

Create `crates/ovs-event/src/lib.rs`:

```rust
//! Event schema shared by every `ovs` subcommand.

mod event;
mod id;
mod timestamp;

pub use event::{CallId, Host, SchemaVersion, SessionKey, Source, SourceRef, UnsupportedVersion};
pub use id::{EventId, EventIdError};
pub use timestamp::{Timestamp, TimestampError};
```

- [ ] **Step 2: Write the failing tests**

Create `crates/ovs-event/src/timestamp.rs` with only the tests for now:

```rust
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
```

Create `crates/ovs-event/src/id.rs` with only tests:

```rust
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
        assert!(a.as_str().bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)));
    }

    #[test]
    fn derive_differs_by_position_and_host() {
        let host = Host::new("ali-desktop");
        let base = EventId::derive(&host, Source::ClaudeCode, "s1", "0:0");
        assert_ne!(base, EventId::derive(&host, Source::ClaudeCode, "s1", "0:1"));
        assert_ne!(base, EventId::derive(&Host::new("laptop"), Source::ClaudeCode, "s1", "0:0"));
    }

    #[test]
    fn rejects_malformed_ids() {
        assert!(EventId::try_from("XYZ".to_owned()).is_err());
        assert!(EventId::try_from("0123456789ABCDEF0123456789ABCDEF".to_owned()).is_err());
        assert!(EventId::try_from("0123456789abcdef0123456789abcdef".to_owned()).is_ok());
    }
}
```

Create `crates/ovs-event/src/event.rs` with only tests:

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn session_key_prefixes_source() {
        assert_eq!(SessionKey::new(Source::ClaudeCode, "6b3b").as_str(), "claude-code:6b3b");
        assert_eq!(SessionKey::new(Source::Opencode, "ses_1").as_str(), "opencode:ses_1");
    }

    #[test]
    fn source_serialises_kebab_case() {
        assert_eq!(serde_json::to_string(&Source::ClaudeCode).unwrap(), "\"claude-code\"");
        assert_eq!(serde_json::from_str::<Source>("\"agy\"").unwrap(), Source::Agy);
    }

    #[test]
    fn schema_version_accepts_only_one() {
        assert_eq!(serde_json::to_string(&SchemaVersion).unwrap(), "1");
        assert!(serde_json::from_str::<SchemaVersion>("1").is_ok());
        let err = serde_json::from_str::<SchemaVersion>("2").unwrap_err();
        assert!(err.to_string().contains("unsupported schema version 2"));
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `nix develop -c cargo test -p ovs-event`
Expected: compile errors, `cannot find type Timestamp`, `EventId`, `SessionKey` etc.

- [ ] **Step 4: Implement**

Prepend to `crates/ovs-event/src/timestamp.rs` (above the tests):

```rust
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
```

Prepend to `crates/ovs-event/src/id.rs`:

```rust
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
```

Prepend to `crates/ovs-event/src/event.rs`:

```rust
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
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `nix develop -c cargo test -p ovs-event`
Expected: 11 passed.

Run: `nix develop -c cargo clippy -p ovs-event --all-targets -- -D warnings && nix develop -c cargo fmt --check`
Expected: no warnings, no diff.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/ovs-event
git commit -m "feat(ovs-event): add event ids, timestamps and identifiers"
```

---

### Task 2: `Snippet` redaction and truncation

**Files:**
- Create: `crates/ovs-event/src/snippet.rs`
- Modify: `crates/ovs-event/src/lib.rs`

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces: `Snippet::new(&str) -> Snippet` (redacts then truncates), `Snippet::as_str(&self) -> &str`, `SNIPPET_MAX_BYTES: usize = 2048`, serde as string, deserialising applies the same policy.

- [ ] **Step 1: Write the failing tests**

Create `crates/ovs-event/src/snippet.rs`:

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn every_rule_compiles() {
        assert_eq!(COMPILED.len(), RULES.len());
    }

    #[test]
    fn redacts_known_secret_shapes() {
        let cases = [
            ("API_KEY=abc123", "API_KEY=[redacted]"),
            ("export GITHUB_TOKEN='a b'", "export GITHUB_TOKEN=[redacted]"),
            ("DB_PASSWORD = hunter2 rest", "DB_PASSWORD = [redacted] rest"),
            ("Authorization: Bearer abc.def", "Authorization: [redacted]"),
            ("key sk-ant-api03-AAAAAAAAAAAAAAAAAAAA end", "key [redacted] end"),
            ("ghp_abcdefghijklmnopqrstuvwxyz0123456789", "[redacted]"),
            ("id AKIAABCDEFGHIJKLMNOP", "id [redacted]"),
            ("AIzaSyA-abcdefghijklmnopqrstuvwxyz01234", "[redacted]"),
        ];
        for (raw, expected) in cases {
            assert_eq!(Snippet::new(raw).as_str(), expected, "input {raw:?}");
        }
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        let text = "cargo test --workspace; input_tokens: 2";
        assert_eq!(Snippet::new(text).as_str(), text);
    }

    #[test]
    fn truncates_on_a_char_boundary() {
        let long = "é".repeat(2000);
        let snippet = Snippet::new(&long);
        assert!(snippet.as_str().len() <= SNIPPET_MAX_BYTES);
        assert!(snippet.as_str().ends_with('…'));
    }

    #[test]
    fn policy_is_idempotent() {
        let once = Snippet::new(&format!("API_KEY=x {}", "a".repeat(5000)));
        let twice = Snippet::new(once.as_str());
        assert_eq!(once, twice);
    }

    #[test]
    fn deserialising_applies_the_policy() {
        let json = format!("\"TOKEN=abc {}\"", "b".repeat(4000));
        let snippet: Snippet = serde_json::from_str(&json).unwrap();
        assert!(snippet.as_str().starts_with("TOKEN=[redacted]"));
        assert!(snippet.as_str().len() <= SNIPPET_MAX_BYTES);
    }
}
```

Change `crates/ovs-event/src/lib.rs` to:

```rust
//! Event schema shared by every `ovs` subcommand.

mod event;
mod id;
mod snippet;
mod timestamp;

pub use event::{CallId, Host, SchemaVersion, SessionKey, Source, SourceRef, UnsupportedVersion};
pub use id::{EventId, EventIdError};
pub use snippet::{SNIPPET_MAX_BYTES, Snippet};
pub use timestamp::{Timestamp, TimestampError};
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `nix develop -c cargo test -p ovs-event snippet`
Expected: compile error, `cannot find type Snippet`.

- [ ] **Step 3: Implement**

Prepend to `crates/ovs-event/src/snippet.rs`:

```rust
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

pub const SNIPPET_MAX_BYTES: usize = 2048;
const ELLIPSIS: char = '…';

struct Rule {
    pattern: &'static str,
    replacement: &'static str,
}

const RULES: &[Rule] = &[
    Rule {
        pattern: r"(?i)(authorization:\s*)(?:bearer\s+|basic\s+)?\S+",
        replacement: "${1}[redacted]",
    },
    Rule {
        pattern: r"\bsk-[A-Za-z0-9_-]{16,}",
        replacement: "[redacted]",
    },
    Rule {
        pattern: r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{30,}",
        replacement: "[redacted]",
    },
    Rule {
        pattern: r"\bgithub_pat_[A-Za-z0-9_]{30,}",
        replacement: "[redacted]",
    },
    Rule {
        pattern: r"\bAKIA[0-9A-Z]{16}\b",
        replacement: "[redacted]",
    },
    Rule {
        pattern: r"\bAIza[0-9A-Za-z_-]{35}",
        replacement: "[redacted]",
    },
    Rule {
        pattern: r#"\b([A-Z0-9_]*(?:TOKEN|KEY|SECRET|PASSWORD)[A-Z0-9_]*)(\s*=\s*)("[^"]*"|'[^']*'|\S+)"#,
        replacement: "${1}${2}[redacted]",
    },
];

// A pattern that fails to compile is dropped here and caught by the `every_rule_compiles` test.
static COMPILED: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    RULES
        .iter()
        .filter_map(|rule| Regex::new(rule.pattern).ok().map(|re| (re, rule.replacement)))
        .collect()
});

/// Agent text after redaction, at most `SNIPPET_MAX_BYTES` long.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub struct Snippet(String);

impl Snippet {
    pub fn new(raw: &str) -> Self {
        Self(truncate(&redact(raw)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for Snippet {
    fn from(raw: String) -> Self {
        Self::new(&raw)
    }
}

impl From<Snippet> for String {
    fn from(snippet: Snippet) -> Self {
        snippet.0
    }
}

fn redact(text: &str) -> String {
    COMPILED
        .iter()
        .fold(text.to_owned(), |acc, (re, replacement)| {
            re.replace_all(&acc, *replacement).into_owned()
        })
}

fn truncate(text: &str) -> String {
    if text.len() <= SNIPPET_MAX_BYTES {
        return text.to_owned();
    }
    let mut end = SNIPPET_MAX_BYTES.saturating_sub(ELLIPSIS.len_utf8());
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let mut out = text.get(..end).unwrap_or_default().to_owned();
    out.push(ELLIPSIS);
    out
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `nix develop -c cargo test -p ovs-event`
Expected: 17 passed.

- [ ] **Step 5: Commit**

```bash
git add Cargo.lock crates/ovs-event
git commit -m "feat(ovs-event): redact and truncate agent text in snippets"
```

---

### Task 3: `Body` and `Event` wire format

**Files:**
- Create: `crates/ovs-event/src/body.rs`
- Modify: `crates/ovs-event/src/event.rs`, `crates/ovs-event/src/lib.rs`

**Interfaces:**
- Consumes: Task 1 types, `Snippet` from Task 2.
- Produces:
  - `Body` enum (serde adjacently tagged `kind`/`data`): `SessionStarted(SessionStarted)`, `SessionEnded { reason: EndReason }`, `SessionStatus { state: ActivityState }`, `Prompt { text: Snippet, by: PromptBy }`, `Message { text: Snippet, model: Option<String> }`, `ToolCalled { call_id: CallId, name: String, input: Snippet }`, `ToolResult { call_id: CallId, ok: bool, error: Option<Snippet>, output: Snippet }`, `Usage(Usage)`, `Error { class: ErrorClass, message: Snippet }`, `HostHeartbeat { collectors: Vec<CollectorStatus> }`, `AlertRaised { alert_id: String, rule: String, severity: Severity, target: SessionKey, message: Snippet }`, `AlertCleared { alert_id: String }`. `Body::kind(&self) -> &'static str`.
  - `SessionStarted { cwd, git_branch, title, agent: Option<String>, pid: Option<u32>, model: Option<String> }` (`Default`).
  - `Usage { model: Option<String>, input, output, cache_read, cache_write, reasoning: u64, cost_usd: Option<f64> }` (`Default`).
  - `EndReason::{Exited, Archived, Killed, Stale}`, `ActivityState::{Working, Idle, WaitingInput, WaitingPermission}`, `PromptBy::{User, Agent}`, `ErrorClass::{RateLimit, Api, ContextOverflow, Aborted, Parse, Other}`, `Severity::{High, Normal}`, `CollectorStatus { name: String, lag: u64 }`; all snake_case on the wire.
  - `Event { v: SchemaVersion, id: EventId, ts: Timestamp, host: Host, source: Source, session: SessionKey, parent: Option<SessionKey>, body: Body, source_ref: Option<SourceRef> }`, `Event::to_line(&self) -> Result<String, EventError>`, `Event::from_line(&str) -> Result<Event, EventError>`.

- [ ] **Step 1: Write the failing tests**

Create `crates/ovs-event/src/body.rs` with only tests:

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn kind_names_match_the_wire() {
        assert_eq!(Body::Usage(Usage::default()).kind(), "usage");
        assert_eq!(
            Body::SessionStatus { state: ActivityState::WaitingPermission }.kind(),
            "session.status"
        );
        let json = serde_json::to_string(&Body::SessionStatus {
            state: ActivityState::WaitingPermission,
        })
        .unwrap();
        assert_eq!(json, r#"{"kind":"session.status","data":{"state":"waiting_permission"}}"#);
    }
}
```

Append to the tests module in `crates/ovs-event/src/event.rs`:

```rust
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
```

Note: `unreachable!` is denied by the workspace lints. Add `clippy::unreachable` to the test module's `#![allow(...)]` list in `event.rs`:

```rust
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::unreachable)]
```

Change `crates/ovs-event/src/lib.rs` to:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `nix develop -c cargo test -p ovs-event`
Expected: compile errors, `cannot find type Body`, `Event`.

- [ ] **Step 3: Implement**

Prepend to `crates/ovs-event/src/body.rs`:

```rust
use serde::{Deserialize, Serialize};

use crate::{CallId, SessionKey, Snippet};

/// The `kind` and `data` of an event. New kinds bump the schema version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum Body {
    #[serde(rename = "session.started")]
    SessionStarted(SessionStarted),
    #[serde(rename = "session.ended")]
    SessionEnded { reason: EndReason },
    #[serde(rename = "session.status")]
    SessionStatus { state: ActivityState },
    #[serde(rename = "prompt")]
    Prompt { text: Snippet, by: PromptBy },
    #[serde(rename = "message")]
    Message { text: Snippet, model: Option<String> },
    #[serde(rename = "tool.called")]
    ToolCalled {
        call_id: CallId,
        name: String,
        input: Snippet,
    },
    #[serde(rename = "tool.result")]
    ToolResult {
        call_id: CallId,
        ok: bool,
        error: Option<Snippet>,
        output: Snippet,
    },
    #[serde(rename = "usage")]
    Usage(Usage),
    #[serde(rename = "error")]
    Error { class: ErrorClass, message: Snippet },
    #[serde(rename = "host.heartbeat")]
    HostHeartbeat { collectors: Vec<CollectorStatus> },
    #[serde(rename = "alert.raised")]
    AlertRaised {
        alert_id: String,
        rule: String,
        severity: Severity,
        target: SessionKey,
        message: Snippet,
    },
    #[serde(rename = "alert.cleared")]
    AlertCleared { alert_id: String },
}

impl Body {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SessionStarted(_) => "session.started",
            Self::SessionEnded { .. } => "session.ended",
            Self::SessionStatus { .. } => "session.status",
            Self::Prompt { .. } => "prompt",
            Self::Message { .. } => "message",
            Self::ToolCalled { .. } => "tool.called",
            Self::ToolResult { .. } => "tool.result",
            Self::Usage(_) => "usage",
            Self::Error { .. } => "error",
            Self::HostHeartbeat { .. } => "host.heartbeat",
            Self::AlertRaised { .. } => "alert.raised",
            Self::AlertCleared { .. } => "alert.cleared",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStarted {
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub title: Option<String>,
    pub agent: Option<String>,
    pub pid: Option<u32>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub model: Option<String>,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    Exited,
    Archived,
    Killed,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityState {
    Working,
    Idle,
    WaitingInput,
    WaitingPermission,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptBy {
    User,
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    RateLimit,
    Api,
    ContextOverflow,
    Aborted,
    Parse,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    High,
    Normal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectorStatus {
    pub name: String,
    pub lag: u64,
}
```

In `crates/ovs-event/src/event.rs`, change the `use` block at the top to:

```rust
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{Body, EventId, Timestamp};
```

and append, above `#[cfg(test)]`:

```rust
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `nix develop -c cargo test -p ovs-event`
Expected: 23 passed.

Run: `nix develop -c cargo clippy -p ovs-event --all-targets -- -D warnings && nix develop -c cargo fmt --check`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/ovs-event
git commit -m "feat(ovs-event): define event kinds and the NDJSON wire format"
```

---

### Task 4: `ovs-state` session fold

**Files:**
- Modify: `Cargo.toml` (add member `crates/ovs-state`)
- Create: `crates/ovs-state/Cargo.toml`, `crates/ovs-state/src/lib.rs`

**Interfaces:**
- Consumes: `ovs_event::{Event, Body, ActivityState, Usage, Snippet, SessionKey, Source, Host, Timestamp, CallId}`.
- Produces:
  - `State::{Working, Idle, WaitingInput, WaitingPermission, Errored, Ended}` (serde snake_case).
  - `SessionState { session, source, host, parent, state, cwd, git_branch, title, model, started_at, last_event_at, activity: Option<Snippet>, open_calls: BTreeMap<CallId, OpenCall>, usage: UsageTotals, errors: u64 }`.
  - `OpenCall { name: String, since: Timestamp }`, `UsageTotals { input, output, cache_read, cache_write, reasoning: u64, cost_usd: Option<f64> }`.
  - `fold(Option<SessionState>, &Event) -> Option<SessionState>` (returns `prev` unchanged for host and alert events).
  - `apply_all<'a>(impl IntoIterator<Item = &'a Event>) -> BTreeMap<SessionKey, SessionState>`.
  - `QUESTION_TOOLS: &[&str] = &["AskUserQuestion", "question"]`.
- Time-driven transitions (permission heuristic after 30 s, `ended` after 24 h idle) are out of scope here; they belong to the alert milestone.

- [ ] **Step 1: Scaffold the crate and write the failing tests**

Add `"crates/ovs-state",` to the workspace `members` after `"crates/ovs-event",`.

Create `crates/ovs-state/Cargo.toml`:

```toml
[package]
name = "ovs-state"
description = "Folds ovs events into per-session state"
version.workspace = true
edition.workspace = true
publish.workspace = true
license.workspace = true

[dependencies]
ovs-event = { version = "0.1.0", path = "../ovs-event" }
serde = { version = "1.0.228", features = ["derive"] }

[lints]
workspace = true
```

Create `crates/ovs-state/src/lib.rs` with only tests:

```rust
//! Session state derived from the event log.

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use ovs_event::{
        ActivityState, Body, CallId, EndReason, ErrorClass, Event, EventId, Host, PromptBy,
        SchemaVersion, SessionKey, SessionStarted, Snippet, Source, Timestamp, Usage,
    };

    use super::*;

    fn ev(session: &str, millis: i64, body: Body) -> Event {
        let host = Host::new("ali-desktop");
        Event {
            v: SchemaVersion,
            id: EventId::derive(&host, Source::ClaudeCode, session, &millis.to_string()),
            ts: Timestamp::from_epoch_millis(millis).unwrap(),
            host,
            source: Source::ClaudeCode,
            session: SessionKey::new(Source::ClaudeCode, session),
            parent: None,
            body,
            source_ref: None,
        }
    }

    fn prompt(text: &str) -> Body {
        Body::Prompt { text: Snippet::new(text), by: PromptBy::User }
    }

    fn called(id: &str, name: &str) -> Body {
        Body::ToolCalled { call_id: CallId::new(id), name: name.to_owned(), input: Snippet::new("{}") }
    }

    fn result(id: &str) -> Body {
        Body::ToolResult { call_id: CallId::new(id), ok: true, error: None, output: Snippet::new("ok") }
    }

    #[test]
    fn first_prompt_creates_a_working_session() {
        let s = fold(None, &ev("s1", 1_000, prompt("fix it"))).unwrap();
        assert_eq!(s.state, State::Working);
        assert_eq!(s.activity.unwrap().as_str(), "fix it");
        assert_eq!(s.session.as_str(), "claude-code:s1");
        assert_eq!(s.started_at, Timestamp::from_epoch_millis(1_000).unwrap());
    }

    #[test]
    fn started_fills_metadata() {
        let start = Body::SessionStarted(SessionStarted {
            cwd: Some("/tmp/p".to_owned()),
            git_branch: Some("main".to_owned()),
            ..SessionStarted::default()
        });
        let s = fold(None, &ev("s1", 1_000, start)).unwrap();
        assert_eq!(s.state, State::Idle);
        assert_eq!(s.cwd.as_deref(), Some("/tmp/p"));
        assert_eq!(s.git_branch.as_deref(), Some("main"));
    }

    #[test]
    fn open_question_turns_idle_into_waiting_input() {
        let events = [
            ev("s1", 1_000, prompt("go")),
            ev("s1", 2_000, called("t1", "AskUserQuestion")),
            ev("s1", 3_000, Body::SessionStatus { state: ActivityState::Idle }),
        ];
        let s = apply_all(&events).remove(&SessionKey::new(Source::ClaudeCode, "s1")).unwrap();
        assert_eq!(s.state, State::WaitingInput);
        let s = fold(Some(s), &ev("s1", 4_000, result("t1"))).unwrap();
        assert_eq!(s.state, State::Working);
        assert!(s.open_calls.is_empty());
    }

    #[test]
    fn idle_without_open_question_is_idle() {
        let events = [
            ev("s1", 1_000, called("t1", "Bash")),
            ev("s1", 2_000, result("t1")),
            ev("s1", 3_000, Body::SessionStatus { state: ActivityState::Idle }),
        ];
        let s = apply_all(&events).remove(&SessionKey::new(Source::ClaudeCode, "s1")).unwrap();
        assert_eq!(s.state, State::Idle);
    }

    #[test]
    fn hook_status_sets_waiting_permission() {
        let s = fold(None, &ev("s1", 1_000, Body::SessionStatus { state: ActivityState::WaitingPermission })).unwrap();
        assert_eq!(s.state, State::WaitingPermission);
    }

    #[test]
    fn usage_accumulates_and_sets_model() {
        let usage = |input, cost| Body::Usage(Usage {
            model: Some("claude-opus-5-5".to_owned()),
            input,
            output: 5,
            cost_usd: cost,
            ..Usage::default()
        });
        let events = [ev("s1", 1_000, usage(10, None)), ev("s1", 2_000, usage(20, Some(0.5)))];
        let s = apply_all(&events).remove(&SessionKey::new(Source::ClaudeCode, "s1")).unwrap();
        assert_eq!(s.usage.input, 30);
        assert_eq!(s.usage.output, 10);
        assert_eq!(s.usage.cost_usd, Some(0.5));
        assert_eq!(s.model.as_deref(), Some("claude-opus-5-5"));
    }

    #[test]
    fn error_then_activity_recovers() {
        let err = Body::Error { class: ErrorClass::RateLimit, message: Snippet::new("429") };
        let s = fold(None, &ev("s1", 1_000, err)).unwrap();
        assert_eq!((s.state, s.errors), (State::Errored, 1));
        let s = fold(Some(s), &ev("s1", 2_000, prompt("retry"))).unwrap();
        assert_eq!((s.state, s.errors), (State::Working, 1));
    }

    #[test]
    fn ended_session_revives_on_resume() {
        let s = fold(None, &ev("s1", 1_000, Body::SessionEnded { reason: EndReason::Exited })).unwrap();
        assert_eq!(s.state, State::Ended);
        let s = fold(Some(s), &ev("s1", 2_000, prompt("again"))).unwrap();
        assert_eq!(s.state, State::Working);
    }

    #[test]
    fn last_event_at_never_moves_backwards() {
        let s = fold(None, &ev("s1", 5_000, prompt("a"))).unwrap();
        let s = fold(Some(s), &ev("s1", 1_000, prompt("b"))).unwrap();
        assert_eq!(s.last_event_at, Timestamp::from_epoch_millis(5_000).unwrap());
    }

    #[test]
    fn host_events_do_not_create_sessions() {
        assert!(fold(None, &ev("s1", 1_000, Body::HostHeartbeat { collectors: Vec::new() })).is_none());
    }

    #[test]
    fn replay_is_deterministic() {
        let events = [
            ev("s1", 1_000, prompt("go")),
            ev("s2", 1_500, prompt("other")),
            ev("s1", 2_000, called("t1", "Bash")),
        ];
        assert_eq!(apply_all(&events), apply_all(&events));
        assert_eq!(apply_all(&events).len(), 2);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `nix develop -c cargo test -p ovs-state`
Expected: compile errors, `cannot find function fold`, `apply_all`, `State`.

- [ ] **Step 3: Implement**

Insert into `crates/ovs-state/src/lib.rs` between the doc line and `#[cfg(test)]`:

```rust
use std::collections::BTreeMap;

use ovs_event::{
    ActivityState, Body, CallId, Event, Host, SessionKey, Snippet, Source, Timestamp, Usage,
};
use serde::{Deserialize, Serialize};

/// Tool names that mean the agent is asking the user a question.
pub const QUESTION_TOOLS: &[&str] = &["AskUserQuestion", "question"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Working,
    Idle,
    WaitingInput,
    WaitingPermission,
    Errored,
    Ended,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenCall {
    pub name: String,
    pub since: Timestamp,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    pub cost_usd: Option<f64>,
}

impl UsageTotals {
    fn add(&mut self, usage: &Usage) {
        self.input = self.input.saturating_add(usage.input);
        self.output = self.output.saturating_add(usage.output);
        self.cache_read = self.cache_read.saturating_add(usage.cache_read);
        self.cache_write = self.cache_write.saturating_add(usage.cache_write);
        self.reasoning = self.reasoning.saturating_add(usage.reasoning);
        self.cost_usd = match (self.cost_usd, usage.cost_usd) {
            (Some(total), Some(more)) => Some(total + more),
            (total, None) => total,
            (None, more) => more,
        };
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    pub session: SessionKey,
    pub source: Source,
    pub host: Host,
    pub parent: Option<SessionKey>,
    pub state: State,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub started_at: Timestamp,
    pub last_event_at: Timestamp,
    pub activity: Option<Snippet>,
    pub open_calls: BTreeMap<CallId, OpenCall>,
    pub usage: UsageTotals,
    pub errors: u64,
}

impl SessionState {
    fn new(event: &Event) -> Self {
        Self {
            session: event.session.clone(),
            source: event.source,
            host: event.host.clone(),
            parent: event.parent.clone(),
            state: State::Idle,
            cwd: None,
            git_branch: None,
            title: None,
            model: None,
            started_at: event.ts,
            last_event_at: event.ts,
            activity: None,
            open_calls: BTreeMap::new(),
            usage: UsageTotals::default(),
            errors: 0,
        }
    }

    fn has_open_question(&self) -> bool {
        self.open_calls
            .values()
            .any(|call| QUESTION_TOOLS.contains(&call.name.as_str()))
    }

    fn state_for(&self, reported: ActivityState) -> State {
        match reported {
            ActivityState::Working => State::Working,
            ActivityState::Idle if self.has_open_question() => State::WaitingInput,
            ActivityState::Idle => State::Idle,
            ActivityState::WaitingInput => State::WaitingInput,
            ActivityState::WaitingPermission => State::WaitingPermission,
        }
    }
}

/// Applies one event to a session's state. Host and alert events leave `prev` unchanged.
pub fn fold(prev: Option<SessionState>, event: &Event) -> Option<SessionState> {
    if matches!(
        event.body,
        Body::HostHeartbeat { .. } | Body::AlertRaised { .. } | Body::AlertCleared { .. }
    ) {
        return prev;
    }
    let mut s = prev.unwrap_or_else(|| SessionState::new(event));
    s.last_event_at = s.last_event_at.max(event.ts);
    match &event.body {
        Body::SessionStarted(start) => {
            s.cwd = start.cwd.clone().or(s.cwd.take());
            s.git_branch = start.git_branch.clone().or(s.git_branch.take());
            s.title = start.title.clone().or(s.title.take());
            s.model = start.model.clone().or(s.model.take());
        }
        Body::SessionEnded { .. } => s.state = State::Ended,
        Body::SessionStatus { state } => s.state = s.state_for(*state),
        Body::Prompt { text, .. } => {
            s.state = State::Working;
            s.activity = Some(text.clone());
        }
        Body::Message { text, model } => {
            s.state = State::Working;
            s.activity = Some(text.clone());
            if model.is_some() {
                s.model = model.clone();
            }
        }
        Body::ToolCalled { call_id, name, input } => {
            s.open_calls.insert(
                call_id.clone(),
                OpenCall { name: name.clone(), since: event.ts },
            );
            s.activity = Some(Snippet::new(&format!("{name}: {}", input.as_str())));
            s.state = if QUESTION_TOOLS.contains(&name.as_str()) {
                State::WaitingInput
            } else {
                State::Working
            };
        }
        Body::ToolResult { call_id, .. } => {
            s.open_calls.remove(call_id);
            s.state = if s.has_open_question() {
                State::WaitingInput
            } else {
                State::Working
            };
        }
        Body::Usage(usage) => {
            s.usage.add(usage);
            if usage.model.is_some() {
                s.model = usage.model.clone();
            }
        }
        Body::Error { message, .. } => {
            s.state = State::Errored;
            s.errors = s.errors.saturating_add(1);
            s.activity = Some(message.clone());
        }
        Body::HostHeartbeat { .. } | Body::AlertRaised { .. } | Body::AlertCleared { .. } => {}
    }
    Some(s)
}

/// Folds events in the order given into one state per session.
pub fn apply_all<'a>(events: impl IntoIterator<Item = &'a Event>) -> BTreeMap<SessionKey, SessionState> {
    let mut sessions = BTreeMap::new();
    for event in events {
        let prev = sessions.remove(&event.session);
        if let Some(next) = fold(prev, event) {
            sessions.insert(event.session.clone(), next);
        }
    }
    sessions
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `nix develop -c cargo test -p ovs-state`
Expected: 11 passed.

Run: `nix develop -c cargo clippy -p ovs-state --all-targets -- -D warnings && nix develop -c cargo fmt --check`
Expected: clean. If `arithmetic_side_effects` flags `total + more` on `f64`, replace that arm with `(Some(total), Some(more)) => Some(total.add(more))` after `use std::ops::Add;` — do not add an `allow`.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/ovs-state
git commit -m "feat(ovs-state): fold events into per-session state"
```

---

### Task 5: Claude Code transcript line parser

**Files:**
- Modify: `Cargo.toml` (add member `crates/ovs-collect-claude`)
- Create: `crates/ovs-collect-claude/Cargo.toml`, `crates/ovs-collect-claude/src/lib.rs`, `crates/ovs-collect-claude/src/record.rs`

**Interfaces:**
- Consumes: `ovs_event::*` from Tasks 1–3.
- Produces:
  - `FileState { offset: u64, started: bool, seen_usage: HashSet<String> /* not persisted */ }` (`Default`, serde).
  - `LineContext<'a> { host: &'a Host, native_session: &'a str, session: &'a SessionKey, parent: Option<&'a SessionKey>, path: &'a str, offset: u64 }`.
  - `parse_line(&LineContext<'_>, &str, &mut FileState) -> Result<Vec<Event>, LineError>`.
  - `pub(crate) fn build(&LineContext<'_>, Timestamp, String, Body) -> Event` (used by Task 7 for parse-error events).
  - `LineError::{Json { path, offset, source }, Timestamp { path, offset, kind }}`.
- Mapping (from the spec's per-source notes):
  - First `user`/`assistant`/`system` record carrying `cwd` → `session.started { cwd, git_branch }`, position `"<offset>:started"`.
  - `user` with `isMeta: true` → nothing. String content or `text` block → `prompt` (or `error { class: aborted }` when it starts with `[Request interrupted by user`). `tool_result` block → `tool.result`.
  - `assistant` with `isApiErrorMessage: true` → `error` (`rate_limit` when `error == "rate_limit"`, else `api`). Otherwise `text` → `message`, `tool_use` → `tool.called`, then `usage` once per `message.id` with position `"usage:<message.id>"`. Model `<synthetic>` → `None`.
  - `system` with subtype `turn_duration` or `stop_hook_summary` → `session.status { idle }`.
  - Every other record type → nothing.
  - Block positions: `"<offset>:<block index>"`.

- [ ] **Step 1: Scaffold and write the failing tests**

Add `"crates/ovs-collect-claude",` to workspace `members`.

Create `crates/ovs-collect-claude/Cargo.toml`:

```toml
[package]
name = "ovs-collect-claude"
description = "Turns Claude Code's on-disk session store into ovs events"
version.workspace = true
edition.workspace = true
publish.workspace = true
license.workspace = true

[dependencies]
ovs-event = { version = "0.1.0", path = "../ovs-event" }
serde = { version = "1.0.228", features = ["derive"] }
serde_json = "1.0.154"
thiserror = "2"
tracing = "0.1.44"

[dev-dependencies]
tempfile = "3.27.0"

[lints]
workspace = true
```

Create `crates/ovs-collect-claude/src/lib.rs`:

```rust
//! Reads Claude Code's on-disk session store and turns it into `ovs` events.

mod record;

pub use record::{FileState, LineContext, LineError, parse_line};
```

Create `crates/ovs-collect-claude/src/record.rs` with only tests:

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use ovs_event::{ActivityState, Body, ErrorClass, Host, PromptBy, SessionKey, Source};

    use super::*;

    const PROMPT: &str = r#"{"type":"user","timestamp":"2026-10-20T05:00:00.000Z","sessionId":"s1","cwd":"/tmp/proj","gitBranch":"main","message":{"role":"user","content":"fix the parser"}}"#;
    const META: &str = r#"{"type":"user","isMeta":true,"timestamp":"2026-10-20T05:00:00.500Z","sessionId":"s1","cwd":"/tmp/proj","message":{"role":"user","content":"<caveat>"}}"#;
    const ASSISTANT_TEXT: &str = r#"{"type":"assistant","timestamp":"2026-10-20T05:00:01.000Z","sessionId":"s1","cwd":"/tmp/proj","message":{"id":"msg_1","model":"claude-opus-5-5","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"Running tests"}],"usage":{"input_tokens":2,"cache_creation_input_tokens":27640,"cache_read_input_tokens":23327,"output_tokens":265,"output_tokens_details":{"thinking_tokens":94}}}}"#;
    const ASSISTANT_TOOL: &str = r#"{"type":"assistant","timestamp":"2026-10-20T05:00:01.000Z","sessionId":"s1","cwd":"/tmp/proj","message":{"id":"msg_1","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo test"}}],"usage":{"input_tokens":2,"output_tokens":265}}}"#;
    const TOOL_ERROR: &str = r#"{"type":"user","timestamp":"2026-10-20T05:00:02.000Z","sessionId":"s1","cwd":"/tmp/proj","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","is_error":true,"content":[{"type":"text","text":"exit 101"}]}]}}"#;
    const INTERRUPT: &str = r#"{"type":"user","timestamp":"2026-10-20T05:00:03.000Z","sessionId":"s1","cwd":"/tmp/proj","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}"#;
    const API_ERROR: &str = r#"{"type":"assistant","isApiErrorMessage":true,"error":"rate_limit","timestamp":"2026-10-20T05:00:04.000Z","sessionId":"s1","cwd":"/tmp/proj","message":{"id":"msg_2","model":"<synthetic>","content":[{"type":"text","text":"API Error: 429"}]}}"#;
    const TURN_END: &str = r#"{"type":"system","subtype":"turn_duration","durationMs":1200,"timestamp":"2026-10-20T05:00:05.000Z","sessionId":"s1","cwd":"/tmp/proj"}"#;
    const TITLE: &str = r#"{"type":"ai-title","aiTitle":"Parser fix"}"#;

    struct Fixture {
        host: Host,
        session: SessionKey,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                host: Host::new("test-host"),
                session: SessionKey::new(Source::ClaudeCode, "s1"),
            }
        }

        fn parse(&self, offset: u64, line: &str, file: &mut FileState) -> Vec<Event> {
            let ctx = LineContext {
                host: &self.host,
                native_session: "s1",
                session: &self.session,
                parent: None,
                path: "/p/s1.jsonl",
                offset,
            };
            parse_line(&ctx, line, file).unwrap()
        }
    }

    fn kinds(events: &[Event]) -> Vec<&'static str> {
        events.iter().map(|e| e.body.kind()).collect()
    }

    #[test]
    fn first_prompt_starts_the_session() {
        let fx = Fixture::new();
        let mut file = FileState::default();
        let events = fx.parse(0, PROMPT, &mut file);
        assert_eq!(kinds(&events), ["session.started", "prompt"]);
        let Body::SessionStarted(start) = &events[0].body else { unreachable!() };
        assert_eq!(start.cwd.as_deref(), Some("/tmp/proj"));
        assert_eq!(start.git_branch.as_deref(), Some("main"));
        let Body::Prompt { text, by } = &events[1].body else { unreachable!() };
        assert_eq!((text.as_str(), *by), ("fix the parser", PromptBy::User));
        assert_eq!(events[1].source_ref.as_ref().unwrap().offset, 0);
        assert!(file.started);
        assert_eq!(kinds(&fx.parse(500, PROMPT, &mut file)), ["prompt"]);
    }

    #[test]
    fn meta_records_are_skipped() {
        let fx = Fixture::new();
        let mut file = FileState { started: true, ..FileState::default() };
        assert!(fx.parse(0, META, &mut file).is_empty());
    }

    #[test]
    fn usage_is_emitted_once_per_message_id() {
        let fx = Fixture::new();
        let mut file = FileState { started: true, ..FileState::default() };
        let first = fx.parse(100, ASSISTANT_TEXT, &mut file);
        assert_eq!(kinds(&first), ["message", "usage"]);
        let Body::Usage(usage) = &first[1].body else { unreachable!() };
        assert_eq!(
            (usage.input, usage.output, usage.cache_read, usage.cache_write, usage.reasoning),
            (2, 265, 23327, 27640, 94)
        );
        assert_eq!(usage.model.as_deref(), Some("claude-opus-5-5"));
        let second = fx.parse(900, ASSISTANT_TOOL, &mut file);
        assert_eq!(kinds(&second), ["tool.called"]);
        let Body::ToolCalled { name, input, .. } = &second[0].body else { unreachable!() };
        assert_eq!(name, "Bash");
        assert_eq!(input.as_str(), r#"{"command":"cargo test"}"#);
    }

    #[test]
    fn usage_id_depends_on_message_not_offset() {
        let fx = Fixture::new();
        let a = fx.parse(100, ASSISTANT_TEXT, &mut FileState { started: true, ..FileState::default() });
        let b = fx.parse(900, ASSISTANT_TOOL, &mut FileState { started: true, ..FileState::default() });
        assert_eq!(a[1].id, b[1].id);
    }

    #[test]
    fn failed_tool_result_carries_the_error() {
        let fx = Fixture::new();
        let events = fx.parse(0, TOOL_ERROR, &mut FileState { started: true, ..FileState::default() });
        let Body::ToolResult { call_id, ok, error, output } = &events[0].body else { unreachable!() };
        assert_eq!(call_id.as_str(), "toolu_1");
        assert!(!ok);
        assert_eq!(error.as_ref().unwrap().as_str(), "exit 101");
        assert_eq!(output.as_str(), "exit 101");
    }

    #[test]
    fn interrupt_is_an_aborted_error() {
        let fx = Fixture::new();
        let events = fx.parse(0, INTERRUPT, &mut FileState { started: true, ..FileState::default() });
        assert!(matches!(events[0].body, Body::Error { class: ErrorClass::Aborted, .. }));
    }

    #[test]
    fn api_error_maps_rate_limit() {
        let fx = Fixture::new();
        let events = fx.parse(0, API_ERROR, &mut FileState { started: true, ..FileState::default() });
        assert_eq!(kinds(&events), ["error"]);
        assert!(matches!(events[0].body, Body::Error { class: ErrorClass::RateLimit, .. }));
    }

    #[test]
    fn turn_end_reports_idle() {
        let fx = Fixture::new();
        let events = fx.parse(0, TURN_END, &mut FileState { started: true, ..FileState::default() });
        assert!(matches!(events[0].body, Body::SessionStatus { state: ActivityState::Idle }));
    }

    #[test]
    fn unrelated_records_and_bad_json() {
        let fx = Fixture::new();
        assert!(fx.parse(0, TITLE, &mut FileState::default()).is_empty());
        let ctx = LineContext {
            host: &fx.host,
            native_session: "s1",
            session: &fx.session,
            parent: None,
            path: "/p/s1.jsonl",
            offset: 7,
        };
        let err = parse_line(&ctx, "{not json", &mut FileState::default()).unwrap_err();
        assert!(err.to_string().starts_with("/p/s1.jsonl:7: line is not JSON"));
    }

    #[test]
    fn ids_are_stable_across_reads() {
        let fx = Fixture::new();
        let a = fx.parse(0, PROMPT, &mut FileState::default());
        let b = fx.parse(0, PROMPT, &mut FileState::default());
        assert_eq!(a, b);
    }
}
```

Add `clippy::unreachable` to that test module's `#![allow(...)]` list.

- [ ] **Step 2: Run tests to verify they fail**

Run: `nix develop -c cargo test -p ovs-collect-claude`
Expected: compile errors, `cannot find function parse_line`, `FileState`, `LineContext`.

- [ ] **Step 3: Implement**

Prepend to `crates/ovs-collect-claude/src/record.rs`:

```rust
use std::collections::HashSet;

use ovs_event::{
    ActivityState, Body, CallId, ErrorClass, Event, EventId, Host, PromptBy, SchemaVersion,
    SessionKey, SessionStarted, Snippet, Source, SourceRef, Timestamp, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const INTERRUPT_PREFIX: &str = "[Request interrupted by user";
const SYNTHETIC_MODEL: &str = "<synthetic>";

/// Read progress for one transcript file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileState {
    pub offset: u64,
    pub started: bool,
    // Not persisted: after a restart a repeated usage line gets the same id and is dropped at ingest.
    #[serde(skip)]
    pub seen_usage: HashSet<String>,
}

pub struct LineContext<'a> {
    pub host: &'a Host,
    pub native_session: &'a str,
    pub session: &'a SessionKey,
    pub parent: Option<&'a SessionKey>,
    pub path: &'a str,
    pub offset: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum LineError {
    #[error("{path}:{offset}: line is not JSON: {source}")]
    Json {
        path: String,
        offset: u64,
        source: serde_json::Error,
    },
    #[error("{path}:{offset}: {kind} record has no valid timestamp")]
    Timestamp {
        path: String,
        offset: u64,
        kind: String,
    },
}

pub fn parse_line(
    ctx: &LineContext<'_>,
    line: &str,
    file: &mut FileState,
) -> Result<Vec<Event>, LineError> {
    let record: Value = serde_json::from_str(line).map_err(|source| LineError::Json {
        path: ctx.path.to_owned(),
        offset: ctx.offset,
        source,
    })?;
    let kind = record.get("type").and_then(Value::as_str).unwrap_or_default();
    if !matches!(kind, "user" | "assistant" | "system") {
        return Ok(Vec::new());
    }
    let ts = record
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|text| Timestamp::parse(text).ok())
        .ok_or_else(|| LineError::Timestamp {
            path: ctx.path.to_owned(),
            offset: ctx.offset,
            kind: kind.to_owned(),
        })?;
    let mut out = Vec::new();
    if !file.started
        && let Some(cwd) = str_field(&record, "cwd")
    {
        let start = SessionStarted {
            cwd: Some(cwd),
            git_branch: str_field(&record, "gitBranch"),
            ..SessionStarted::default()
        };
        out.push(build(ctx, ts, format!("{}:started", ctx.offset), Body::SessionStarted(start)));
        file.started = true;
    }
    match kind {
        "user" => user_events(ctx, ts, &record, &mut out),
        "assistant" => assistant_events(ctx, ts, &record, file, &mut out),
        _ => system_events(ctx, ts, &record, &mut out),
    }
    Ok(out)
}

pub(crate) fn build(ctx: &LineContext<'_>, ts: Timestamp, position: String, body: Body) -> Event {
    Event {
        v: SchemaVersion,
        id: EventId::derive(ctx.host, Source::ClaudeCode, ctx.native_session, &position),
        ts,
        host: ctx.host.clone(),
        source: Source::ClaudeCode,
        session: ctx.session.clone(),
        parent: ctx.parent.cloned(),
        body,
        source_ref: Some(SourceRef {
            host: ctx.host.clone(),
            path: ctx.path.to_owned(),
            offset: ctx.offset,
        }),
    }
}

fn str_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn prompt_or_abort(text: &str) -> Body {
    if text.starts_with(INTERRUPT_PREFIX) {
        Body::Error {
            class: ErrorClass::Aborted,
            message: Snippet::new(text),
        }
    } else {
        Body::Prompt {
            text: Snippet::new(text),
            by: PromptBy::User,
        }
    }
}

fn user_events(ctx: &LineContext<'_>, ts: Timestamp, record: &Value, out: &mut Vec<Event>) {
    if record.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return;
    }
    let Some(content) = record.get("message").and_then(|m| m.get("content")) else {
        return;
    };
    match content {
        Value::String(text) => {
            out.push(build(ctx, ts, format!("{}:0", ctx.offset), prompt_or_abort(text)));
        }
        Value::Array(blocks) => {
            for (index, block) in blocks.iter().enumerate() {
                let position = format!("{}:{index}", ctx.offset);
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_result") => {
                        let failed = block.get("is_error").and_then(Value::as_bool) == Some(true);
                        let output = text_of(block.get("content").unwrap_or(&Value::Null));
                        let body = Body::ToolResult {
                            call_id: CallId::new(
                                block.get("tool_use_id").and_then(Value::as_str).unwrap_or_default(),
                            ),
                            ok: !failed,
                            error: failed.then(|| Snippet::new(&output)),
                            output: Snippet::new(&output),
                        };
                        out.push(build(ctx, ts, position, body));
                    }
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            out.push(build(ctx, ts, position, prompt_or_abort(text)));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn assistant_events(
    ctx: &LineContext<'_>,
    ts: Timestamp,
    record: &Value,
    file: &mut FileState,
    out: &mut Vec<Event>,
) {
    let message = record.get("message").unwrap_or(&Value::Null);
    if record.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true) {
        let class = match record.get("error").and_then(Value::as_str) {
            Some("rate_limit") => ErrorClass::RateLimit,
            _ => ErrorClass::Api,
        };
        let text = text_of(message.get("content").unwrap_or(&Value::Null));
        out.push(build(
            ctx,
            ts,
            format!("{}:error", ctx.offset),
            Body::Error { class, message: Snippet::new(&text) },
        ));
        return;
    }
    let model = message
        .get("model")
        .and_then(Value::as_str)
        .filter(|name| *name != SYNTHETIC_MODEL)
        .map(str::to_owned);
    if let Some(Value::Array(blocks)) = message.get("content") {
        for (index, block) in blocks.iter().enumerate() {
            let position = format!("{}:{index}", ctx.offset);
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        let body = Body::Message { text: Snippet::new(text), model: model.clone() };
                        out.push(build(ctx, ts, position, body));
                    }
                }
                Some("tool_use") => {
                    let input = block.get("input").map(Value::to_string).unwrap_or_default();
                    let body = Body::ToolCalled {
                        call_id: CallId::new(block.get("id").and_then(Value::as_str).unwrap_or_default()),
                        name: block.get("name").and_then(Value::as_str).unwrap_or_default().to_owned(),
                        input: Snippet::new(&input),
                    };
                    out.push(build(ctx, ts, position, body));
                }
                _ => {}
            }
        }
    }
    // One API message is split into one line per content block, each repeating the usage.
    if let (Some(id), Some(usage)) = (message.get("id").and_then(Value::as_str), message.get("usage"))
        && file.seen_usage.insert(id.to_owned())
    {
        out.push(build(ctx, ts, format!("usage:{id}"), Body::Usage(usage_from(usage, model))));
    }
}

fn count(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn usage_from(usage: &Value, model: Option<String>) -> Usage {
    Usage {
        model,
        input: count(usage, "input_tokens"),
        output: count(usage, "output_tokens"),
        cache_read: count(usage, "cache_read_input_tokens"),
        cache_write: count(usage, "cache_creation_input_tokens"),
        reasoning: usage
            .get("output_tokens_details")
            .map(|details| count(details, "thinking_tokens"))
            .unwrap_or(0),
        cost_usd: None,
    }
}

fn system_events(ctx: &LineContext<'_>, ts: Timestamp, record: &Value, out: &mut Vec<Event>) {
    if matches!(
        record.get("subtype").and_then(Value::as_str),
        Some("turn_duration" | "stop_hook_summary")
    ) {
        out.push(build(
            ctx,
            ts,
            format!("{}:status", ctx.offset),
            Body::SessionStatus { state: ActivityState::Idle },
        ));
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `nix develop -c cargo test -p ovs-collect-claude`
Expected: 10 passed.

Run: `nix develop -c cargo clippy -p ovs-collect-claude --all-targets -- -D warnings && nix develop -c cargo fmt`
Expected: clean (`cargo fmt` rewrites long lines; that is fine before committing).

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/ovs-collect-claude
git commit -m "feat(ovs-collect-claude): parse transcript lines into events"
```

---

### Task 6: Live-session registry

**Files:**
- Create: `crates/ovs-collect-claude/src/registry.rs`
- Modify: `crates/ovs-collect-claude/src/lib.rs`

**Interfaces:**
- Consumes: `ovs_event::{Event, Body, ActivityState, EndReason, EventId, Host, SchemaVersion, SessionKey, Source, Timestamp}`.
- Produces:
  - `LiveEntry { pid: u32, session_id: String, status: String, status_updated_at: String }` (serde, `Eq`).
  - `read_registry(&Path) -> Result<Vec<LiveEntry>, RegistryError>` (missing dir → empty; unparseable file → skipped).
  - `diff_registry(&Host, &BTreeMap<String, LiveEntry>, &[LiveEntry], impl Fn(u32) -> bool, Timestamp) -> (Vec<Event>, BTreeMap<String, LiveEntry>)`.
  - `pid_alive(u32) -> bool`.
- Rules: status `busy` or `shell` → `working`, `idle` → `idle`, anything else → no event. A changed entry for a live pid emits `session.status` with position `"status:<statusUpdatedAt>"` and ts from `statusUpdatedAt` (epoch ms or RFC 3339; falls back to `now`). An entry in `previous` that is missing from the live set emits `session.ended { exited }` with position `"ended:<pid>"` and ts `now`.

- [ ] **Step 1: Write the failing tests**

Create `crates/ovs-collect-claude/src/registry.rs` with only tests:

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use std::fs;

    use ovs_event::{ActivityState, Body, EndReason, Host, Timestamp};

    use super::*;

    fn write_entry(dir: &Path, pid: u32, status: &str, updated: u64) {
        fs::write(
            dir.join(format!("{pid}.json")),
            format!(r#"{{"pid":{pid},"sessionId":"s{pid}","cwd":"/tmp","status":"{status}","statusUpdatedAt":{updated}}}"#),
        )
        .unwrap();
    }

    #[test]
    fn missing_directory_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_registry(&dir.path().join("nope")).unwrap().is_empty());
    }

    #[test]
    fn reads_entries_and_skips_garbage() {
        let dir = tempfile::tempdir().unwrap();
        write_entry(dir.path(), 123, "busy", 1_792_474_392_123);
        fs::write(dir.path().join("999.json"), "{partial").unwrap();
        let live = read_registry(dir.path()).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].pid, 123);
        assert_eq!(live[0].session_id, "s123");
        assert_eq!(live[0].status_updated_at, "1792474392123");
    }

    #[test]
    fn status_changes_and_exits_become_events() {
        let dir = tempfile::tempdir().unwrap();
        let host = Host::new("test-host");
        let now = Timestamp::from_epoch_millis(1_792_474_400_000).unwrap();

        write_entry(dir.path(), 123, "busy", 1_792_474_392_123);
        let (events, live) = diff_registry(&host, &BTreeMap::new(), &read_registry(dir.path()).unwrap(), |_| true, now);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].body, Body::SessionStatus { state: ActivityState::Working }));
        assert_eq!(events[0].session.as_str(), "claude-code:s123");
        assert_eq!(events[0].ts.to_string(), "2026-10-20T05:33:12.123Z");

        let (events, live) = diff_registry(&host, &live, &read_registry(dir.path()).unwrap(), |_| true, now);
        assert!(events.is_empty());

        write_entry(dir.path(), 123, "idle", 1_792_474_395_000);
        let (events, live) = diff_registry(&host, &live, &read_registry(dir.path()).unwrap(), |_| true, now);
        assert!(matches!(events[0].body, Body::SessionStatus { state: ActivityState::Idle }));

        let (events, live) = diff_registry(&host, &live, &read_registry(dir.path()).unwrap(), |_| false, now);
        assert!(matches!(events[0].body, Body::SessionEnded { reason: EndReason::Exited }));
        assert_eq!(events[0].ts, now);
        assert!(live.is_empty());
    }
}
```

Change `crates/ovs-collect-claude/src/lib.rs` to:

```rust
//! Reads Claude Code's on-disk session store and turns it into `ovs` events.

mod record;
mod registry;

pub use record::{FileState, LineContext, LineError, parse_line};
pub use registry::{LiveEntry, RegistryError, diff_registry, pid_alive, read_registry};
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `nix develop -c cargo test -p ovs-collect-claude registry`
Expected: compile errors, `cannot find function read_registry`.

- [ ] **Step 3: Implement**

Prepend to `crates/ovs-collect-claude/src/registry.rs`:

```rust
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::{fs, io};

use ovs_event::{
    ActivityState, Body, EndReason, Event, EventId, Host, SchemaVersion, SessionKey, Source,
    Timestamp,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One `~/.claude/sessions/<pid>.json` file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveEntry {
    pub pid: u32,
    pub session_id: String,
    pub status: String,
    pub status_updated_at: String,
}

#[derive(Debug, thiserror::Error)]
#[error("cannot list Claude Code live sessions in {path}")]
pub struct RegistryError {
    path: PathBuf,
    #[source]
    source: io::Error,
}

pub fn read_registry(dir: &Path) -> Result<Vec<LiveEntry>, RegistryError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(RegistryError { path: dir.to_owned(), source }),
    };
    let mut live = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|source| RegistryError { path: dir.to_owned(), source })?
            .path();
        if path.extension() != Some(OsStr::new("json")) {
            continue;
        }
        // Claude Code rewrites these files in place, so a half-written one is expected now and then.
        match fs::read_to_string(&path).ok().and_then(|text| parse_entry(&text)) {
            Some(parsed) => live.push(parsed),
            None => tracing::debug!(path = %path.display(), "skipping unreadable live-session file"),
        }
    }
    live.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    Ok(live)
}

fn parse_entry(text: &str) -> Option<LiveEntry> {
    let value: Value = serde_json::from_str(text).ok()?;
    Some(LiveEntry {
        pid: u32::try_from(value.get("pid")?.as_u64()?).ok()?,
        session_id: value.get("sessionId")?.as_str()?.to_owned(),
        status: value
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        status_updated_at: value
            .get("statusUpdatedAt")
            .map(Value::to_string)
            .unwrap_or_default(),
    })
}

pub fn diff_registry(
    host: &Host,
    previous: &BTreeMap<String, LiveEntry>,
    current: &[LiveEntry],
    is_alive: impl Fn(u32) -> bool,
    now: Timestamp,
) -> (Vec<Event>, BTreeMap<String, LiveEntry>) {
    let mut events = Vec::new();
    let mut next = BTreeMap::new();
    for entry in current.iter().filter(|entry| is_alive(entry.pid)) {
        if previous.get(&entry.session_id) != Some(entry)
            && let Some(state) = activity(&entry.status)
        {
            let ts = updated_at(&entry.status_updated_at).unwrap_or(now);
            let position = format!("status:{}", entry.status_updated_at);
            events.push(registry_event(host, &entry.session_id, &position, ts, Body::SessionStatus { state }));
        }
        next.insert(entry.session_id.clone(), entry.clone());
    }
    for (session_id, entry) in previous {
        if !next.contains_key(session_id) {
            let position = format!("ended:{}", entry.pid);
            let body = Body::SessionEnded { reason: EndReason::Exited };
            events.push(registry_event(host, session_id, &position, now, body));
        }
    }
    (events, next)
}

fn activity(status: &str) -> Option<ActivityState> {
    match status {
        "busy" | "shell" => Some(ActivityState::Working),
        "idle" => Some(ActivityState::Idle),
        _ => None,
    }
}

fn updated_at(raw: &str) -> Option<Timestamp> {
    match raw.parse::<i64>() {
        Ok(millis) => Timestamp::from_epoch_millis(millis).ok(),
        Err(_) => Timestamp::parse(raw.trim_matches('"')).ok(),
    }
}

fn registry_event(host: &Host, session_id: &str, position: &str, ts: Timestamp, body: Body) -> Event {
    Event {
        v: SchemaVersion,
        id: EventId::derive(host, Source::ClaudeCode, session_id, position),
        ts,
        host: host.clone(),
        source: Source::ClaudeCode,
        session: SessionKey::new(Source::ClaudeCode, session_id),
        parent: None,
        body,
        source_ref: None,
    }
}

#[cfg(target_os = "linux")]
pub fn pid_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

// Without /proc, rely on Claude Code removing the file on exit (unverified off Linux).
#[cfg(not(target_os = "linux"))]
pub fn pid_alive(_pid: u32) -> bool {
    true
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `nix develop -c cargo test -p ovs-collect-claude`
Expected: 13 passed.

Run: `nix develop -c cargo clippy -p ovs-collect-claude --all-targets -- -D warnings && nix develop -c cargo fmt`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/ovs-collect-claude
git commit -m "feat(ovs-collect-claude): report live session status and exits"
```

---

### Task 7: Cursor and tailing collector

**Files:**
- Create: `crates/ovs-collect-claude/src/cursor.rs`, `crates/ovs-collect-claude/src/collector.rs`
- Modify: `crates/ovs-collect-claude/src/lib.rs`

**Interfaces:**
- Consumes: `parse_line`, `build`, `FileState`, `LineContext` (Task 5); `read_registry`, `diff_registry`, `LiveEntry` (Task 6).
- Produces:
  - `Cursor { files: BTreeMap<PathBuf, FileState>, live: BTreeMap<String, LiveEntry> }`, `Cursor::load(&Path) -> Result<Cursor, CursorError>` (missing file → default), `Cursor::save(&self, &Path) -> Result<(), CursorError>` (temp file then rename).
  - `Collector::open(root: PathBuf, host: Host, cursor_path: PathBuf, since: Option<SystemTime>) -> Result<Collector, CollectError>`.
  - `Collector::poll(&mut self, out: &mut impl Write, is_alive: impl Fn(u32) -> bool) -> Result<PollStats, CollectError>`.
  - `PollStats { events: usize, parse_errors: usize }`.
- Behaviour: discovers `<root>/projects/*/<sid>.jsonl` (session `claude-code:<sid>`) and `<root>/projects/*/<sid>/subagents/agent-<id>.jsonl` (session `claude-code:agent-<id>`, parent `claude-code:<sid>`), sorted by path. Reads only complete lines from the stored offset. A file shorter than its offset is re-read from 0. A line that fails to parse becomes `error { class: parse }` with position `"<offset>:parse"`. Files never seen before and last modified before `since` are skipped. Events are written and flushed before the cursor is saved.

- [ ] **Step 1: Write the failing tests**

Create `crates/ovs-collect-claude/src/collector.rs` with only tests:

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use std::fs::{self, OpenOptions};
    use std::io::Write as _;
    use std::time::{Duration, SystemTime};

    use ovs_event::{Body, ErrorClass, Event, Host};

    use super::*;

    const LINE: &str = r#"{"type":"user","timestamp":"2026-10-20T05:00:00.000Z","sessionId":"s1","cwd":"/tmp/proj","message":{"role":"user","content":"hello"}}"#;

    struct Env {
        dir: tempfile::TempDir,
    }

    impl Env {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir_all(dir.path().join("claude/projects/-tmp-proj/s1/subagents")).unwrap();
            Self { dir }
        }

        fn transcript(&self) -> PathBuf {
            self.dir.path().join("claude/projects/-tmp-proj/s1.jsonl")
        }

        fn append(&self, path: &Path, text: &str) {
            OpenOptions::new().create(true).append(true).open(path).unwrap().write_all(text.as_bytes()).unwrap();
        }

        fn collector(&self, since: Option<SystemTime>) -> Collector {
            Collector::open(
                self.dir.path().join("claude"),
                Host::new("test-host"),
                self.dir.path().join("state/cursor.json"),
                since,
            )
            .unwrap()
        }

        fn poll(&self, collector: &mut Collector) -> Vec<Event> {
            let mut out = Vec::new();
            collector.poll(&mut out, |_| true).unwrap();
            String::from_utf8(out).unwrap().lines().map(|l| Event::from_line(l).unwrap()).collect()
        }
    }

    #[test]
    fn partial_lines_wait_for_their_newline() {
        let env = Env::new();
        let mut collector = env.collector(None);
        let (head, tail) = LINE.split_at(40);
        env.append(&env.transcript(), head);
        assert!(env.poll(&mut collector).is_empty());
        env.append(&env.transcript(), &format!("{tail}\n"));
        let kinds: Vec<_> = env.poll(&mut collector).iter().map(|e| e.body.kind()).collect();
        assert_eq!(kinds, ["session.started", "prompt"]);
    }

    #[test]
    fn restart_resumes_from_the_cursor() {
        let env = Env::new();
        env.append(&env.transcript(), &format!("{LINE}\n"));
        assert_eq!(env.poll(&mut env.collector(None)).len(), 2);
        assert!(env.poll(&mut env.collector(None)).is_empty());
        env.append(&env.transcript(), &format!("{LINE}\n"));
        let again = env.poll(&mut env.collector(None));
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].body.kind(), "prompt");
    }

    #[test]
    fn subagent_transcripts_carry_their_parent() {
        let env = Env::new();
        let sub = env.dir.path().join("claude/projects/-tmp-proj/s1/subagents/agent-a1.jsonl");
        env.append(&sub, &format!("{LINE}\n"));
        let events = env.poll(&mut env.collector(None));
        assert_eq!(events[0].session.as_str(), "claude-code:agent-a1");
        assert_eq!(events[0].parent.as_ref().unwrap().as_str(), "claude-code:s1");
    }

    #[test]
    fn bad_lines_become_parse_errors_and_reading_continues() {
        let env = Env::new();
        env.append(&env.transcript(), &format!("{{broken\n{LINE}\n"));
        let mut collector = env.collector(None);
        let mut out = Vec::new();
        let stats = collector.poll(&mut out, |_| true).unwrap();
        assert_eq!(stats.parse_errors, 1);
        let events: Vec<Event> = String::from_utf8(out).unwrap().lines().map(|l| Event::from_line(l).unwrap()).collect();
        assert!(matches!(events[0].body, Body::Error { class: ErrorClass::Parse, .. }));
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn truncated_files_are_read_again() {
        let env = Env::new();
        env.append(&env.transcript(), &format!("{LINE}\n{LINE}\n"));
        let mut collector = env.collector(None);
        assert_eq!(env.poll(&mut collector).len(), 3);
        fs::write(env.transcript(), format!("{LINE}\n")).unwrap();
        assert_eq!(env.poll(&mut collector).len(), 2);
    }

    #[test]
    fn since_skips_old_unseen_files() {
        let env = Env::new();
        env.append(&env.transcript(), &format!("{LINE}\n"));
        let future = SystemTime::now() + Duration::from_secs(3600);
        assert!(env.poll(&mut env.collector(Some(future))).is_empty());
    }
}
```

Add `clippy::arithmetic_side_effects` to that test module's `#![allow(...)]` list (the `SystemTime + Duration` line).

Create `crates/ovs-collect-claude/src/cursor.rs` with only tests:

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn missing_cursor_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cursor = Cursor::load(&dir.path().join("none.json")).unwrap();
        assert!(cursor.files.is_empty() && cursor.live.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/cursor.json");
        let mut cursor = Cursor::default();
        cursor.files.insert(PathBuf::from("/p/s1.jsonl"), FileState { offset: 42, started: true, ..FileState::default() });
        cursor.save(&path).unwrap();
        let loaded = Cursor::load(&path).unwrap();
        assert_eq!(loaded.files[&PathBuf::from("/p/s1.jsonl")].offset, 42);
        assert!(!path.with_extension("tmp").exists());
    }

    #[test]
    fn corrupt_cursor_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cursor.json");
        fs::write(&path, "{").unwrap();
        assert!(Cursor::load(&path).is_err());
    }
}
```

Change `crates/ovs-collect-claude/src/lib.rs` to:

```rust
//! Reads Claude Code's on-disk session store and turns it into `ovs` events.

mod collector;
mod cursor;
mod record;
mod registry;

pub use collector::{CollectError, Collector, PollStats};
pub use cursor::{Cursor, CursorError};
pub use record::{FileState, LineContext, LineError, parse_line};
pub use registry::{LiveEntry, RegistryError, diff_registry, pid_alive, read_registry};
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `nix develop -c cargo test -p ovs-collect-claude`
Expected: compile errors, `cannot find type Collector`, `Cursor`.

- [ ] **Step 3: Implement the cursor**

Prepend to `crates/ovs-collect-claude/src/cursor.rs`:

```rust
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::{fs, io};

use serde::{Deserialize, Serialize};

use crate::{FileState, LiveEntry};

/// Everything the collector needs to resume after a restart.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Cursor {
    #[serde(default)]
    pub files: BTreeMap<PathBuf, FileState>,
    #[serde(default)]
    pub live: BTreeMap<String, LiveEntry>,
}

#[derive(Debug, thiserror::Error)]
pub enum CursorError {
    #[error("cannot read cursor {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cursor {path} is not valid JSON; delete it to re-read every transcript")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("cannot create cursor directory {path}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot encode cursor")]
    Encode {
        #[source]
        source: serde_json::Error,
    },
    #[error("cannot write cursor {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot move {from} to {to}")]
    Rename {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl Cursor {
    pub fn load(path: &Path) -> Result<Self, CursorError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => return Err(CursorError::Read { path: path.to_owned(), source }),
        };
        serde_json::from_str(&text).map_err(|source| CursorError::Parse { path: path.to_owned(), source })
    }

    pub fn save(&self, path: &Path) -> Result<(), CursorError> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|source| CursorError::CreateDir { path: dir.to_owned(), source })?;
        }
        let text = serde_json::to_string(self).map_err(|source| CursorError::Encode { source })?;
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, text).map_err(|source| CursorError::Write { path: tmp.clone(), source })?;
        fs::rename(&tmp, path).map_err(|source| CursorError::Rename { from: tmp, to: path.to_owned(), source })
    }
}
```

- [ ] **Step 4: Implement the collector**

Prepend to `crates/ovs-collect-claude/src/collector.rs`:

```rust
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ovs_event::{Body, ErrorClass, Event, EventError, Host, SessionKey, Snippet, Source, Timestamp};

use crate::cursor::{Cursor, CursorError};
use crate::record::{FileState, LineContext, build, parse_line};
use crate::registry::{RegistryError, diff_registry, read_registry};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PollStats {
    pub events: usize,
    pub parse_errors: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum CollectError {
    #[error("cannot list {path}")]
    Discover {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot open transcript {path}")]
    Open {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot read transcript {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("transcript {path} offset overflowed u64")]
    OffsetOverflow { path: PathBuf },
    #[error("cannot encode event")]
    Encode {
        #[source]
        source: EventError,
    },
    #[error("cannot write events")]
    Write {
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Cursor(#[from] CursorError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
}

struct Transcript {
    path: PathBuf,
    native: String,
    session: SessionKey,
    parent: Option<SessionKey>,
}

pub struct Collector {
    root: PathBuf,
    host: Host,
    cursor_path: PathBuf,
    since: Option<SystemTime>,
    cursor: Cursor,
}

impl Collector {
    pub fn open(
        root: PathBuf,
        host: Host,
        cursor_path: PathBuf,
        since: Option<SystemTime>,
    ) -> Result<Self, CollectError> {
        let cursor = Cursor::load(&cursor_path)?;
        Ok(Self { root, host, cursor_path, since, cursor })
    }

    pub fn poll(
        &mut self,
        out: &mut impl Write,
        is_alive: impl Fn(u32) -> bool,
    ) -> Result<PollStats, CollectError> {
        let mut stats = PollStats::default();
        for transcript in discover(&self.root.join("projects"))? {
            if !self.cursor.files.contains_key(&transcript.path)
                && self.older_than_since(&transcript.path)?
            {
                continue;
            }
            let file = self.cursor.files.entry(transcript.path.clone()).or_default();
            let events = read_new_lines(&self.host, &transcript, file, &mut stats)?;
            write_events(out, &events)?;
        }
        let live = read_registry(&self.root.join("sessions"))?;
        let (events, next) = diff_registry(&self.host, &self.cursor.live, &live, is_alive, Timestamp::now());
        write_events(out, &events)?;
        stats.events = stats.events.saturating_add(events.len());
        self.cursor.live = next;
        out.flush().map_err(|source| CollectError::Write { source })?;
        self.cursor.save(&self.cursor_path)?;
        Ok(stats)
    }

    fn older_than_since(&self, path: &Path) -> Result<bool, CollectError> {
        let Some(since) = self.since else {
            return Ok(false);
        };
        let modified = fs::metadata(path)
            .and_then(|meta| meta.modified())
            .map_err(|source| CollectError::Read { path: path.to_owned(), source })?;
        Ok(modified < since)
    }
}

fn list_dir(dir: &Path) -> Result<Vec<PathBuf>, CollectError> {
    match fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|entry| {
                entry
                    .map(|e| e.path())
                    .map_err(|source| CollectError::Discover { path: dir.to_owned(), source })
            })
            .collect(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(source) => Err(CollectError::Discover { path: dir.to_owned(), source }),
    }
}

fn jsonl_stem(path: &Path) -> Option<&str> {
    (path.extension() == Some(OsStr::new("jsonl")))
        .then(|| path.file_stem().and_then(OsStr::to_str))
        .flatten()
}

fn discover(projects: &Path) -> Result<Vec<Transcript>, CollectError> {
    let mut found = Vec::new();
    for project in list_dir(projects)?.into_iter().filter(|p| p.is_dir()) {
        for path in list_dir(&project)? {
            if let Some(stem) = jsonl_stem(&path) {
                found.push(Transcript {
                    native: stem.to_owned(),
                    session: SessionKey::new(Source::ClaudeCode, stem),
                    parent: None,
                    path: path.clone(),
                });
            } else if path.is_dir()
                && let Some(parent_id) = path.file_name().and_then(OsStr::to_str)
            {
                let parent = SessionKey::new(Source::ClaudeCode, parent_id);
                for sub in list_dir(&path.join("subagents"))? {
                    if let Some(stem) = jsonl_stem(&sub) {
                        found.push(Transcript {
                            native: stem.to_owned(),
                            session: SessionKey::new(Source::ClaudeCode, stem),
                            parent: Some(parent.clone()),
                            path: sub.clone(),
                        });
                    }
                }
            }
        }
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(found)
}

fn read_new_lines(
    host: &Host,
    transcript: &Transcript,
    file: &mut FileState,
    stats: &mut PollStats,
) -> Result<Vec<Event>, CollectError> {
    let path = &transcript.path;
    let len = fs::metadata(path)
        .map_err(|source| CollectError::Read { path: path.clone(), source })?
        .len();
    if len < file.offset {
        *file = FileState::default();
    }
    if len == file.offset {
        return Ok(Vec::new());
    }
    let mut handle = File::open(path).map_err(|source| CollectError::Open { path: path.clone(), source })?;
    handle
        .seek(SeekFrom::Start(file.offset))
        .map_err(|source| CollectError::Read { path: path.clone(), source })?;
    let mut buf = Vec::new();
    handle
        .read_to_end(&mut buf)
        .map_err(|source| CollectError::Read { path: path.clone(), source })?;

    let path_text = path.to_string_lossy();
    let mut events = Vec::new();
    let mut rest = buf.as_slice();
    while let Some(newline) = rest.iter().position(|byte| *byte == b'\n') {
        let (line, tail) = rest.split_at(newline);
        rest = tail.get(1..).unwrap_or_default();
        let offset = file.offset;
        let ctx = LineContext {
            host,
            native_session: &transcript.native,
            session: &transcript.session,
            parent: transcript.parent.as_ref(),
            path: &path_text,
            offset,
        };
        let parsed = match std::str::from_utf8(line) {
            Ok(text) if text.trim().is_empty() => Ok(Vec::new()),
            Ok(text) => parse_line(&ctx, text, file).map_err(|err| err.to_string()),
            Err(err) => Err(format!("{path_text}:{offset}: line is not UTF-8: {err}")),
        };
        match parsed {
            Ok(mut found) => events.append(&mut found),
            Err(message) => {
                stats.parse_errors = stats.parse_errors.saturating_add(1);
                tracing::warn!(path = %path_text, offset, %message, "skipping unparseable transcript line");
                let body = Body::Error { class: ErrorClass::Parse, message: Snippet::new(&message) };
                events.push(build(&ctx, Timestamp::now(), format!("{offset}:parse"), body));
            }
        }
        file.offset = u64::try_from(newline)
            .ok()
            .and_then(|len| len.checked_add(1))
            .and_then(|len| offset.checked_add(len))
            .ok_or_else(|| CollectError::OffsetOverflow { path: path.clone() })?;
    }
    stats.events = stats.events.saturating_add(events.len());
    Ok(events)
}

fn write_events(out: &mut impl Write, events: &[Event]) -> Result<(), CollectError> {
    for event in events {
        let line = event.to_line().map_err(|source| CollectError::Encode { source })?;
        writeln!(out, "{line}").map_err(|source| CollectError::Write { source })?;
    }
    Ok(())
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `nix develop -c cargo test -p ovs-collect-claude`
Expected: 22 passed.

Run: `nix develop -c cargo clippy -p ovs-collect-claude --all-targets -- -D warnings && nix develop -c cargo fmt`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/ovs-collect-claude
git commit -m "feat(ovs-collect-claude): tail transcripts from a persisted cursor"
```

---

### Task 8: `ovs collect claude` binary

**Files:**
- Modify: `Cargo.toml` (add member `crates/ovs-cli`), `flake.nix`, `README.md`
- Create: `crates/ovs-cli/Cargo.toml`, `crates/ovs-cli/src/main.rs`, `crates/ovs-cli/tests/collect_claude.rs`

**Interfaces:**
- Consumes: `Collector`, `pid_alive` (Task 7), `Host`, `Timestamp` (Task 1).
- Produces: binary `ovs`; `ovs collect claude [--root DIR] [--cursor FILE] [--host NAME] [--interval-ms N] [--once] [--since RFC3339]`. Env: `OVS_CLAUDE_ROOT`, `OVS_CURSOR`, `OVS_HOST`, `OVS_LOG`. Defaults: root `$HOME/.claude`; cursor `$XDG_STATE_HOME/ovs/collect-claude.json`, else `$HOME/.local/state/ovs/collect-claude.json`; host = system hostname; interval 1000 ms.

- [ ] **Step 1: Scaffold and write the failing test**

Add `"crates/ovs-cli",` to workspace `members`.

Create `crates/ovs-cli/Cargo.toml`:

```toml
[package]
name = "ovs-cli"
description = "The ovs command: composable agent monitor tools"
version.workspace = true
edition.workspace = true
publish.workspace = true
license.workspace = true

[[bin]]
name = "ovs"
path = "src/main.rs"

[dependencies]
anyhow = "1.0.104"
clap = { version = "4.6.7", features = ["derive", "env"] }
gethostname = "1"
ovs-collect-claude = { version = "0.1.0", path = "../ovs-collect-claude" }
ovs-event = { version = "0.1.0", path = "../ovs-event" }
tracing = "0.1.44"
tracing-subscriber = { version = "0.3.23", features = ["env-filter"] }

[dev-dependencies]
assert_cmd = "2.2.2"
serde_json = "1.0.154"
tempfile = "3.27.0"

[lints]
workspace = true
```

Create `crates/ovs-cli/tests/collect_claude.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;

const MAIN: &str = concat!(
    r#"{"type":"user","timestamp":"2026-10-20T05:00:00.000Z","sessionId":"s1","cwd":"/tmp/proj","gitBranch":"main","message":{"role":"user","content":"fix the parser"}}"#,
    "\n",
    r#"{"type":"assistant","timestamp":"2026-10-20T05:00:01.000Z","sessionId":"s1","cwd":"/tmp/proj","message":{"id":"msg_1","model":"claude-opus-5-5","content":[{"type":"text","text":"Running tests"}],"usage":{"input_tokens":10,"output_tokens":5}}}"#,
    "\n",
    r#"{"type":"assistant","timestamp":"2026-10-20T05:00:01.000Z","sessionId":"s1","cwd":"/tmp/proj","message":{"id":"msg_1","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo test"}}],"usage":{"input_tokens":10,"output_tokens":5}}}"#,
    "\n",
    r#"{"type":"user","timestamp":"2026-10-20T05:00:02.000Z","sessionId":"s1","cwd":"/tmp/proj","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","is_error":false,"content":"ok"}]}}"#,
    "\n",
    r#"{"type":"system","subtype":"turn_duration","timestamp":"2026-10-20T05:00:03.000Z","sessionId":"s1","cwd":"/tmp/proj"}"#,
    "\n",
);

const SUB: &str = concat!(
    r#"{"type":"user","timestamp":"2026-10-20T05:00:01.500Z","sessionId":"s1","agentId":"a1","isSidechain":true,"cwd":"/tmp/proj","message":{"role":"user","content":"look up the schema"}}"#,
    "\n",
);

fn run(root: &Path, cursor: &Path) -> Vec<Value> {
    let output = Command::cargo_bin("ovs")
        .unwrap()
        .args(["collect", "claude", "--once", "--host", "test-host"])
        .arg("--root")
        .arg(root)
        .arg("--cursor")
        .arg(cursor)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn collects_main_and_subagent_transcripts_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("claude");
    let project = root.join("projects/-tmp-proj");
    fs::create_dir_all(project.join("s1/subagents")).unwrap();
    fs::write(project.join("s1.jsonl"), MAIN).unwrap();
    fs::write(project.join("s1/subagents/agent-a1.jsonl"), SUB).unwrap();
    let cursor = dir.path().join("state/cursor.json");

    let events = run(&root, &cursor);
    let seen: Vec<(&str, &str)> = events
        .iter()
        .map(|e| (e["session"].as_str().unwrap(), e["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(
        seen,
        [
            ("claude-code:s1", "session.started"),
            ("claude-code:s1", "prompt"),
            ("claude-code:s1", "message"),
            ("claude-code:s1", "usage"),
            ("claude-code:s1", "tool.called"),
            ("claude-code:s1", "tool.result"),
            ("claude-code:s1", "session.status"),
            ("claude-code:agent-a1", "session.started"),
            ("claude-code:agent-a1", "prompt"),
        ]
    );
    assert_eq!(events[8]["parent"], "claude-code:s1");
    assert!(events.iter().all(|e| e["host"] == "test-host" && e["v"] == 1));

    assert!(run(&root, &cursor).is_empty());
}

#[test]
fn rejects_a_bad_since() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("ovs")
        .unwrap()
        .args(["collect", "claude", "--once", "--since", "yesterday"])
        .arg("--root")
        .arg(dir.path())
        .arg("--cursor")
        .arg(dir.path().join("c.json"))
        .assert()
        .failure()
        .stderr(predicates::str::contains("--since is not an RFC 3339 time"));
}
```

Add `predicates = "3"` to `[dev-dependencies]` in `crates/ovs-cli/Cargo.toml` (assert_cmd's `stderr` matcher takes a predicate).

- [ ] **Step 2: Run the test to verify it fails**

Run: `nix develop -c cargo test -p ovs-cli`
Expected: FAIL to build, `src/main.rs` missing.

- [ ] **Step 3: Implement**

Create `crates/ovs-cli/src/main.rs`:

```rust
//! `ovs`: composable agent monitor commands.

use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use ovs_collect_claude::{Collector, pid_alive};
use ovs_event::{Host, Timestamp};

/// One second keeps the live view current; re-statting ~1000 transcripts per poll costs little.
const DEFAULT_INTERVAL_MS: u64 = 1000;

#[derive(Debug, Parser)]
#[command(name = "ovs", version, about = "Composable agent monitor commands")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Read one agent tool's local store and print events as NDJSON on stdout.
    #[command(subcommand)]
    Collect(CollectSource),
}

#[derive(Debug, Subcommand)]
enum CollectSource {
    /// Claude Code transcripts and live sessions.
    Claude(ClaudeArgs),
}

#[derive(Debug, Args)]
struct ClaudeArgs {
    /// Claude Code data directory [default: $HOME/.claude]
    #[arg(long, env = "OVS_CLAUDE_ROOT")]
    root: Option<PathBuf>,
    /// Cursor file [default: $XDG_STATE_HOME/ovs/collect-claude.json]
    #[arg(long, env = "OVS_CURSOR")]
    cursor: Option<PathBuf>,
    /// Host name stamped on every event [default: system hostname]
    #[arg(long, env = "OVS_HOST")]
    host: Option<String>,
    /// Milliseconds between polls.
    #[arg(long, default_value_t = DEFAULT_INTERVAL_MS)]
    interval_ms: u64,
    /// Poll once and exit.
    #[arg(long)]
    once: bool,
    /// Skip transcripts not seen before and last modified before this RFC 3339 time.
    #[arg(long)]
    since: Option<String>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("OVS_LOG"))
        .with_writer(io::stderr)
        .init();
    match Cli::parse().command {
        Command::Collect(CollectSource::Claude(args)) => collect_claude(args),
    }
}

fn collect_claude(args: ClaudeArgs) -> Result<()> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let root = match args.root {
        Some(root) => root,
        None => home.as_deref().context("HOME is unset; pass --root")?.join(".claude"),
    };
    let cursor = match args.cursor {
        Some(cursor) => cursor,
        None => state_dir(home.as_deref())?.join("collect-claude.json"),
    };
    let host = Host::new(
        args.host
            .unwrap_or_else(|| gethostname::gethostname().to_string_lossy().into_owned()),
    );
    let since = args
        .since
        .as_deref()
        .map(Timestamp::parse)
        .transpose()
        .context("--since is not an RFC 3339 time")?
        .map(Timestamp::to_system_time);

    let mut collector = Collector::open(root.clone(), host, cursor.clone(), since)
        .with_context(|| format!("cannot open cursor {}", cursor.display()))?;
    let mut out = io::stdout().lock();
    loop {
        let stats = collector
            .poll(&mut out, pid_alive)
            .with_context(|| format!("collecting from {}", root.display()))?;
        tracing::debug!(events = stats.events, parse_errors = stats.parse_errors, "poll finished");
        if args.once {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(args.interval_ms));
    }
}

fn state_dir(home: Option<&Path>) -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME").filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir).join("ovs"));
    }
    Ok(home.context("HOME is unset; pass --cursor")?.join(".local/state/ovs"))
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `nix develop -c cargo test -p ovs-cli`
Expected: 2 passed.

- [ ] **Step 5: Build `ovs` in the flake and document it**

In `flake.nix`, change:

```nix
            cargoBuildFlags = [ "-p" "overseer-cli" ];
```

to:

```nix
            cargoBuildFlags = [ "-p" "overseer-cli" "-p" "ovs-cli" ];
```

In `README.md`, insert this section directly before `## Build and test`:

~~~markdown
## Agent monitor (`ovs`)

`ovs` is the start of overseer's redesign into small commands joined by one
NDJSON event stream (ADR 0011, design in
`docs/superpowers/specs/2026-10-20-agent-monitor-design.md`). Today it has one
subcommand:

```sh
ovs collect claude --once | jq -c '{session, kind}'
ovs collect claude --since 2026-10-20T00:00:00Z     # follow, skipping old transcripts
```

It reads `~/.claude/projects/**.jsonl` and `~/.claude/sessions/*.json`, prints
one event per line on stdout, and keeps its position in
`$XDG_STATE_HOME/ovs/collect-claude.json`, so a restart continues where it
stopped. Set `OVS_LOG=debug` to see per-poll counts on stderr.
~~~

- [ ] **Step 6: Verify the whole workspace and the Nix build**

Run: `nix develop -c cargo test --workspace`
Expected: all tests pass, including the existing overseer suites.

Run: `nix develop -c cargo clippy --workspace --all-targets -- -D warnings && nix develop -c cargo fmt --check`
Expected: clean.

Run: `nix build && ./result/bin/ovs collect claude --once --cursor "$(mktemp -d)/c.json" | head -3`
Expected: three JSON lines from this machine's real Claude Code sessions, each with `"v":1` and `"source":"claude-code"`.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock flake.nix README.md crates/ovs-cli
git commit -m "feat(ovs-cli): add ovs collect claude"
```

---

## Out of scope for M1 (planned in later milestones)

- `ovs ship`, `ovs serve`, `EventStore` (M2); web UI (M3); opencode and agy collectors (M4); `claude-hook`, `alert`, `notify`, time-driven state transitions (M5); Postgres, history, cost, home-manager module (M6).
