//! Session state derived from the event log.

use std::collections::BTreeMap;

use ovs_event::{
    ActivityState, Body, CallId, Event, Host, PromptBy, SessionKey, Snippet, Source, Timestamp,
    Usage,
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

    fn active_state(&self) -> State {
        if self.has_open_question() {
            State::WaitingInput
        } else {
            State::Working
        }
    }

    fn state_for(&self, reported: ActivityState) -> State {
        match reported {
            ActivityState::Working => self.active_state(),
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
        Body::SessionEnded { .. } => {
            s.open_calls.clear();
            s.state = State::Ended;
        }
        Body::SessionStatus { state } => s.state = s.state_for(*state),
        Body::Prompt { text, by } => {
            if matches!(by, PromptBy::User) {
                s.open_calls.clear();
            }
            s.state = s.active_state();
            s.activity = Some(text.clone());
        }
        Body::Message { text, model } => {
            s.state = s.active_state();
            s.activity = Some(text.clone());
            if model.is_some() {
                s.model = model.clone();
            }
        }
        Body::ToolCalled {
            call_id,
            name,
            input,
        } => {
            s.open_calls.insert(
                call_id.clone(),
                OpenCall {
                    name: name.clone(),
                    since: event.ts,
                },
            );
            s.activity = Some(Snippet::new(&format!("{name}: {}", input.as_str())));
            s.state = s.active_state();
        }
        Body::ToolResult { call_id, .. } => {
            s.open_calls.remove(call_id);
            s.state = s.active_state();
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
pub fn apply_all<'a>(
    events: impl IntoIterator<Item = &'a Event>,
) -> BTreeMap<SessionKey, SessionState> {
    let mut sessions = BTreeMap::new();
    for event in events {
        let prev = sessions.remove(&event.session);
        if let Some(next) = fold(prev, event) {
            sessions.insert(event.session.clone(), next);
        }
    }
    sessions
}

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
        Body::Prompt {
            text: Snippet::new(text),
            by: PromptBy::User,
        }
    }

    fn called(id: &str, name: &str) -> Body {
        Body::ToolCalled {
            call_id: CallId::new(id),
            name: name.to_owned(),
            input: Snippet::new("{}"),
        }
    }

    fn result(id: &str) -> Body {
        Body::ToolResult {
            call_id: CallId::new(id),
            ok: true,
            error: None,
            output: Snippet::new("ok"),
        }
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
            ev(
                "s1",
                3_000,
                Body::SessionStatus {
                    state: ActivityState::Idle,
                },
            ),
        ];
        let s = apply_all(&events)
            .remove(&SessionKey::new(Source::ClaudeCode, "s1"))
            .unwrap();
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
            ev(
                "s1",
                3_000,
                Body::SessionStatus {
                    state: ActivityState::Idle,
                },
            ),
        ];
        let s = apply_all(&events)
            .remove(&SessionKey::new(Source::ClaudeCode, "s1"))
            .unwrap();
        assert_eq!(s.state, State::Idle);
    }

    #[test]
    fn hook_status_sets_waiting_permission() {
        let s = fold(
            None,
            &ev(
                "s1",
                1_000,
                Body::SessionStatus {
                    state: ActivityState::WaitingPermission,
                },
            ),
        )
        .unwrap();
        assert_eq!(s.state, State::WaitingPermission);
    }

    #[test]
    fn usage_accumulates_and_sets_model() {
        let usage = |input, cost| {
            Body::Usage(Usage {
                model: Some("claude-opus-5-5".to_owned()),
                input,
                output: 5,
                cost_usd: cost,
                ..Usage::default()
            })
        };
        let events = [
            ev("s1", 1_000, usage(10, None)),
            ev("s1", 2_000, usage(20, Some(0.5))),
        ];
        let s = apply_all(&events)
            .remove(&SessionKey::new(Source::ClaudeCode, "s1"))
            .unwrap();
        assert_eq!(s.usage.input, 30);
        assert_eq!(s.usage.output, 10);
        assert_eq!(s.usage.cost_usd, Some(0.5));
        assert_eq!(s.model.as_deref(), Some("claude-opus-5-5"));
    }

    #[test]
    fn error_then_activity_recovers() {
        let err = Body::Error {
            class: ErrorClass::RateLimit,
            message: Snippet::new("429"),
        };
        let s = fold(None, &ev("s1", 1_000, err)).unwrap();
        assert_eq!((s.state, s.errors), (State::Errored, 1));
        let s = fold(Some(s), &ev("s1", 2_000, prompt("retry"))).unwrap();
        assert_eq!((s.state, s.errors), (State::Working, 1));
    }

    #[test]
    fn ended_session_revives_on_resume() {
        let s = fold(
            None,
            &ev(
                "s1",
                1_000,
                Body::SessionEnded {
                    reason: EndReason::Exited,
                },
            ),
        )
        .unwrap();
        assert_eq!(s.state, State::Ended);
        let s = fold(Some(s), &ev("s1", 2_000, prompt("again"))).unwrap();
        assert_eq!(s.state, State::Working);
    }

    #[test]
    fn last_event_at_never_moves_backwards() {
        let s = fold(None, &ev("s1", 5_000, prompt("a"))).unwrap();
        let s = fold(Some(s), &ev("s1", 1_000, prompt("b"))).unwrap();
        assert_eq!(
            s.last_event_at,
            Timestamp::from_epoch_millis(5_000).unwrap()
        );
    }

    #[test]
    fn host_events_do_not_create_sessions() {
        assert!(
            fold(
                None,
                &ev(
                    "s1",
                    1_000,
                    Body::HostHeartbeat {
                        collectors: Vec::new()
                    }
                )
            )
            .is_none()
        );
    }

    #[test]
    fn user_prompt_abandons_open_question() {
        let events = [
            ev("s1", 1_000, called("q1", "AskUserQuestion")),
            ev("s1", 2_000, prompt("never mind")),
            ev(
                "s1",
                3_000,
                Body::SessionStatus {
                    state: ActivityState::Idle,
                },
            ),
        ];
        let s = apply_all(&events)
            .remove(&SessionKey::new(Source::ClaudeCode, "s1"))
            .unwrap();
        assert_eq!(s.state, State::Idle);
        assert!(s.open_calls.is_empty());
    }

    #[test]
    fn session_end_clears_open_calls() {
        let events = [
            ev("s1", 1_000, called("t1", "Bash")),
            ev(
                "s1",
                2_000,
                Body::SessionEnded {
                    reason: EndReason::Exited,
                },
            ),
        ];
        let s = apply_all(&events)
            .remove(&SessionKey::new(Source::ClaudeCode, "s1"))
            .unwrap();
        assert!(s.open_calls.is_empty());
        assert_eq!(s.state, State::Ended);
    }

    #[test]
    fn activity_keeps_open_question_visible() {
        let s = fold(None, &ev("s1", 1_000, called("q1", "AskUserQuestion"))).unwrap();
        assert_eq!(s.state, State::WaitingInput);
        let s = fold(Some(s), &ev("s1", 2_000, called("t2", "Bash"))).unwrap();
        assert_eq!(s.state, State::WaitingInput);
        let s = fold(
            Some(s),
            &ev(
                "s1",
                3_000,
                Body::Message {
                    text: Snippet::new("working on it"),
                    model: None,
                },
            ),
        )
        .unwrap();
        assert_eq!(s.state, State::WaitingInput);
        let s = fold(Some(s), &ev("s1", 4_000, result("t2"))).unwrap();
        assert_eq!(s.state, State::WaitingInput);
        let s = fold(Some(s), &ev("s1", 5_000, result("q1"))).unwrap();
        assert_eq!(s.state, State::Working);
    }

    #[test]
    fn opencode_question_tool_waits_for_input() {
        let s = fold(None, &ev("s1", 1_000, called("q1", "question"))).unwrap();
        assert_eq!(s.state, State::WaitingInput);
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
