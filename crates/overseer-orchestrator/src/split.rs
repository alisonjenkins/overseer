//! Task splitting: builds prompts, parses subtasks, and validates splitting plans.

use std::path::{Path, PathBuf};

use overseer_backends::Backend;
use overseer_core::{Permissions, Profile, RunRequest};

/// A single subtask produced by splitting a larger task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitSubtask {
    pub title: String,
    pub body: String,
    pub file: String,
}

/// The collection of subtasks produced by splitting a larger task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitPlan {
    pub subtasks: Vec<SplitSubtask>,
}

/// Errors that can occur when generating or validating a task split.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SplitError {
    #[error("no JSON array found in response")]
    NoJson,
    #[error("invalid JSON in split response: {detail}")]
    InvalidJson { detail: String },
    #[error("split produced too few subtasks: got {count}, expected 2 to 5")]
    TooFew { count: usize },
    #[error("split produced too many subtasks: got {count}, expected 2 to 5")]
    TooMany { count: usize },
    #[error("invalid file path {file:?}: {reason}")]
    BadFile { file: String, reason: String },
    #[error("backend failed during split: {detail}")]
    Backend { detail: String },
}

/// Builds a prompt asking the model to split a task into 2 to 5 subtasks.
pub fn build_split_prompt(task_title: &str, task_body: &str) -> String {
    format!(
        r#"Split the following task into 2 to 5 smaller subtasks.
Each subtask must be a small step approximately the size of a single file edit.
The subtasks must be ordered sequentially in the exact order they must run.

Output ONLY a raw JSON array containing 2 to 5 objects, with no preamble or prose.
Each object must have exactly these keys:
- "title": a concise summary of the subtask
- "body": detailed instructions for what to change in that step
- "file": one repository-relative file path targeted by this step (no leading slash, no '..')

Task Title:
{task_title}

Task Description:
{task_body}
"#
    )
}

fn validate_file_path(file: &str) -> Result<(), SplitError> {
    let bad = |reason: &str| {
        Err(SplitError::BadFile {
            file: file.to_owned(),
            reason: reason.to_owned(),
        })
    };
    if file.is_empty() {
        return bad("path cannot be empty");
    }
    if file.starts_with('/') {
        return bad("path must not start with '/'");
    }
    if file.contains('\\') || file.contains('\0') {
        return bad("path cannot contain backslash or NUL byte");
    }
    if Path::new(file)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return bad("path cannot contain '..' as a path component");
    }
    Ok(())
}

fn get_string_field<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
    idx: usize,
) -> Result<&'a str, SplitError> {
    let val = obj.get(key).ok_or_else(|| SplitError::InvalidJson {
        detail: format!("missing field '{key}' at index {idx}"),
    })?;
    val.as_str().ok_or_else(|| SplitError::InvalidJson {
        detail: format!("field '{key}' at index {idx} must be a string"),
    })
}

/// Parses and validates a JSON split response from model output.
pub fn parse_split(answer: &str) -> Result<SplitPlan, SplitError> {
    let start = answer.find('[').ok_or(SplitError::NoJson)?;
    let end = answer.rfind(']').ok_or(SplitError::NoJson)?;
    if end < start {
        return Err(SplitError::NoJson);
    }

    let json_str = answer.get(start..=end).ok_or(SplitError::NoJson)?;
    let parsed: serde_json::Value =
        serde_json::from_str(json_str).map_err(|err| SplitError::InvalidJson {
            detail: err.to_string(),
        })?;

    let items = parsed.as_array().ok_or_else(|| SplitError::InvalidJson {
        detail: "expected top-level JSON array".to_owned(),
    })?;

    let count = items.len();
    if count < 2 {
        return Err(SplitError::TooFew { count });
    }
    if count > 5 {
        return Err(SplitError::TooMany { count });
    }

    let mut subtasks = Vec::with_capacity(count);
    for (idx, item) in items.iter().enumerate() {
        let obj = item.as_object().ok_or_else(|| SplitError::InvalidJson {
            detail: format!("item at index {idx} must be an object"),
        })?;

        let title = get_string_field(obj, "title", idx)?;
        if title.trim().is_empty() {
            return Err(SplitError::InvalidJson {
                detail: format!("field 'title' at index {idx} must not be empty"),
            });
        }

        let body = get_string_field(obj, "body", idx)?;
        if body.trim().is_empty() {
            return Err(SplitError::InvalidJson {
                detail: format!("field 'body' at index {idx} must not be empty"),
            });
        }

        let file = get_string_field(obj, "file", idx)?;
        validate_file_path(file)?;

        subtasks.push(SplitSubtask {
            title: title.to_owned(),
            body: body.to_owned(),
            file: file.to_owned(),
        });
    }

    Ok(SplitPlan { subtasks })
}

/// Requests a task split from a model backend using read-only permissions.
pub async fn request_split(
    backend: &dyn Backend,
    profile: &Profile,
    task_title: &str,
    task_body: &str,
) -> Result<SplitPlan, SplitError> {
    let prompt = build_split_prompt(task_title, task_body);
    let request = RunRequest {
        system: profile.role_prompt.clone(),
        prompt,
        workdir: PathBuf::from("."),
        permissions: Permissions::Read,
    };

    let outcome = backend
        .run(&request)
        .await
        .map_err(|err| SplitError::Backend {
            detail: err.to_string(),
        })?;

    if let Some(failure) = outcome.failure {
        return Err(SplitError::Backend {
            detail: failure.to_string(),
        });
    }

    parse_split(&outcome.output)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]

    use async_trait::async_trait;
    use overseer_backends::BackendError;
    use overseer_core::{BackendKind, RunOutcome, Usage};

    use super::*;

    fn test_profile() -> Profile {
        Profile {
            name: "split".into(),
            backend: BackendKind::Openai,
            model: Some("local".into()),
            base_url: None,
            api_key_env: None,
            command: None,
            prompt_flag: None,
            prompt_stdin: false,
            role_prompt: Some("You are careful.".into()),
            fallback: Vec::new(),
            permissions: Permissions::Read,
            timeout_secs: 10,
            metered: false,
            permission_args: Default::default(),
            env: Default::default(),
            agent_tools: false,
            max_steps: 40,
            skills: Vec::new(),
            gpu: false,
            context_chars: 48000,
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    struct FakeBackend {
        output: String,
        failure: Option<overseer_core::FailureKind>,
    }

    #[async_trait]
    impl Backend for FakeBackend {
        async fn run(&self, req: &RunRequest) -> Result<RunOutcome, BackendError> {
            assert_eq!(req.permissions, Permissions::Read);
            assert!(req.prompt.contains("Main"));
            assert!(req.prompt.contains("Body"));
            Ok(RunOutcome {
                output: self.output.clone(),
                usage: Usage::default(),
                failure: self.failure.clone(),
            })
        }
    }

    #[test]
    fn parses_valid_split_in_order() {
        let json = r#"[
            {"title": "Step 1", "body": "Do first thing", "file": "src/first.rs"},
            {"title": "Step 2", "body": "Do second thing", "file": "src/second.rs"},
            {"title": "Step 3", "body": "Do third thing", "file": "src/third.rs"}
        ]"#;
        let plan = parse_split(json).expect("valid split should parse");
        assert_eq!(plan.subtasks.len(), 3);
        assert_eq!(
            plan.subtasks[0],
            SplitSubtask {
                title: "Step 1".into(),
                body: "Do first thing".into(),
                file: "src/first.rs".into()
            }
        );
        assert_eq!(
            plan.subtasks[1],
            SplitSubtask {
                title: "Step 2".into(),
                body: "Do second thing".into(),
                file: "src/second.rs".into()
            }
        );
        assert_eq!(
            plan.subtasks[2],
            SplitSubtask {
                title: "Step 3".into(),
                body: "Do third thing".into(),
                file: "src/third.rs".into()
            }
        );
    }

    #[test]
    fn accepts_fenced_split() {
        let response = "```json\n[{\"title\":\"1\",\"body\":\"b1\",\"file\":\"src/1.rs\"},{\"title\":\"2\",\"body\":\"b2\",\"file\":\"src/2.rs\"}]\n```";
        let plan = parse_split(response).expect("fenced split should parse");
        assert_eq!(plan.subtasks.len(), 2);
        assert_eq!(plan.subtasks[0].title, "1");
        assert_eq!(plan.subtasks[1].title, "2");
    }

    #[test]
    fn rejects_missing_array() {
        assert_eq!(parse_split("no json here"), Err(SplitError::NoJson));
        assert_eq!(
            parse_split("[ {\"title\":\"1\",\"body\":\"b\",\"file\":\"f.rs\"}"),
            Err(SplitError::NoJson)
        );
        assert_eq!(parse_split("] ["), Err(SplitError::NoJson));
    }

    #[test]
    fn rejects_too_few_and_too_many() {
        assert_eq!(
            parse_split(r#"[{"title":"1","body":"b","file":"1.rs"}]"#),
            Err(SplitError::TooFew { count: 1 })
        );
        assert_eq!(parse_split("[]"), Err(SplitError::TooFew { count: 0 }));
        let six = r#"[{"title":"1","body":"b","file":"1.rs"},{"title":"2","body":"b","file":"2.rs"},{"title":"3","body":"b","file":"3.rs"},{"title":"4","body":"b","file":"4.rs"},{"title":"5","body":"b","file":"5.rs"},{"title":"6","body":"b","file":"6.rs"}]"#;
        assert_eq!(parse_split(six), Err(SplitError::TooMany { count: 6 }));
    }

    #[test]
    fn rejects_unsafe_file_paths() {
        assert!(
            matches!(parse_split(r#"[{"title":"1","body":"b","file":"/etc/passwd"},{"title":"2","body":"b","file":"s.rs"}]"#), Err(SplitError::BadFile { file, .. }) if file == "/etc/passwd")
        );
        assert!(
            matches!(parse_split(r#"[{"title":"1","body":"b","file":"../out.rs"},{"title":"2","body":"b","file":"s.rs"}]"#), Err(SplitError::BadFile { file, .. }) if file == "../out.rs")
        );
        assert!(
            matches!(parse_split(r#"[{"title":"1","body":"b","file":"a/../b.rs"},{"title":"2","body":"b","file":"s.rs"}]"#), Err(SplitError::BadFile { file, .. }) if file == "a/../b.rs")
        );
        assert!(
            matches!(parse_split(r#"[{"title":"1","body":"b","file":""},{"title":"2","body":"b","file":"s.rs"}]"#), Err(SplitError::BadFile { file, .. }) if file.is_empty())
        );
        assert!(
            matches!(parse_split(r#"[{"title":"1","body":"b","file":"a\\b.rs"},{"title":"2","body":"b","file":"s.rs"}]"#), Err(SplitError::BadFile { file, .. }) if file == "a\\b.rs")
        );
        assert!(
            matches!(parse_split(r#"[{"title":"1","body":"b","file":"a/\u0000b.rs"},{"title":"2","body":"b","file":"s.rs"}]"#), Err(SplitError::BadFile { file, .. }) if file.contains('\0'))
        );
    }

    #[test]
    fn rejects_invalid_json() {
        assert!(matches!(
            parse_split("[{not json}]"),
            Err(SplitError::InvalidJson { .. })
        ));
        assert!(matches!(
            parse_split("[1, 2]"),
            Err(SplitError::InvalidJson { .. })
        ));
        assert!(
            matches!(parse_split(r#"[{"title":"1","file":"s.rs"},{"title":"2","body":"b","file":"s.rs"}]"#), Err(SplitError::InvalidJson { detail }) if detail.contains("body") && detail.contains("0"))
        );
        assert!(
            matches!(parse_split(r#"[{"title":"","body":"b","file":"s.rs"},{"title":"2","body":"b","file":"s.rs"}]"#), Err(SplitError::InvalidJson { detail }) if detail.contains("title") && detail.contains("0"))
        );
        assert!(
            matches!(parse_split(r#"[{"title":"1","body":"","file":"s.rs"},{"title":"2","body":"b","file":"s.rs"}]"#), Err(SplitError::InvalidJson { detail }) if detail.contains("body") && detail.contains("0"))
        );
    }

    #[test]
    fn rejects_whole_split_on_one_bad_item() {
        let array = r#"[{"title":"1","body":"b","file":"1.rs"},{"title":"2","body":"b","file":"/etc/shadow"},{"title":"3","body":"b","file":"3.rs"}]"#;
        assert!(
            matches!(parse_split(array), Err(SplitError::BadFile { file, .. }) if file == "/etc/shadow")
        );
    }

    #[tokio::test]
    async fn request_split_uses_backend_answer() {
        let fake = FakeBackend {
            output: r#"[{"title":"1","body":"b1","file":"src/1.rs"},{"title":"2","body":"b2","file":"src/2.rs"}]"#.into(),
            failure: None,
        };
        let plan = request_split(&fake, &test_profile(), "Main", "Body")
            .await
            .expect("should succeed");
        assert_eq!(plan.subtasks.len(), 2);
        assert_eq!(plan.subtasks[0].title, "1");
        assert_eq!(plan.subtasks[0].body, "b1");
        assert_eq!(plan.subtasks[0].file, "src/1.rs");
        assert_eq!(plan.subtasks[1].title, "2");
        assert_eq!(plan.subtasks[1].body, "b2");
        assert_eq!(plan.subtasks[1].file, "src/2.rs");
    }
}
