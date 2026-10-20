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
    Message {
        text: Snippet,
        model: Option<String>,
    },
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn kind_names_match_the_wire() {
        assert_eq!(Body::Usage(Usage::default()).kind(), "usage");
        assert_eq!(
            Body::SessionStatus {
                state: ActivityState::WaitingPermission
            }
            .kind(),
            "session.status"
        );
        let json = serde_json::to_string(&Body::SessionStatus {
            state: ActivityState::WaitingPermission,
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"kind":"session.status","data":{"state":"waiting_permission"}}"#
        );
    }
}
