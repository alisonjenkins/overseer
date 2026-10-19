//! Task splitting: builds prompts, parses subtasks, and validates splitting plans.

use std::path::{Path, PathBuf};

use overseer_backends::Backend;
use overseer_core::{
    FailureKind, NewTask, Permissions, Profile, RunRequest, Task, TaskId, TaskStatus,
};
use overseer_store::{Store, StoreError};

pub const RUNAWAY_MAX_STEPS: &str = "exceeded max_steps";
pub const RUNAWAY_CONTEXT_BUDGET: &str = "context budget exceeded";
pub const RUNAWAY_STATUS_500: &str = "status 500";

/// Returns true if `kind` matches a runaway failure signal (max_steps exceeded,
/// context budget exceeded, or server status 500).
pub fn is_runaway(kind: &FailureKind) -> bool {
    match kind {
        FailureKind::Other(msg) => {
            msg.contains(RUNAWAY_MAX_STEPS)
                || msg.contains(RUNAWAY_CONTEXT_BUDGET)
                || msg.contains(RUNAWAY_STATUS_500)
        }
        _ => false,
    }
}

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

/// Errors that can occur when applying a task split or completing split parents.
#[derive(Debug, thiserror::Error)]
pub enum SplitApplyError {
    #[error("split refused: {reason}")]
    Refused { reason: String },

    #[error("store operation failed: {source}")]
    Store {
        #[source]
        source: Box<StoreError>,
    },
}

impl From<StoreError> for SplitApplyError {
    fn from(source: StoreError) -> Self {
        Self::Store {
            source: Box::new(source),
        }
    }
}

/// Creates child tasks from a validated split plan, sequences their dependencies,
/// links them to `parent`, and sets `parent` to Blocked.
pub fn apply_split(
    store: &mut Store,
    parent: TaskId,
    plan: &SplitPlan,
) -> Result<Vec<TaskId>, SplitApplyError> {
    let parent_task = store.get_task(parent)?;

    match parent_task.status {
        TaskStatus::Backlog
        | TaskStatus::Ready
        | TaskStatus::Running
        | TaskStatus::Blocked
        | TaskStatus::Review => {}
        other => {
            return Err(SplitApplyError::Refused {
                reason: format!("task {parent} in status {other} cannot be split"),
            });
        }
    }

    let existing_children = store.list_child_tasks(parent)?;
    if !existing_children.is_empty() {
        return Err(SplitApplyError::Refused {
            reason: format!(
                "task {parent} already has {} child tasks",
                existing_children.len()
            ),
        });
    }

    if plan.subtasks.is_empty() {
        return Err(SplitApplyError::Refused {
            reason: "split plan contains no subtasks".to_string(),
        });
    }

    let total = plan.subtasks.len();
    let mut created_ids = Vec::with_capacity(total);
    let mut prev_id: Option<TaskId> = None;

    let cancel_created = |store: &mut Store, ids: &[TaskId]| {
        for &id in ids {
            let _ = store.set_status(id, TaskStatus::Cancelled);
        }
    };

    for (idx, subtask) in plan.subtasks.iter().enumerate() {
        let step = idx.saturating_add(1);
        let extra = format!(
            "Split from task {parent}, step {step} of {total}. Target file: {}.",
            subtask.file
        );
        let body = if subtask.body.ends_with('\n') {
            format!("{}{extra}", subtask.body)
        } else {
            format!("{}\n{extra}", subtask.body)
        };

        let depends_on = prev_id.map(|id| vec![id]).unwrap_or_default();
        let new_task = NewTask {
            title: subtask.title.clone(),
            body,
            priority: parent_task.priority,
            profile: parent_task.profile.clone(),
            depends_on,
            ready: true,
            source: None,
            external_id: None,
            parent_task_id: Some(parent),
        };

        let child = match store.add_task(new_task) {
            Ok(task) => task,
            Err(err) => {
                cancel_created(store, &created_ids);
                return Err(SplitApplyError::from(err));
            }
        };

        if let Err(err) = store.set_task_parent(child.id, parent) {
            created_ids.push(child.id);
            cancel_created(store, &created_ids);
            return Err(SplitApplyError::from(err));
        }

        prev_id = Some(child.id);
        created_ids.push(child.id);
    }

    let ids_str = created_ids
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let reason = format!("split into {total} subtasks: {ids_str}");

    if let Err(err) = store.set_status_with_reason(parent, TaskStatus::Blocked, Some(&reason)) {
        cancel_created(store, &created_ids);
        return Err(SplitApplyError::from(err));
    }

    Ok(created_ids)
}

/// Marks a split parent task Done when all of its child tasks are Done.
pub fn complete_split_parents(store: &mut Store) -> Result<Vec<TaskId>, SplitApplyError> {
    let blocked_tasks = store.list_tasks(Some(TaskStatus::Blocked))?;
    let mut completed = Vec::new();

    for task in blocked_tasks {
        let Some(ref reason) = task.reason else {
            continue;
        };
        if !reason.starts_with("split into ") {
            continue;
        }

        let children = store.list_child_tasks(task.id)?;
        if !children.is_empty() && children.iter().all(|c| c.status == TaskStatus::Done) {
            store.set_status(task.id, TaskStatus::Done)?;
            completed.push(task.id);
        }
    }

    Ok(completed)
}

/// Errors that can occur when executing the task split step.
#[derive(Debug, thiserror::Error)]
pub enum SplitStepError {
    #[error("split refused: {0}")]
    Refused(String),

    #[error("split request failed: {0}")]
    Split(#[from] SplitError),

    #[error("apply split failed: {0}")]
    Apply(#[from] SplitApplyError),
}

/// Splits a task into subtasks using the provided model profile and backend,
/// validating and applying the resulting split plan.
pub async fn split_task(
    store: &mut Store,
    backend: &dyn Backend,
    profile: &Profile,
    task: &Task,
) -> Result<Vec<TaskId>, SplitStepError> {
    if task.parent_task_id.is_some() {
        return Err(SplitStepError::Refused(format!(
            "task {} is a child task and cannot be split",
            task.id
        )));
    }

    let children = store
        .list_child_tasks(task.id)
        .map_err(SplitApplyError::from)?;
    if !children.is_empty() {
        return Err(SplitStepError::Refused(format!(
            "task {} already has child tasks",
            task.id
        )));
    }

    let plan = request_split(backend, profile, &task.title, &task.body).await?;
    let child_ids = apply_split(store, task.id, &plan)?;
    tracing::info!("task {} split into {} subtasks", task.id, child_ids.len());
    Ok(child_ids)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::arithmetic_side_effects
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

    #[test]
    fn apply_split_creates_ordered_children() {
        let mut store = Store::open_in_memory().unwrap();
        let parent = store
            .add_task(NewTask {
                title: "Parent Task".into(),
                body: "Parent Body".into(),
                priority: 3,
                profile: Some("split-profile".into()),
                ..Default::default()
            })
            .unwrap();
        store.set_status(parent.id, TaskStatus::Running).unwrap();

        let plan = SplitPlan {
            subtasks: vec![
                SplitSubtask {
                    title: "Subtask 1".into(),
                    body: "Body 1".into(),
                    file: "crates/a.rs".into(),
                },
                SplitSubtask {
                    title: "Subtask 2".into(),
                    body: "Body 2".into(),
                    file: "crates/b.rs".into(),
                },
                SplitSubtask {
                    title: "Subtask 3".into(),
                    body: "Body 3".into(),
                    file: "crates/c.rs".into(),
                },
            ],
        };

        let child_ids = apply_split(&mut store, parent.id, &plan).unwrap();
        assert_eq!(child_ids.len(), 3);

        let c1 = store.get_task(child_ids[0]).unwrap();
        let c2 = store.get_task(child_ids[1]).unwrap();
        let c3 = store.get_task(child_ids[2]).unwrap();

        assert!(c1.depends_on.is_empty());
        assert_eq!(c2.depends_on, vec![c1.id]);
        assert_eq!(c3.depends_on, vec![c2.id]);

        for (task, (num, file)) in [(&c1, (1, "a")), (&c2, (2, "b")), (&c3, (3, "c"))] {
            assert_eq!(task.title, format!("Subtask {num}"));
            assert_eq!(
                task.body,
                format!(
                    "Body {num}\nSplit from task {}, step {num} of 3. Target file: crates/{file}.rs.",
                    parent.id
                )
            );
            assert_eq!(task.priority, 3);
            assert_eq!(task.profile.as_deref(), Some("split-profile"));
            assert_eq!(task.status, TaskStatus::Ready);
            assert_eq!(task.parent_task_id, Some(parent.id));
        }

        let updated_parent = store.get_task(parent.id).unwrap();
        assert_eq!(updated_parent.status, TaskStatus::Blocked);
        let expected_reason = format!("split into 3 subtasks: {}, {}, {}", c1.id, c2.id, c3.id);
        assert_eq!(
            updated_parent.reason.as_deref(),
            Some(expected_reason.as_str())
        );
    }

    #[test]
    fn apply_split_refuses_done_or_already_split_parent() {
        let mut store = Store::open_in_memory().unwrap();
        let plan = SplitPlan {
            subtasks: vec![
                SplitSubtask {
                    title: "Part 1".into(),
                    body: "B1".into(),
                    file: "file1.rs".into(),
                },
                SplitSubtask {
                    title: "Part 2".into(),
                    body: "B2".into(),
                    file: "file2.rs".into(),
                },
            ],
        };

        // Parent that is done
        let p_done = store.add_task(NewTask::default()).unwrap();
        store.set_status(p_done.id, TaskStatus::Done).unwrap();
        let err_done = apply_split(&mut store, p_done.id, &plan).unwrap_err();
        assert!(matches!(err_done, SplitApplyError::Refused { .. }));

        // Parent that already has children
        let p_with_child = store.add_task(NewTask::default()).unwrap();
        store
            .set_status(p_with_child.id, TaskStatus::Running)
            .unwrap();
        let child = store
            .add_task(NewTask {
                parent_task_id: Some(p_with_child.id),
                ..Default::default()
            })
            .unwrap();
        store.set_task_parent(child.id, p_with_child.id).unwrap();

        let err_split = apply_split(&mut store, p_with_child.id, &plan).unwrap_err();
        assert!(matches!(err_split, SplitApplyError::Refused { .. }));
    }

    #[test]
    fn complete_waits_for_all_children() {
        let mut store = Store::open_in_memory().unwrap();
        let parent = store.add_task(NewTask::default()).unwrap();
        store.set_status(parent.id, TaskStatus::Running).unwrap();

        let plan = SplitPlan {
            subtasks: vec![
                SplitSubtask {
                    title: "Sub 1".into(),
                    body: "B1".into(),
                    file: "f1.rs".into(),
                },
                SplitSubtask {
                    title: "Sub 2".into(),
                    body: "B2".into(),
                    file: "f2.rs".into(),
                },
            ],
        };

        let child_ids = apply_split(&mut store, parent.id, &plan).unwrap();
        assert_eq!(child_ids.len(), 2);
        assert_eq!(
            store.get_task(parent.id).unwrap().status,
            TaskStatus::Blocked
        );

        // Neither child is done -> nothing completed
        let completed = complete_split_parents(&mut store).unwrap();
        assert!(completed.is_empty());
        assert_eq!(
            store.get_task(parent.id).unwrap().status,
            TaskStatus::Blocked
        );

        // Only first child is done -> still nothing completed
        store.set_status(child_ids[0], TaskStatus::Done).unwrap();
        let completed = complete_split_parents(&mut store).unwrap();
        assert!(completed.is_empty());
        assert_eq!(
            store.get_task(parent.id).unwrap().status,
            TaskStatus::Blocked
        );

        // Both children are done -> parent marked done
        store.set_status(child_ids[1], TaskStatus::Done).unwrap();
        let completed = complete_split_parents(&mut store).unwrap();
        assert_eq!(completed, vec![parent.id]);
        assert_eq!(store.get_task(parent.id).unwrap().status, TaskStatus::Done);

        // Subsequent call doesn't touch already completed parent
        let completed_again = complete_split_parents(&mut store).unwrap();
        assert!(completed_again.is_empty());
    }

    #[test]
    fn complete_ignores_non_split_blocks() {
        let mut store = Store::open_in_memory().unwrap();
        let t1 = store.add_task(NewTask::default()).unwrap();
        store
            .set_status_with_reason(t1.id, TaskStatus::Blocked, Some("manual block"))
            .unwrap();

        let t2 = store.add_task(NewTask::default()).unwrap();
        store.set_status(t2.id, TaskStatus::Blocked).unwrap();

        let t3 = store.add_task(NewTask::default()).unwrap();
        store
            .set_status_with_reason(
                t3.id,
                TaskStatus::Blocked,
                Some("split into 2 subtasks: 99, 100"),
            )
            .unwrap();

        let completed = complete_split_parents(&mut store).unwrap();
        assert!(completed.is_empty());
        assert_eq!(store.get_task(t1.id).unwrap().status, TaskStatus::Blocked);
        assert_eq!(store.get_task(t2.id).unwrap().status, TaskStatus::Blocked);
        assert_eq!(store.get_task(t3.id).unwrap().status, TaskStatus::Blocked);
    }

    #[test]
    fn runaway_classification() {
        assert!(is_runaway(&FailureKind::Other("exceeded max_steps".into())));
        assert!(is_runaway(&FailureKind::Other(
            "context budget exceeded after eliding tool output".into()
        )));
        assert!(is_runaway(&FailureKind::Other(
            "status 500: internal error".into()
        )));
        assert!(!is_runaway(&FailureKind::Other(
            "review backend unavailable".into()
        )));
        assert!(!is_runaway(&FailureKind::Unreachable));
    }

    #[tokio::test]
    async fn split_task_refuses_split_children() {
        let mut store = Store::open_in_memory().unwrap();
        let fake = FakeBackend {
            output: "[]".into(),
            failure: None,
        };
        let profile = test_profile();

        let parent = store
            .add_task(NewTask {
                title: "Parent".into(),
                body: "Body".into(),
                ..Default::default()
            })
            .unwrap();
        let child = store
            .add_task(NewTask {
                title: "Child".into(),
                parent_task_id: Some(parent.id),
                ..Default::default()
            })
            .unwrap();
        store.set_task_parent(child.id, parent.id).unwrap();

        let err_parent = split_task(&mut store, &fake, &profile, &parent)
            .await
            .unwrap_err();
        assert!(matches!(err_parent, SplitStepError::Refused(_)));

        let err_child = split_task(&mut store, &fake, &profile, &child)
            .await
            .unwrap_err();
        assert!(matches!(err_child, SplitStepError::Refused(_)));
    }

    #[tokio::test]
    async fn split_task_creates_children() {
        let mut store = Store::open_in_memory().unwrap();
        let parent = store
            .add_task(NewTask {
                title: "Parent Task".into(),
                body: "Parent Body".into(),
                ..Default::default()
            })
            .unwrap();
        store.set_status(parent.id, TaskStatus::Ready).unwrap();

        let fake = FakeBackend {
            output: r#"[
                {"title": "Step 1", "body": "Do first", "file": "src/1.rs"},
                {"title": "Step 2", "body": "Do second", "file": "src/2.rs"},
                {"title": "Step 3", "body": "Do third", "file": "src/3.rs"}
            ]"#
            .into(),
            failure: None,
        };

        let child_ids = split_task(&mut store, &fake, &test_profile(), &parent)
            .await
            .unwrap();
        assert_eq!(child_ids.len(), 3);

        let updated_parent = store.get_task(parent.id).unwrap();
        assert_eq!(updated_parent.status, TaskStatus::Blocked);
        let expected_reason = format!(
            "split into 3 subtasks: {}, {}, {}",
            child_ids[0], child_ids[1], child_ids[2]
        );
        assert_eq!(
            updated_parent.reason.as_deref(),
            Some(expected_reason.as_str())
        );

        let children = store.list_child_tasks(parent.id).unwrap();
        assert_eq!(children.len(), 3);
        assert_eq!(children[0].id, child_ids[0]);
        assert_eq!(children[1].id, child_ids[1]);
        assert_eq!(children[2].id, child_ids[2]);
        assert_eq!(children[1].depends_on, vec![child_ids[0]]);
        assert_eq!(children[2].depends_on, vec![child_ids[1]]);
    }
}
