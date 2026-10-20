use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ovs_event::{
    Body, ErrorClass, Event, EventError, Host, SessionKey, Snippet, Source, Timestamp,
};

use crate::cursor::{Cursor, CursorError};
use crate::record::{FileState, LineContext, build, parse_line};
use crate::registry::{RegistryError, diff_registry, read_registry};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PollStats {
    pub events: usize,
    pub parse_errors: usize,
    pub read_errors: usize,
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

impl CollectError {
    fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::Open { source, .. } | Self::Read { source, .. }
                if source.kind() == io::ErrorKind::NotFound
        )
    }
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
    reported: HashSet<PathBuf>,
}

impl Collector {
    pub fn open(
        root: PathBuf,
        host: Host,
        cursor_path: PathBuf,
        since: Option<SystemTime>,
    ) -> Result<Self, CollectError> {
        let cursor = Cursor::load(&cursor_path)?;
        Ok(Self {
            root,
            host,
            cursor_path,
            since,
            cursor,
            reported: HashSet::new(),
        })
    }

    pub fn poll(
        &mut self,
        out: &mut impl Write,
        is_alive: impl Fn(u32) -> bool,
    ) -> Result<PollStats, CollectError> {
        let mut stats = PollStats::default();
        let mut cursor = self.cursor.clone();
        let mut reported = self.reported.clone();
        let discovered = discover(&self.root.join("projects"), &mut stats)?;
        let known: HashSet<PathBuf> = discovered.iter().map(|t| t.path.clone()).collect();
        cursor.files.retain(|path, _| known.contains(path));
        reported.retain(|path| known.contains(path));
        for transcript in discovered {
            match self.read_transcript(&transcript, &mut cursor, &mut stats) {
                Ok(events) => {
                    reported.remove(&transcript.path);
                    write_events(out, &events)?;
                }
                Err(err) if err.is_not_found() => {
                    tracing::debug!(
                        path = %transcript.path.display(),
                        error = %err,
                        "transcript vanished before it was read"
                    );
                    cursor.files.remove(&transcript.path);
                    reported.remove(&transcript.path);
                }
                Err(err) => {
                    stats.read_errors = stats.read_errors.saturating_add(1);
                    tracing::warn!(
                        path = %transcript.path.display(),
                        error = %err,
                        "skipping unreadable transcript"
                    );
                    if reported.insert(transcript.path.clone()) {
                        let offset = cursor
                            .files
                            .get(&transcript.path)
                            .map_or(0, |file| file.offset);
                        let event = read_error_event(&self.host, &transcript, offset, &err);
                        stats.events = stats.events.saturating_add(1);
                        write_events(out, &[event])?;
                    }
                }
            }
        }
        let live = read_registry(&self.root.join("sessions"))?;
        let (events, next) =
            diff_registry(&self.host, &cursor.live, &live, is_alive, Timestamp::now());
        write_events(out, &events)?;
        stats.events = stats.events.saturating_add(events.len());
        cursor.live = next;
        out.flush()
            .map_err(|source| CollectError::Write { source })?;
        cursor.save(&self.cursor_path)?;
        self.cursor = cursor;
        self.reported = reported;
        Ok(stats)
    }

    fn read_transcript(
        &self,
        transcript: &Transcript,
        cursor: &mut Cursor,
        stats: &mut PollStats,
    ) -> Result<Vec<Event>, CollectError> {
        if !cursor.files.contains_key(&transcript.path)
            && self.older_than_since(&transcript.path)?
        {
            return Ok(Vec::new());
        }
        let file = cursor.files.entry(transcript.path.clone()).or_default();
        read_new_lines(&self.host, transcript, file, stats)
    }

    fn older_than_since(&self, path: &Path) -> Result<bool, CollectError> {
        let Some(since) = self.since else {
            return Ok(false);
        };
        let modified = fs::metadata(path)
            .and_then(|meta| meta.modified())
            .map_err(|source| CollectError::Read {
                path: path.to_owned(),
                source,
            })?;
        Ok(modified < since)
    }
}

fn list_dir(dir: &Path) -> Result<Vec<PathBuf>, CollectError> {
    match fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|entry| {
                entry
                    .map(|e| e.path())
                    .map_err(|source| CollectError::Discover {
                        path: dir.to_owned(),
                        source,
                    })
            })
            .collect(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(source) => Err(CollectError::Discover {
            path: dir.to_owned(),
            source,
        }),
    }
}

fn jsonl_stem(path: &Path) -> Option<&str> {
    (path.extension() == Some(OsStr::new("jsonl")))
        .then(|| path.file_stem().and_then(OsStr::to_str))
        .flatten()
}

fn list_or_count(dir: &Path, stats: &mut PollStats) -> Vec<PathBuf> {
    list_dir(dir).unwrap_or_else(|err| {
        stats.read_errors = stats.read_errors.saturating_add(1);
        tracing::warn!(
            path = %dir.display(),
            error = %err,
            "skipping directory that cannot be listed"
        );
        Vec::new()
    })
}

fn discover(projects: &Path, stats: &mut PollStats) -> Result<Vec<Transcript>, CollectError> {
    let mut found = Vec::new();
    for project in list_dir(projects)?.into_iter().filter(|p| p.is_dir()) {
        for path in list_or_count(&project, stats) {
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
                for sub in list_or_count(&path.join("subagents"), stats) {
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
    // Byte order puts <sid>.jsonl before <sid>/subagents/, so a parent session is read before its subagents.
    found.sort_by(|a, b| a.path.as_os_str().cmp(b.path.as_os_str()));
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
        .map_err(|source| CollectError::Read {
            path: path.clone(),
            source,
        })?
        .len();
    if len < file.offset {
        *file = FileState::default();
    }
    if len == file.offset {
        return Ok(Vec::new());
    }
    let mut handle = File::open(path).map_err(|source| CollectError::Open {
        path: path.clone(),
        source,
    })?;
    handle
        .seek(SeekFrom::Start(file.offset))
        .map_err(|source| CollectError::Read {
            path: path.clone(),
            source,
        })?;
    let mut buf = Vec::new();
    handle
        .read_to_end(&mut buf)
        .map_err(|source| CollectError::Read {
            path: path.clone(),
            source,
        })?;

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
                let body = Body::Error {
                    class: ErrorClass::Parse,
                    message: Snippet::new(&message),
                };
                events.push(build(
                    &ctx,
                    Timestamp::now(),
                    format!("{offset}:parse"),
                    body,
                ));
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

fn read_error_event(
    host: &Host,
    transcript: &Transcript,
    offset: u64,
    err: &CollectError,
) -> Event {
    let path = transcript.path.to_string_lossy();
    let ctx = LineContext {
        host,
        native_session: &transcript.native,
        session: &transcript.session,
        parent: transcript.parent.as_ref(),
        path: &path,
        offset,
    };
    let body = Body::Error {
        class: ErrorClass::Other,
        message: Snippet::new(&err_with_sources(err)),
    };
    build(&ctx, Timestamp::now(), format!("{offset}:read-error"), body)
}

fn err_with_sources(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn write_events(out: &mut impl Write, events: &[Event]) -> Result<(), CollectError> {
    for event in events {
        let line = event
            .to_line()
            .map_err(|source| CollectError::Encode { source })?;
        writeln!(out, "{line}").map_err(|source| CollectError::Write { source })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]

    use std::fs::{self, OpenOptions};
    use std::os::unix::fs::PermissionsExt;
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
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
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
            String::from_utf8(out)
                .unwrap()
                .lines()
                .map(|l| Event::from_line(l).unwrap())
                .collect()
        }
    }

    #[test]
    fn unlistable_project_is_skipped_and_counted() {
        let env = Env::new();
        let blocked = env.dir.path().join("claude/projects/-tmp-blocked");
        fs::create_dir_all(&blocked).unwrap();
        env.append(&env.transcript(), &format!("{LINE}\n"));
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
        let mut collector = env.collector(None);
        let mut out = Vec::new();
        let result = collector.poll(&mut out, |_| true);
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();
        let stats = result.unwrap();
        assert_eq!(stats.read_errors, 1);
        assert_eq!(String::from_utf8(out).unwrap().lines().count(), 2);
    }

    #[test]
    fn partial_lines_wait_for_their_newline() {
        let env = Env::new();
        let mut collector = env.collector(None);
        let (head, tail) = LINE.split_at(40);
        env.append(&env.transcript(), head);
        assert!(env.poll(&mut collector).is_empty());
        env.append(&env.transcript(), &format!("{tail}\n"));
        let kinds: Vec<_> = env
            .poll(&mut collector)
            .iter()
            .map(|e| e.body.kind())
            .collect();
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
        let sub = env
            .dir
            .path()
            .join("claude/projects/-tmp-proj/s1/subagents/agent-a1.jsonl");
        env.append(&sub, &format!("{LINE}\n"));
        let events = env.poll(&mut env.collector(None));
        assert_eq!(events[0].session.as_str(), "claude-code:agent-a1");
        assert_eq!(
            events[0].parent.as_ref().unwrap().as_str(),
            "claude-code:s1"
        );
    }

    #[test]
    fn bad_lines_become_parse_errors_and_reading_continues() {
        let env = Env::new();
        env.append(&env.transcript(), &format!("{{broken\n{LINE}\n"));
        let mut collector = env.collector(None);
        let mut out = Vec::new();
        let stats = collector.poll(&mut out, |_| true).unwrap();
        assert_eq!(stats.parse_errors, 1);
        let events: Vec<Event> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| Event::from_line(l).unwrap())
            .collect();
        assert!(matches!(
            events[0].body,
            Body::Error {
                class: ErrorClass::Parse,
                ..
            }
        ));
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

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("disk full"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn failed_write_leaves_lines_for_the_next_poll() {
        let env = Env::new();
        env.append(&env.transcript(), &format!("{LINE}\n"));
        let mut collector = env.collector(None);
        assert!(collector.poll(&mut FailingWriter, |_| true).is_err());
        let kinds: Vec<_> = env
            .poll(&mut collector)
            .iter()
            .map(|e| e.body.kind())
            .collect();
        assert_eq!(kinds, ["session.started", "prompt"]);
    }

    #[test]
    fn unreadable_transcript_is_reported_once_and_others_continue() {
        let env = Env::new();
        let project = env.dir.path().join("claude/projects/-tmp-proj");
        let a = project.join("a.jsonl");
        let b = project.join("b.jsonl");
        env.append(&a, &format!("{LINE}\n"));
        env.append(&b, &format!("{LINE}\n"));
        fs::set_permissions(&a, fs::Permissions::from_mode(0o000)).unwrap();
        let mut collector = env.collector(None);
        let mut out = Vec::new();
        let stats = collector.poll(&mut out, |_| true).unwrap();
        let events: Vec<Event> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| Event::from_line(l).unwrap())
            .collect();
        let errors: Vec<&Event> = events
            .iter()
            .filter(|e| {
                matches!(
                    e.body,
                    Body::Error {
                        class: ErrorClass::Other,
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].session.as_str(), "claude-code:a");
        let b_kinds: Vec<_> = events
            .iter()
            .filter(|e| e.session.as_str() == "claude-code:b")
            .map(|e| e.body.kind())
            .collect();
        assert_eq!(b_kinds, ["session.started", "prompt"]);
        assert_eq!(events.len(), 3);
        assert_eq!(stats.read_errors, 1);
        assert!(env.poll(&mut collector).is_empty());
        fs::set_permissions(&a, fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn deleted_transcripts_leave_the_cursor() {
        let env = Env::new();
        env.append(&env.transcript(), &format!("{LINE}\n"));
        assert_eq!(env.poll(&mut env.collector(None)).len(), 2);
        fs::remove_file(env.transcript()).unwrap();
        let mut collector = env.collector(None);
        assert!(env.poll(&mut collector).is_empty());
        let cursor = Cursor::load(&env.dir.path().join("state/cursor.json")).unwrap();
        assert!(cursor.files.is_empty());
    }
}
