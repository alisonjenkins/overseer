use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
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
    pub status_updated_at: Option<String>,
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
        Err(source) => {
            return Err(RegistryError {
                path: dir.to_owned(),
                source,
            });
        }
    };
    let mut live = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|source| RegistryError {
                path: dir.to_owned(),
                source,
            })?
            .path();
        if path.extension() != Some(OsStr::new("json")) {
            continue;
        }
        // Claude Code rewrites these files in place, so a half-written one is expected now and then.
        match fs::read_to_string(&path)
            .ok()
            .and_then(|text| parse_entry(&text))
        {
            Some(parsed) => live.push(parsed),
            None => {
                tracing::debug!(path = %path.display(), "skipping unreadable live-session file")
            }
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
        status_updated_at: value.get("statusUpdatedAt").and_then(stamp_text),
    })
}

fn stamp_text(value: &Value) -> Option<String> {
    match value {
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) => Some(text.clone()),
        _ => None,
    }
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
            let ts = entry
                .status_updated_at
                .as_deref()
                .and_then(updated_at)
                .unwrap_or(now);
            let position = match &entry.status_updated_at {
                Some(stamp) => format!("status:{stamp}"),
                None => format!("status:{}:{}", entry.status, epoch_millis(now)),
            };
            events.push(registry_event(
                host,
                &entry.session_id,
                &position,
                ts,
                Body::SessionStatus { state },
            ));
        }
        next.insert(entry.session_id.clone(), entry.clone());
    }
    for (session_id, entry) in previous {
        if !next.contains_key(session_id) {
            let position = format!("ended:{}", entry.pid);
            let body = Body::SessionEnded {
                reason: EndReason::Exited,
            };
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
        Err(_) => Timestamp::parse(raw).ok(),
    }
}

fn epoch_millis(now: Timestamp) -> u128 {
    now.to_system_time()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis())
}

fn registry_event(
    host: &Host,
    session_id: &str,
    position: &str,
    ts: Timestamp,
    body: Body,
) -> Event {
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

    fn write_raw(dir: &Path, pid: u32, json: &str) {
        fs::write(dir.join(format!("{pid}.json")), json).unwrap();
    }

    #[test]
    fn missing_updated_at_still_emits_distinct_status_events() {
        let dir = tempfile::tempdir().unwrap();
        let host = Host::new("test-host");
        let first_now = Timestamp::from_epoch_millis(1_792_474_400_000).unwrap();
        let second_now = Timestamp::from_epoch_millis(1_792_474_405_000).unwrap();

        write_raw(
            dir.path(),
            123,
            r#"{"pid":123,"sessionId":"s123","status":"busy"}"#,
        );
        let (busy, live) = diff_registry(
            &host,
            &BTreeMap::new(),
            &read_registry(dir.path()).unwrap(),
            |_| true,
            first_now,
        );
        write_raw(
            dir.path(),
            123,
            r#"{"pid":123,"sessionId":"s123","status":"idle"}"#,
        );
        let (idle, _) = diff_registry(
            &host,
            &live,
            &read_registry(dir.path()).unwrap(),
            |_| true,
            second_now,
        );

        assert_eq!(busy.len(), 1);
        assert_eq!(idle.len(), 1);
        assert_ne!(busy[0].id, idle[0].id);
    }

    #[test]
    fn string_updated_at_is_unquoted_and_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let host = Host::new("test-host");
        let now = Timestamp::from_epoch_millis(1_792_474_400_000).unwrap();

        write_raw(
            dir.path(),
            123,
            r#"{"pid":123,"sessionId":"s123","status":"busy","statusUpdatedAt":"2026-10-20T05:33:12.123Z"}"#,
        );
        let (events, _) = diff_registry(
            &host,
            &BTreeMap::new(),
            &read_registry(dir.path()).unwrap(),
            |_| true,
            now,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].ts,
            Timestamp::parse("2026-10-20T05:33:12.123Z").unwrap()
        );
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
        assert_eq!(live[0].status_updated_at.as_deref(), Some("1792474392123"));
    }

    #[test]
    fn status_changes_and_exits_become_events() {
        let dir = tempfile::tempdir().unwrap();
        let host = Host::new("test-host");
        let now = Timestamp::from_epoch_millis(1_792_474_400_000).unwrap();

        write_entry(dir.path(), 123, "busy", 1_792_474_392_123);
        let (events, live) = diff_registry(
            &host,
            &BTreeMap::new(),
            &read_registry(dir.path()).unwrap(),
            |_| true,
            now,
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0].body,
            Body::SessionStatus {
                state: ActivityState::Working
            }
        ));
        assert_eq!(events[0].session.as_str(), "claude-code:s123");
        assert_eq!(events[0].ts.to_string(), "2026-10-20T05:33:12.123Z");

        let (events, live) = diff_registry(
            &host,
            &live,
            &read_registry(dir.path()).unwrap(),
            |_| true,
            now,
        );
        assert!(events.is_empty());

        write_entry(dir.path(), 123, "idle", 1_792_474_395_000);
        let (events, live) = diff_registry(
            &host,
            &live,
            &read_registry(dir.path()).unwrap(),
            |_| true,
            now,
        );
        assert!(matches!(
            events[0].body,
            Body::SessionStatus {
                state: ActivityState::Idle
            }
        ));

        let (events, live) = diff_registry(
            &host,
            &live,
            &read_registry(dir.path()).unwrap(),
            |_| false,
            now,
        );
        assert!(matches!(
            events[0].body,
            Body::SessionEnded {
                reason: EndReason::Exited
            }
        ));
        assert_eq!(events[0].ts, now);
        assert!(live.is_empty());
    }
}
