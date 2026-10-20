use std::collections::HashSet;

use ovs_event::{
    ActivityState, Body, CallId, ErrorClass, Event, EventId, Host, PromptBy, SchemaVersion,
    SessionKey, SessionStarted, Snippet, Source, SourceRef, Timestamp, Usage, redact,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const INTERRUPT_PREFIX: &str = "[Request interrupted by user";
const SYNTHETIC_MODEL: &str = "<synthetic>";
const REDACTED: &str = "[redacted]";

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
    let kind = record
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
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
        out.push(build(
            ctx,
            ts,
            format!("{}:started", ctx.offset),
            Body::SessionStarted(start),
        ));
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
            out.push(build(
                ctx,
                ts,
                format!("{}:0", ctx.offset),
                prompt_or_abort(text),
            ));
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
                                block
                                    .get("tool_use_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default(),
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
            Body::Error {
                class,
                message: Snippet::new(&text),
            },
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
                        let body = Body::Message {
                            text: Snippet::new(text),
                            model: model.clone(),
                        };
                        out.push(build(ctx, ts, position, body));
                    }
                }
                Some("tool_use") => {
                    let input = block
                        .get("input")
                        .map(|input| redact_json(input).to_string())
                        .unwrap_or_default();
                    let body = Body::ToolCalled {
                        call_id: CallId::new(
                            block.get("id").and_then(Value::as_str).unwrap_or_default(),
                        ),
                        name: block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        input: Snippet::new(&input),
                    };
                    out.push(build(ctx, ts, position, body));
                }
                _ => {}
            }
        }
    }
    // One API message is split into one line per content block, each repeating the usage.
    if let (Some(id), Some(usage)) = (
        message.get("id").and_then(Value::as_str),
        message.get("usage"),
    ) && file.seen_usage.insert(id.to_owned())
    {
        out.push(build(
            ctx,
            ts,
            format!("usage:{id}"),
            Body::Usage(usage_from(usage, model)),
        ));
    }
}

const SECRET_KEY_MARKERS: [&str; 8] = [
    "authorization",
    "token",
    "secret",
    "password",
    "apikey",
    "api_key",
    "api-key",
    "cookie",
];

fn is_secret_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SECRET_KEY_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

fn redact_json(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact(text)),
        Value::Array(items) => Value::Array(items.iter().map(redact_json).collect()),
        Value::Object(entries) => Value::Object(
            entries
                .iter()
                .map(|(key, entry)| {
                    let redacted = if is_secret_key(key) {
                        Value::String(REDACTED.to_owned())
                    } else {
                        redact_json(entry)
                    };
                    (key.clone(), redacted)
                })
                .collect(),
        ),
        other => other.clone(),
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
            Body::SessionStatus {
                state: ActivityState::Idle,
            },
        ));
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
    const TOOL_QUOTED_SECRET: &str = r#"{"type":"assistant","timestamp":"2026-10-20T05:00:06.000Z","sessionId":"s1","cwd":"/tmp/proj","message":{"id":"msg_3","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"toolu_2","name":"Bash","input":{"command":"API_KEY=\"a b\" run"}}]}}"#;
    const TOOL_SECRET_KEY: &str = r#"{"type":"assistant","timestamp":"2026-10-20T05:00:07.000Z","sessionId":"s1","cwd":"/tmp/proj","message":{"id":"msg_4","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"toolu_3","name":"WebFetch","input":{"headers":{"Authorization":"Bearer abc"},"url":"x"}}]}}"#;

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
        let Body::SessionStarted(start) = &events[0].body else {
            unreachable!()
        };
        assert_eq!(start.cwd.as_deref(), Some("/tmp/proj"));
        assert_eq!(start.git_branch.as_deref(), Some("main"));
        let Body::Prompt { text, by } = &events[1].body else {
            unreachable!()
        };
        assert_eq!((text.as_str(), *by), ("fix the parser", PromptBy::User));
        assert_eq!(events[1].source_ref.as_ref().unwrap().offset, 0);
        assert!(file.started);
        assert_eq!(kinds(&fx.parse(500, PROMPT, &mut file)), ["prompt"]);
    }

    #[test]
    fn meta_records_are_skipped() {
        let fx = Fixture::new();
        let mut file = FileState {
            started: true,
            ..FileState::default()
        };
        assert!(fx.parse(0, META, &mut file).is_empty());
    }

    #[test]
    fn usage_is_emitted_once_per_message_id() {
        let fx = Fixture::new();
        let mut file = FileState {
            started: true,
            ..FileState::default()
        };
        let first = fx.parse(100, ASSISTANT_TEXT, &mut file);
        assert_eq!(kinds(&first), ["message", "usage"]);
        let Body::Usage(usage) = &first[1].body else {
            unreachable!()
        };
        assert_eq!(
            (
                usage.input,
                usage.output,
                usage.cache_read,
                usage.cache_write,
                usage.reasoning
            ),
            (2, 265, 23327, 27640, 94)
        );
        assert_eq!(usage.model.as_deref(), Some("claude-opus-5-5"));
        let second = fx.parse(900, ASSISTANT_TOOL, &mut file);
        assert_eq!(kinds(&second), ["tool.called"]);
        let Body::ToolCalled { name, input, .. } = &second[0].body else {
            unreachable!()
        };
        assert_eq!(name, "Bash");
        assert_eq!(input.as_str(), r#"{"command":"cargo test"}"#);
    }

    #[test]
    fn usage_id_depends_on_message_not_offset() {
        let fx = Fixture::new();
        let a = fx.parse(
            100,
            ASSISTANT_TEXT,
            &mut FileState {
                started: true,
                ..FileState::default()
            },
        );
        let b = fx.parse(
            900,
            ASSISTANT_TOOL,
            &mut FileState {
                started: true,
                ..FileState::default()
            },
        );
        assert_eq!(a[1].id, b[1].id);
    }

    #[test]
    fn failed_tool_result_carries_the_error() {
        let fx = Fixture::new();
        let events = fx.parse(
            0,
            TOOL_ERROR,
            &mut FileState {
                started: true,
                ..FileState::default()
            },
        );
        let Body::ToolResult {
            call_id,
            ok,
            error,
            output,
        } = &events[0].body
        else {
            unreachable!()
        };
        assert_eq!(call_id.as_str(), "toolu_1");
        assert!(!ok);
        assert_eq!(error.as_ref().unwrap().as_str(), "exit 101");
        assert_eq!(output.as_str(), "exit 101");
    }

    #[test]
    fn interrupt_is_an_aborted_error() {
        let fx = Fixture::new();
        let events = fx.parse(
            0,
            INTERRUPT,
            &mut FileState {
                started: true,
                ..FileState::default()
            },
        );
        assert!(matches!(
            events[0].body,
            Body::Error {
                class: ErrorClass::Aborted,
                ..
            }
        ));
    }

    #[test]
    fn api_error_maps_rate_limit() {
        let fx = Fixture::new();
        let events = fx.parse(
            0,
            API_ERROR,
            &mut FileState {
                started: true,
                ..FileState::default()
            },
        );
        assert_eq!(kinds(&events), ["error"]);
        assert!(matches!(
            events[0].body,
            Body::Error {
                class: ErrorClass::RateLimit,
                ..
            }
        ));
    }

    #[test]
    fn turn_end_reports_idle() {
        let fx = Fixture::new();
        let events = fx.parse(
            0,
            TURN_END,
            &mut FileState {
                started: true,
                ..FileState::default()
            },
        );
        assert!(matches!(
            events[0].body,
            Body::SessionStatus {
                state: ActivityState::Idle
            }
        ));
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
        assert!(
            err.to_string()
                .starts_with("/p/s1.jsonl:7: line is not JSON")
        );
    }

    fn tool_input(events: &[Event]) -> String {
        match &events[0].body {
            Body::ToolCalled { input, .. } => input.as_str().to_owned(),
            _ => unreachable!(),
        }
    }

    #[test]
    fn tool_input_quoted_secret_is_redacted() {
        let fx = Fixture::new();
        let events = fx.parse(
            0,
            TOOL_QUOTED_SECRET,
            &mut FileState {
                started: true,
                ..FileState::default()
            },
        );
        let input = tool_input(&events);
        assert!(input.contains("[redacted]"), "{input}");
        assert!(!input.contains("a b"), "{input}");
        assert!(!input.contains(r#"b\""#), "{input}");
    }

    #[test]
    fn tool_input_secret_keys_are_redacted() {
        let fx = Fixture::new();
        let events = fx.parse(
            0,
            TOOL_SECRET_KEY,
            &mut FileState {
                started: true,
                ..FileState::default()
            },
        );
        let input = tool_input(&events);
        assert!(input.contains(r#""Authorization":"[redacted]""#), "{input}");
        assert!(!input.contains("abc"), "{input}");
    }

    #[test]
    fn ids_are_stable_across_reads() {
        let fx = Fixture::new();
        let a = fx.parse(0, PROMPT, &mut FileState::default());
        let b = fx.parse(0, PROMPT, &mut FileState::default());
        assert_eq!(a, b);
    }
}
