use std::collections::HashMap;
use std::path::{Path, PathBuf};

use overseer_backends::{Backend, BackendError, build_backend};
use overseer_core::{
    Config, FailureKind, NewReview, Permissions, Profile, RunFinish, RunRequest, RunStatus, Task,
    TaskId, TaskStatus, Verdict, parse_review,
};
use overseer_store::{Store, StoreError};

use crate::router::{RouterError, plan_attempts};
use crate::runner::{Attempt, RunnerError, continue_task_with_backends, write_transcript};
use crate::worktree::{self, WorktreeError};

const MAX_DIFF_BYTES: usize = 60 * 1024;
const TRUNCATION_NOTE: &str = "\n\n[Diff truncated to 60 KB; remaining lines omitted]";
const MAX_FILE_LINES: usize = 400;
const MAX_TOTAL_FILE_BYTES: usize = 60_000;

/// Outcome of aggregating reviewer verdicts for a task round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewOutcome {
    /// All reviewers passed; task remains in `review` and is marked approved.
    Approved,
    /// At least one reviewer asked for changes and round < max_reworks.
    Rework,
    /// A reviewer escalated or max_reworks was reached without approval; moves to `needs-human`.
    NeedsHuman,
}

/// A reviewer's result for one round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewerResult {
    pub profile: String,
    pub verdict: Verdict,
    pub feedback: String,
    pub path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    #[error("task {task_id} names unknown reviewer profile {profile:?}")]
    UnknownReviewerProfile { task_id: TaskId, profile: String },

    #[error("failed to build backend for profile {profile:?}")]
    BuildBackend {
        profile: String,
        #[source]
        source: BackendError,
    },

    #[error("no backend was supplied for profile {profile:?}")]
    MissingBackend { profile: String },

    #[error("task {task_id} has no profile left to attempt for review")]
    Route {
        task_id: TaskId,
        #[source]
        source: RouterError,
    },

    #[error("task {task_id} reviewer {profile:?} failed: all attempts in plan failed")]
    ReviewerFailed { task_id: TaskId, profile: String },

    #[error("failed to get diff for branch {branch:?}")]
    Diff {
        branch: String,
        #[source]
        source: WorktreeError,
    },

    #[error("store step {step} failed")]
    Store {
        step: &'static str,
        #[source]
        source: StoreError,
    },

    #[error("failed to write review file at {path:?}")]
    WriteReview {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed during task rework")]
    Rework {
        #[source]
        source: RunnerError,
    },
}

fn store_step(step: &'static str) -> impl FnOnce(StoreError) -> ReviewError {
    move |source| ReviewError::Store { step, source }
}

/// The overall verdict for a round across all reviewers.
/// Any ESCALATE gives ESCALATE; else any CHANGES gives CHANGES; else PASS.
pub fn overall_verdict(verdicts: &[Verdict]) -> Verdict {
    if verdicts.contains(&Verdict::Escalate) {
        Verdict::Escalate
    } else if verdicts.contains(&Verdict::Changes) {
        Verdict::Changes
    } else {
        Verdict::Pass
    }
}

/// Aggregate reviewer verdicts into a `ReviewOutcome`.
/// - Any ESCALATE -> `NeedsHuman`
/// - Else any CHANGES -> `Rework` if round < max_reworks else `NeedsHuman`
/// - Else (all PASS) -> `Approved`
pub fn aggregate_verdicts(verdicts: &[Verdict], round: u8, max_reworks: u8) -> ReviewOutcome {
    if verdicts.contains(&Verdict::Escalate) {
        ReviewOutcome::NeedsHuman
    } else if verdicts.contains(&Verdict::Changes) {
        if round < max_reworks {
            ReviewOutcome::Rework
        } else {
            ReviewOutcome::NeedsHuman
        }
    } else {
        ReviewOutcome::Approved
    }
}

/// Build the follow-up prompt for rework with the concatenated CHANGES feedback.
pub fn build_rework_prompt(changes_feedback: &str) -> String {
    format!(
        "A reviewer asked for these changes:\n\n{changes_feedback}\n\nFix them on the existing branch; do not start over."
    )
}

/// Truncate diff to 60 KB if longer, appending an explanatory note.
fn truncate_diff(diff: &str) -> (String, bool) {
    if diff.len() <= MAX_DIFF_BYTES {
        return (diff.to_owned(), false);
    }
    let mut boundary = MAX_DIFF_BYTES;
    while boundary > 0 && !diff.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    let truncated = diff.get(..boundary).unwrap_or("");
    let mut out = String::with_capacity(truncated.len().saturating_add(TRUNCATION_NOTE.len()));
    out.push_str(truncated);
    out.push_str(TRUNCATION_NOTE);
    (out, true)
}

fn extract_diff_paths(diff: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in diff.lines() {
        let raw = if let Some(rest) = line.strip_prefix("diff --git ") {
            rest.rfind(" b/")
                .and_then(|i| rest.get(i.saturating_add(3)..))
        } else {
            line.strip_prefix("+++ b/")
        };
        if let Some(mut path) = raw.map(str::trim) {
            if let Some(unquoted) = path.strip_prefix('"').and_then(|p| p.strip_suffix('"')) {
                path = unquoted;
            }
            if !path.is_empty() && path != "/dev/null" && !paths.iter().any(|p| p == path) {
                paths.push(path.to_owned());
            }
        }
    }
    paths
}

fn append_file_contents(prompt: &mut String, diff: &str, worktree: &Path) {
    let paths = extract_diff_paths(diff);
    let mut total_added_bytes = 0usize;

    for path in paths {
        let file_path = worktree.join(&path);
        if !file_path.is_file() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&file_path) else {
            continue;
        };
        if total_added_bytes >= MAX_TOTAL_FILE_BYTES {
            break;
        }

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();
        let mut file_buf = String::new();
        let mut included_lines = 0usize;

        for line in lines.iter().take(MAX_FILE_LINES) {
            let line_bytes = line.len().saturating_add(1);
            if total_added_bytes.saturating_add(line_bytes) > MAX_TOTAL_FILE_BYTES {
                break;
            }
            file_buf.push_str(line);
            file_buf.push('\n');
            total_added_bytes = total_added_bytes.saturating_add(line_bytes);
            included_lines = included_lines.saturating_add(1);
        }

        let omitted = total_lines.saturating_sub(included_lines);
        if omitted > 0 {
            if omitted == 1 {
                file_buf.push_str("[1 line omitted]\n");
            } else {
                file_buf.push_str(&format!("[{omitted} lines omitted]\n"));
            }
        }

        prompt.push_str(&format!("### File: {path}\n```\n{file_buf}```\n\n"));
    }
}

/// Build the reviewer prompt containing the task title, body, the unified diff,
/// and full contents of changed files existing in the worktree.
/// Truncates the diff to 60 KB with an explanatory note if longer.
/// Explicitly instructs the model:
/// - Reply with the verdict line first (`VERDICT: PASS`, `VERDICT: CHANGES`, or `VERDICT: ESCALATE`)
/// - Follow with a short bullet list of problems (CHANGES) or reasons (ESCALATE)
/// - Do NOT use tools or run commands
/// - Do NOT edit files
pub fn build_reviewer_prompt(title: &str, body: &str, diff: &str, worktree: &Path) -> String {
    let (diff_content, _) = truncate_diff(diff);
    let mut prompt = String::new();
    prompt.push_str("Task Title: ");
    prompt.push_str(title);
    prompt.push_str("\n\nTask Description:\n");
    if body.trim().is_empty() {
        prompt.push_str("(No description provided)\n");
    } else {
        prompt.push_str(body);
        prompt.push('\n');
    }
    prompt.push_str("\nDiff against base branch:\n```diff\n");
    prompt.push_str(&diff_content);
    prompt.push_str("\n```\n\n");
    append_file_contents(&mut prompt, diff, worktree);
    prompt.push_str(
        "Instructions for review:\n\
         1. You are reviewing the changes above for the task described.\n\
         2. Reply with the verdict as the FIRST non-empty line of your response:\n\
            - `VERDICT: PASS` if the changes correctly address the task without issues.\n\
            - `VERDICT: CHANGES` if modifications are required before this can be accepted.\n\
            - `VERDICT: ESCALATE` if human intervention is required.\n\
         3. If your verdict is CHANGES, follow it with a short bullet list of concrete problems.\n\
         4. If your verdict is ESCALATE, follow it with a short bullet list of reasons.\n\
         5. Do NOT use any tools or run commands because all necessary context and diff are provided above.\n\
         6. Do NOT edit files.\n",
    );
    prompt
}

/// For the task's current round, run all configured reviewers concurrently,
/// each as its own run row, write each reply to
/// `<state-dir>/reviews/task-<id>/round-<n>/<profile>.md`, and record rows in the
/// store `reviews` table.
pub async fn review_task(
    store: &mut Store,
    config: &Config,
    state_dir: &Path,
    task_id: TaskId,
) -> Result<Vec<ReviewerResult>, ReviewError> {
    let mut built = Vec::new();
    for name in &config.review.profiles {
        let profile = config
            .profile(name)
            .ok_or_else(|| ReviewError::UnknownReviewerProfile {
                task_id,
                profile: name.clone(),
            })?;
        let mut to_build = vec![profile];
        for fb_name in &profile.fallback {
            if let Some(fb) = config.profile(fb_name) {
                to_build.push(fb);
            }
        }
        for p in to_build {
            if !built.iter().any(|(n, _): &(&str, _)| *n == p.name.as_str()) {
                let backend = build_backend(p).map_err(|source| ReviewError::BuildBackend {
                    profile: p.name.clone(),
                    source,
                })?;
                built.push((p.name.as_str(), backend));
            }
        }
    }
    let backends: HashMap<&str, &dyn Backend> = built
        .iter()
        .map(|(name, backend)| (*name, backend.as_ref()))
        .collect();
    review_task_with_backends(store, config, state_dir, task_id, &backends).await
}

/// Like `review_task`, but with backends supplied by caller.
pub async fn review_task_with_backends(
    store: &mut Store,
    config: &Config,
    state_dir: &Path,
    task_id: TaskId,
    backends: &HashMap<&str, &dyn Backend>,
) -> Result<Vec<ReviewerResult>, ReviewError> {
    if config.review.profiles.is_empty() {
        return Ok(Vec::new());
    }

    let task = store.get_task(task_id).map_err(store_step("load task"))?;
    let branch = worktree::branch_name(task_id);
    let diff = worktree::diff(&config.repo, &branch)
        .await
        .map_err(|source| ReviewError::Diff {
            branch: branch.clone(),
            source,
        })?;

    let worktree_dir = worktree::worktree_path(state_dir, task_id);
    let prompt = build_reviewer_prompt(&task.title, &task.body, &diff, &worktree_dir);
    let round_dir = state_dir
        .join("reviews")
        .join(format!("task-{}", task_id))
        .join(format!("round-{}", task.round));
    tokio::fs::create_dir_all(&round_dir)
        .await
        .map_err(|source| ReviewError::WriteReview {
            path: round_dir.clone(),
            source,
        })?;

    let store_mutex = tokio::sync::Mutex::new(store);

    let mut futs = Vec::new();
    for profile_name in &config.review.profiles {
        futs.push(run_single_reviewer(
            &store_mutex,
            config,
            state_dir,
            &worktree_dir,
            &round_dir,
            task_id,
            task.round,
            profile_name,
            &prompt,
            &branch,
            &task,
            backends,
        ));
    }

    let results = futures_util::future::join_all(futs)
        .await
        .into_iter()
        .collect::<Result<Vec<_>, ReviewError>>()?;

    Ok(results)
}

#[expect(clippy::too_many_arguments)]
async fn run_single_reviewer(
    store_mutex: &tokio::sync::Mutex<&mut Store>,
    config: &Config,
    state_dir: &Path,
    worktree_dir: &Path,
    round_dir: &Path,
    task_id: TaskId,
    round: u8,
    profile_name: &str,
    prompt: &str,
    branch: &str,
    task: &Task,
    backends: &HashMap<&str, &dyn Backend>,
) -> Result<ReviewerResult, ReviewError> {
    let profile =
        config
            .profile(profile_name)
            .ok_or_else(|| ReviewError::UnknownReviewerProfile {
                task_id,
                profile: profile_name.to_owned(),
            })?;

    let attempt1 = run_reviewer_attempt(
        store_mutex,
        config,
        state_dir,
        worktree_dir,
        task_id,
        profile,
        prompt,
        branch,
        task,
        backends,
    )
    .await;

    let (verdict, feedback, reply_text) = match attempt1 {
        Ok(out) => {
            if let Ok(parsed) = parse_review(&out) {
                (parsed.verdict, parsed.feedback, out)
            } else {
                retry_reviewer(
                    store_mutex,
                    config,
                    state_dir,
                    worktree_dir,
                    task_id,
                    profile,
                    prompt,
                    branch,
                    task,
                    backends,
                )
                .await
            }
        }
        Err(_) => {
            retry_reviewer(
                store_mutex,
                config,
                state_dir,
                worktree_dir,
                task_id,
                profile,
                prompt,
                branch,
                task,
                backends,
            )
            .await
        }
    };

    let reply_path = round_dir.join(format!("{profile_name}.md"));
    let tmp_path = round_dir.join(format!("{profile_name}.md.tmp"));
    tokio::fs::write(&tmp_path, &reply_text)
        .await
        .map_err(|source| ReviewError::WriteReview {
            path: tmp_path.clone(),
            source,
        })?;
    tokio::fs::rename(&tmp_path, &reply_path)
        .await
        .map_err(|source| ReviewError::WriteReview {
            path: reply_path.clone(),
            source,
        })?;

    let new_review = NewReview {
        task_id,
        round,
        profile: profile_name.to_owned(),
        verdict,
        path: reply_path.clone(),
    };
    store_mutex
        .lock()
        .await
        .record_review(new_review)
        .map_err(store_step("record review"))?;

    Ok(ReviewerResult {
        profile: profile_name.to_owned(),
        verdict,
        feedback,
        path: reply_path,
    })
}

#[expect(clippy::too_many_arguments)]
async fn retry_reviewer(
    store_mutex: &tokio::sync::Mutex<&mut Store>,
    config: &Config,
    state_dir: &Path,
    worktree_dir: &Path,
    task_id: TaskId,
    profile: &Profile,
    prompt: &str,
    branch: &str,
    task: &Task,
    backends: &HashMap<&str, &dyn Backend>,
) -> (Verdict, String, String) {
    let attempt2 = run_reviewer_attempt(
        store_mutex,
        config,
        state_dir,
        worktree_dir,
        task_id,
        profile,
        prompt,
        branch,
        task,
        backends,
    )
    .await;

    match attempt2 {
        Ok(out) => {
            if let Ok(parsed) = parse_review(&out) {
                (parsed.verdict, parsed.feedback, out)
            } else {
                (
                    Verdict::Escalate,
                    "Unparseable review verdict after retry; counted as ESCALATE.".to_owned(),
                    format!(
                        "{out}\n\n[Note: Unparseable review verdict after retry; counted as ESCALATE]"
                    ),
                )
            }
        }
        Err(_) => (
            Verdict::Escalate,
            "Reviewer failed to run after retry; counted as ESCALATE.".to_owned(),
            "VERDICT: ESCALATE\n\n[Note: Reviewer failed to run after retry; counted as ESCALATE]"
                .to_owned(),
        ),
    }
}

#[expect(clippy::too_many_arguments)]
async fn run_reviewer_attempt(
    store_mutex: &tokio::sync::Mutex<&mut Store>,
    config: &Config,
    state_dir: &Path,
    worktree_dir: &Path,
    task_id: TaskId,
    start_profile: &Profile,
    prompt: &str,
    branch: &str,
    task: &Task,
    backends: &HashMap<&str, &dyn Backend>,
) -> Result<String, ReviewError> {
    let credits_low = {
        let store = store_mutex.lock().await;
        store
            .credits_low()
            .map_err(store_step("read credits_low"))?
    };
    let plan = plan_attempts(config, start_profile, credits_low)
        .map_err(|source| ReviewError::Route { task_id, source })?;

    let mut credits_exhausted = false;
    for profile in plan {
        if credits_exhausted && profile.metered {
            tracing::info!(
                task_id = task_id.get(),
                profile = %profile.name,
                "skipping metered reviewer profile after credits ran out"
            );
            continue;
        }

        let backend = backends
            .get(profile.name.as_str())
            .copied()
            .ok_or_else(|| ReviewError::MissingBackend {
                profile: profile.name.clone(),
            })?;

        let run_id = {
            let mut store = store_mutex.lock().await;
            store
                .start_run(task_id, &profile.name, None)
                .map_err(store_step("start run"))?
        };

        let request = RunRequest {
            system: profile.role_prompt.clone(),
            prompt: prompt.to_owned(),
            workdir: worktree_dir.to_path_buf(),
            permissions: Permissions::Read,
        };

        let outcome = backend.run(&request).await;
        let mut attempt = match outcome {
            Ok(res) => Attempt {
                output: res.output,
                usage: res.usage,
                failure: res.failure,
                commit: None,
            },
            Err(err) => Attempt::failed(FailureKind::Other(format!("backend error: {err}"))),
        };

        let transcript_path = match write_transcript(
            state_dir,
            run_id,
            task,
            profile,
            branch,
            prompt,
            &attempt,
            profile.role_prompt.as_deref(),
        )
        .await
        {
            Ok(p) => Some(p),
            Err(err) => {
                attempt
                    .failure
                    .get_or_insert(FailureKind::Other(format!("transcript: {err}")));
                None
            }
        };

        let status = if attempt.failure.is_none() {
            RunStatus::Succeeded
        } else {
            RunStatus::Failed
        };

        {
            let mut store = store_mutex.lock().await;
            store
                .finish_run(
                    run_id,
                    RunFinish {
                        status,
                        usage: attempt.usage,
                        failure: attempt.failure.as_ref().map(ToString::to_string),
                        transcript_path,
                        branch: Some(branch.to_owned()),
                    },
                )
                .map_err(store_step("finish run"))?;
        }

        if attempt.failure.is_some()
            && profile.metered
            && attempt.failure == Some(FailureKind::QuotaExhausted)
        {
            let mut store = store_mutex.lock().await;
            store
                .set_credits_low(true)
                .map_err(store_step("set credits_low"))?;
            credits_exhausted = true;
        }

        let failure = attempt.failure.clone();
        if failure.is_none() {
            return Ok(attempt.output);
        }

        match failure {
            Some(kind) if !kind.is_routable() => break,
            _ => {}
        }
    }

    Err(ReviewError::ReviewerFailed {
        task_id,
        profile: start_profile.name.clone(),
    })
}

/// Every profile the review loop may need a backend for: the reviewers, the
/// rework profile, the task's own profile (the default rework profile) and the
/// default profile.
fn loop_profile_names(config: &Config, task_profile: Option<&str>) -> Vec<String> {
    let mut names = config.review.profiles.clone();
    let extra = config
        .review
        .rework_profile
        .as_deref()
        .into_iter()
        .chain(task_profile)
        .chain(std::iter::once(config.default_profile.as_str()));
    for name in extra {
        if !names.iter().any(|n| n == name) {
            names.push(name.to_owned());
        }
    }
    names
}

/// Run the review loop on `task_id` until approved, needs-human, or rework fails.
pub async fn run_review_loop(
    store: &mut Store,
    config: &Config,
    state_dir: &Path,
    task_id: TaskId,
) -> Result<TaskStatus, ReviewError> {
    if config.review.profiles.is_empty() {
        return Ok(TaskStatus::Review);
    }
    let task = store.get_task(task_id).map_err(store_step("load task"))?;
    let profile_names = loop_profile_names(config, task.profile.as_deref());
    let mut built = Vec::new();
    for name in &profile_names {
        if let Some(profile) = config.profile(name) {
            let mut to_build = vec![profile];
            for fb_name in &profile.fallback {
                if let Some(fb) = config.profile(fb_name) {
                    to_build.push(fb);
                }
            }
            for p in to_build {
                if !built.iter().any(|(n, _): &(&str, _)| *n == p.name.as_str()) {
                    let backend = build_backend(p).map_err(|source| ReviewError::BuildBackend {
                        profile: p.name.clone(),
                        source,
                    })?;
                    built.push((p.name.as_str(), backend));
                }
            }
        }
    }
    let backends: HashMap<&str, &dyn Backend> = built
        .iter()
        .map(|(name, backend)| (*name, backend.as_ref()))
        .collect();
    run_review_loop_with_backends(store, config, state_dir, task_id, &backends).await
}

fn extract_escalate_bullet(feedback: &str) -> Option<String> {
    for line in feedback.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(['-', '*', '+', '•']) {
            let content = trimmed.trim_start_matches(|c: char| {
                c == '-' || c == '*' || c == '+' || c == '•' || c.is_whitespace()
            });
            if !content.is_empty() {
                return Some(trimmed.chars().take(200).collect());
            }
        }
    }
    None
}

/// Like `run_review_loop`, but with backends supplied by caller.
pub async fn run_review_loop_with_backends(
    store: &mut Store,
    config: &Config,
    state_dir: &Path,
    task_id: TaskId,
    backends: &HashMap<&str, &dyn Backend>,
) -> Result<TaskStatus, ReviewError> {
    if config.review.profiles.is_empty() {
        return Ok(TaskStatus::Review);
    }

    loop {
        let task = store.get_task(task_id).map_err(store_step("load task"))?;
        let results =
            review_task_with_backends(store, config, state_dir, task_id, backends).await?;
        let verdicts: Vec<Verdict> = results.iter().map(|r| r.verdict).collect();
        let overall = overall_verdict(&verdicts);
        let reviewer_details = results
            .iter()
            .map(|r| format!("{}={}", r.profile, r.verdict.as_str().to_uppercase()))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "task {} round {}: {} (reviewers: {})",
            task.id,
            task.round,
            overall.as_str().to_uppercase(),
            reviewer_details
        );

        let outcome = aggregate_verdicts(&verdicts, task.round, config.review.max_reworks);
        match outcome {
            ReviewOutcome::Approved => {
                store
                    .set_approved(task_id, true)
                    .map_err(store_step("set approved"))?;
                return Ok(TaskStatus::Review);
            }
            ReviewOutcome::NeedsHuman => {
                let reason = (overall == Verdict::Escalate).then(|| {
                    let first_esc = results.iter().find(|r| r.verdict == Verdict::Escalate);
                    match first_esc.and_then(|r| extract_escalate_bullet(&r.feedback)) {
                        Some(bullet) => format!("escalation: reviewer: {bullet}"),
                        None => "escalation: reviewer: no reason given".to_owned(),
                    }
                });
                store
                    .set_status_with_reason(task_id, TaskStatus::NeedsHuman, reason.as_deref())
                    .map_err(store_step("set needs-human"))?;
                return Ok(TaskStatus::NeedsHuman);
            }
            ReviewOutcome::Rework => {
                let feedback = results
                    .iter()
                    .filter(|r| r.verdict == Verdict::Changes && !r.feedback.trim().is_empty())
                    .map(|r| r.feedback.trim())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let rework_prompt = build_rework_prompt(&feedback);
                let rework_profile = config.review.rework_profile.as_deref();
                let summary = continue_task_with_backends(
                    store,
                    config,
                    state_dir,
                    task_id,
                    rework_profile,
                    &rework_prompt,
                    backends,
                )
                .await
                .map_err(|source| ReviewError::Rework { source })?;

                if summary.status != TaskStatus::Review {
                    if summary.status == TaskStatus::NeedsHuman
                        && let Some(ref reason) = summary.reason
                        && let Some(esc) = reason.strip_prefix("escalation: ")
                    {
                        let paths: Vec<&str> = esc.split(", ").take(3).collect();
                        println!(
                            "task {} {}: needs-human (escalation: {})",
                            task.id,
                            task.title,
                            paths.join(", ")
                        );
                    }
                    return Ok(summary.status);
                }

                store
                    .increment_round(task_id)
                    .map_err(store_step("increment round"))?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]

    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use overseer_core::{BackendKind, NewTask, RunOutcome, Usage};
    use tempfile::TempDir;

    use super::*;
    use crate::runner::run_task_with_backends;
    use crate::test_support::init_repo;

    struct Fixture {
        _root: TempDir,
        repo: PathBuf,
        state: PathBuf,
        store: Store,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let state = root.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let store = Store::open(&state.join("overseer.db")).unwrap();
        Fixture {
            _root: root,
            repo,
            state,
            store,
        }
    }

    fn test_profile(name: &str) -> Profile {
        Profile {
            name: name.to_owned(),
            backend: BackendKind::Openai,
            model: Some("local".to_owned()),
            base_url: Some("http://127.0.0.1:1/v1".to_owned()),
            api_key_env: None,
            command: None,
            prompt_flag: None,
            prompt_stdin: false,
            role_prompt: Some("You are careful.".to_owned()),
            fallback: Vec::new(),
            permissions: Permissions::WriteWorkdir,
            timeout_secs: 10,
            metered: false,
            permission_args: Default::default(),
            env: Default::default(),
            agent_tools: false,
            max_steps: 40,
            skills: Vec::new(),
        }
    }

    /// Backend that writes a file into the worktree on each invocation.
    struct WorkerBackend {
        file: &'static str,
        counter: AtomicUsize,
    }

    impl WorkerBackend {
        fn new(file: &'static str) -> Self {
            Self {
                file,
                counter: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Backend for WorkerBackend {
        async fn run(&self, req: &RunRequest) -> Result<RunOutcome, BackendError> {
            let n = self.counter.fetch_add(1, Ordering::SeqCst);
            std::fs::write(req.workdir.join(self.file), format!("content round {n}\n")).unwrap();
            Ok(RunOutcome {
                output: format!("wrote round {n}"),
                usage: Usage::default(),
                failure: None,
            })
        }
    }

    /// Backend that returns pre-configured review responses sequentially.
    struct SequentialReviewBackend {
        responses: std::sync::Mutex<Vec<String>>,
    }

    impl SequentialReviewBackend {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
            }
        }
    }

    #[async_trait]
    impl Backend for SequentialReviewBackend {
        async fn run(&self, _req: &RunRequest) -> Result<RunOutcome, BackendError> {
            let mut list = self.responses.lock().unwrap();
            let output = if list.is_empty() {
                "VERDICT: PASS\n\nDefault pass".to_owned()
            } else {
                list.remove(0)
            };
            Ok(RunOutcome {
                output,
                usage: Usage::default(),
                failure: None,
            })
        }
    }

    #[test]
    fn build_reviewer_prompt_contains_all_required_elements() {
        let title = "Fix parser null pointer";
        let body = "Ensure tokenizer returns empty token stream on empty input.";
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\n+fn new() {}\n";

        let prompt = build_reviewer_prompt(title, body, diff, Path::new(""));

        assert!(prompt.contains("Task Title: Fix parser null pointer"));
        assert!(prompt.contains("Ensure tokenizer returns empty token stream on empty input."));
        assert!(prompt.contains("diff --git a/src/lib.rs b/src/lib.rs"));
        assert!(prompt.contains("VERDICT: PASS"));
        assert!(prompt.contains("VERDICT: CHANGES"));
        assert!(prompt.contains("VERDICT: ESCALATE"));
        assert!(prompt.contains("Do NOT use any tools or run commands"));
        assert!(prompt.contains("Do NOT edit files"));
        assert!(!prompt.contains("Diff truncated"));
    }

    #[test]
    fn build_reviewer_prompt_truncates_large_diff() {
        let title = "Large refactor";
        let body = "Lots of lines changed.";
        let large_diff = "a".repeat(65_000);

        let prompt = build_reviewer_prompt(title, body, &large_diff, Path::new(""));

        assert!(prompt.contains("Diff truncated to 60 KB"));
        let (truncated, is_truncated) = truncate_diff(&large_diff);
        assert!(is_truncated);
        assert!(truncated.contains("Diff truncated to 60 KB"));
        assert!(truncated.len() <= MAX_DIFF_BYTES + TRUNCATION_NOTE.len());
    }

    #[test]
    fn build_reviewer_prompt_includes_file_contents_for_small_file() {
        let root = tempfile::tempdir().unwrap();
        let src_dir = root.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let file_path = src_dir.join("lib.rs");
        std::fs::write(
            &file_path,
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        )
        .unwrap();

        let diff = "diff --git a/src/lib.rs b/src/lib.rs\n+    a + b\n";
        let prompt =
            build_reviewer_prompt("Add function", "Add an add function", diff, root.path());

        assert!(prompt.contains("### File: src/lib.rs"));
        assert!(prompt.contains("pub fn add(a: i32, b: i32) -> i32 {"));
        assert!(prompt.contains("a + b"));
        assert!(!prompt.contains("omitted"));
    }

    #[test]
    fn build_reviewer_prompt_truncates_file_over_400_lines() {
        let root = tempfile::tempdir().unwrap();
        let file_path = root.path().join("large.rs");
        let mut content = String::new();
        for i in 0..450 {
            content.push_str(&format!("// line {i}\n"));
        }
        std::fs::write(&file_path, &content).unwrap();

        let diff = "diff --git a/large.rs b/large.rs\n+// line 42\n";
        let prompt = build_reviewer_prompt("Big file", "Lots of lines", diff, root.path());

        assert!(prompt.contains("### File: large.rs"));
        assert!(prompt.contains("// line 0"));
        assert!(prompt.contains("// line 399"));
        assert!(!prompt.contains("// line 400"));
        assert!(prompt.contains("[50 lines omitted]"));
    }

    #[test]
    fn truncate_diff_handles_multibyte_utf8() {
        let euro = "€".repeat(25_000);
        let (truncated, is_truncated) = truncate_diff(&euro);
        assert!(is_truncated);
        assert!(truncated.contains("Diff truncated to 60 KB"));
    }

    #[test]
    fn build_rework_prompt_formats_concatenated_feedback() {
        let prompt = build_rework_prompt("- Line 10: missing check\n- Line 25: wrong name");
        assert_eq!(
            prompt,
            "A reviewer asked for these changes:\n\n- Line 10: missing check\n- Line 25: wrong name\n\nFix them on the existing branch; do not start over."
        );
    }

    #[test]
    fn overall_verdict_rules() {
        assert_eq!(overall_verdict(&[]), Verdict::Pass);
        assert_eq!(overall_verdict(&[Verdict::Pass]), Verdict::Pass);
        assert_eq!(
            overall_verdict(&[Verdict::Pass, Verdict::Pass]),
            Verdict::Pass
        );
        assert_eq!(
            overall_verdict(&[Verdict::Pass, Verdict::Changes]),
            Verdict::Changes
        );
        assert_eq!(
            overall_verdict(&[Verdict::Changes, Verdict::Pass]),
            Verdict::Changes
        );
        assert_eq!(
            overall_verdict(&[Verdict::Pass, Verdict::Escalate]),
            Verdict::Escalate
        );
        assert_eq!(
            overall_verdict(&[Verdict::Changes, Verdict::Escalate]),
            Verdict::Escalate
        );
    }

    #[test]
    fn aggregate_verdicts_approved_when_all_pass() {
        assert_eq!(
            aggregate_verdicts(&[Verdict::Pass], 0, 2),
            ReviewOutcome::Approved
        );
        assert_eq!(
            aggregate_verdicts(&[Verdict::Pass, Verdict::Pass], 1, 2),
            ReviewOutcome::Approved
        );
        assert_eq!(aggregate_verdicts(&[], 0, 2), ReviewOutcome::Approved);
    }

    #[test]
    fn aggregate_verdicts_rework_when_changes_and_under_limit() {
        assert_eq!(
            aggregate_verdicts(&[Verdict::Changes], 0, 2),
            ReviewOutcome::Rework
        );
        assert_eq!(
            aggregate_verdicts(&[Verdict::Pass, Verdict::Changes], 0, 2),
            ReviewOutcome::Rework
        );
        assert_eq!(
            aggregate_verdicts(&[Verdict::Changes], 1, 2),
            ReviewOutcome::Rework
        );
    }

    #[test]
    fn aggregate_verdicts_needs_human_when_changes_and_limit_reached() {
        assert_eq!(
            aggregate_verdicts(&[Verdict::Changes], 2, 2),
            ReviewOutcome::NeedsHuman
        );
        assert_eq!(
            aggregate_verdicts(&[Verdict::Pass, Verdict::Changes], 2, 2),
            ReviewOutcome::NeedsHuman
        );
        assert_eq!(
            aggregate_verdicts(&[Verdict::Changes], 3, 2),
            ReviewOutcome::NeedsHuman
        );
    }

    #[test]
    fn aggregate_verdicts_needs_human_when_any_escalate() {
        assert_eq!(
            aggregate_verdicts(&[Verdict::Escalate], 0, 2),
            ReviewOutcome::NeedsHuman
        );
        assert_eq!(
            aggregate_verdicts(&[Verdict::Pass, Verdict::Escalate], 0, 2),
            ReviewOutcome::NeedsHuman
        );
        assert_eq!(
            aggregate_verdicts(&[Verdict::Changes, Verdict::Escalate], 0, 2),
            ReviewOutcome::NeedsHuman
        );
    }

    fn add_task(store: &mut Store, title: &str) -> TaskId {
        store
            .add_task(NewTask {
                title: title.to_owned(),
                body: "Body".to_owned(),
                ..NewTask::default()
            })
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn review_task_runs_all_reviewers_and_records_rows() {
        let mut fix = fixture();
        let task_id = add_task(&mut fix.store, "Add feature");

        let worker = WorkerBackend::new("feat.txt");
        let rev1 = SequentialReviewBackend::new(vec!["VERDICT: PASS\n\nClean code".to_owned()]);
        let rev2 = SequentialReviewBackend::new(vec!["VERDICT: PASS\n\nShip it".to_owned()]);

        let mut backends: HashMap<&str, &dyn Backend> = HashMap::new();
        backends.insert("local", &worker);
        backends.insert("rev1", &rev1);
        backends.insert("rev2", &rev2);

        let config = Config {
            repo: fix.repo.clone(),
            default_profile: "local".to_owned(),
            review: overseer_core::ReviewConfig {
                profiles: vec!["rev1".to_owned(), "rev2".to_owned()],
                max_reworks: 2,
                rework_profile: None,
            },
            profiles: vec![
                test_profile("local"),
                test_profile("rev1"),
                test_profile("rev2"),
            ],
            learning: Default::default(),
            escalation: Default::default(),
        };

        // First run the task to create branch & commit
        let summary =
            run_task_with_backends(&mut fix.store, &config, &fix.state, task_id, &backends)
                .await
                .unwrap();
        assert_eq!(summary.status, TaskStatus::Review);

        // Run review_task_with_backends
        let results =
            review_task_with_backends(&mut fix.store, &config, &fix.state, task_id, &backends)
                .await
                .unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].verdict, Verdict::Pass);
        assert_eq!(results[1].verdict, Verdict::Pass);

        // Verify files written to <state-dir>/reviews/task-1/round-0/<profile>.md
        let rev1_file = fix.state.join("reviews/task-1/round-0/rev1.md");
        let rev2_file = fix.state.join("reviews/task-1/round-0/rev2.md");
        assert!(rev1_file.is_file());
        assert!(rev2_file.is_file());
        assert!(
            std::fs::read_to_string(rev1_file)
                .unwrap()
                .contains("Clean code")
        );
        assert!(
            std::fs::read_to_string(rev2_file)
                .unwrap()
                .contains("Ship it")
        );

        // Verify reviews table
        let reviews = fix.store.list_reviews(task_id).unwrap();
        assert_eq!(reviews.len(), 2);
        assert_eq!(reviews[0].round, 0);
        assert_eq!(reviews[0].verdict, Verdict::Pass);
        assert_eq!(reviews[1].round, 0);
        assert_eq!(reviews[1].verdict, Verdict::Pass);

        // Verify runs table has run rows for the reviewers
        let runs = fix.store.list_runs(task_id).unwrap();
        // 1 worker run + 2 reviewer runs = 3 runs total
        assert_eq!(runs.len(), 3);
    }

    #[tokio::test]
    async fn review_task_retries_unparseable_output() {
        let mut fix = fixture();
        let task_id = add_task(&mut fix.store, "Fix bug");

        let worker = WorkerBackend::new("fix.txt");
        // rev1 returns unparseable first, then PASS on retry
        let rev1 = SequentialReviewBackend::new(vec![
            "I looked at your code and it looks ok.".to_owned(),
            "VERDICT: PASS\n\nSecond attempt parsed".to_owned(),
        ]);

        let mut backends: HashMap<&str, &dyn Backend> = HashMap::new();
        backends.insert("local", &worker);
        backends.insert("rev1", &rev1);

        let config = Config {
            repo: fix.repo.clone(),
            default_profile: "local".to_owned(),
            review: overseer_core::ReviewConfig {
                profiles: vec!["rev1".to_owned()],
                max_reworks: 2,
                rework_profile: None,
            },
            profiles: vec![test_profile("local"), test_profile("rev1")],
            learning: Default::default(),
            escalation: Default::default(),
        };

        run_task_with_backends(&mut fix.store, &config, &fix.state, task_id, &backends)
            .await
            .unwrap();

        let results =
            review_task_with_backends(&mut fix.store, &config, &fix.state, task_id, &backends)
                .await
                .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].verdict, Verdict::Pass);
        assert_eq!(results[0].feedback, "Second attempt parsed");
    }

    #[tokio::test]
    async fn review_task_unparseable_twice_escalates() {
        let mut fix = fixture();
        let task_id = add_task(&mut fix.store, "Fix bug");

        let worker = WorkerBackend::new("fix.txt");
        // rev1 returns unparseable twice
        let rev1 = SequentialReviewBackend::new(vec![
            "First unparseable output".to_owned(),
            "Second unparseable output".to_owned(),
        ]);

        let mut backends: HashMap<&str, &dyn Backend> = HashMap::new();
        backends.insert("local", &worker);
        backends.insert("rev1", &rev1);

        let config = Config {
            repo: fix.repo.clone(),
            default_profile: "local".to_owned(),
            review: overseer_core::ReviewConfig {
                profiles: vec!["rev1".to_owned()],
                max_reworks: 2,
                rework_profile: None,
            },
            profiles: vec![test_profile("local"), test_profile("rev1")],
            learning: Default::default(),
            escalation: Default::default(),
        };

        run_task_with_backends(&mut fix.store, &config, &fix.state, task_id, &backends)
            .await
            .unwrap();

        let results =
            review_task_with_backends(&mut fix.store, &config, &fix.state, task_id, &backends)
                .await
                .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].verdict, Verdict::Escalate);
        assert!(results[0].feedback.contains("Unparseable"));
    }

    #[tokio::test]
    async fn run_review_loop_approves_task() {
        let mut fix = fixture();
        let task_id = add_task(&mut fix.store, "Add unit tests");

        let worker = WorkerBackend::new("test.rs");
        let rev = SequentialReviewBackend::new(vec!["VERDICT: PASS\n\nLGTM".to_owned()]);

        let mut backends: HashMap<&str, &dyn Backend> = HashMap::new();
        backends.insert("local", &worker);
        backends.insert("rev", &rev);

        let config = Config {
            repo: fix.repo.clone(),
            default_profile: "local".to_owned(),
            review: overseer_core::ReviewConfig {
                profiles: vec!["rev".to_owned()],
                max_reworks: 2,
                rework_profile: None,
            },
            profiles: vec![test_profile("local"), test_profile("rev")],
            learning: Default::default(),
            escalation: Default::default(),
        };

        run_task_with_backends(&mut fix.store, &config, &fix.state, task_id, &backends)
            .await
            .unwrap();

        let status =
            run_review_loop_with_backends(&mut fix.store, &config, &fix.state, task_id, &backends)
                .await
                .unwrap();

        assert_eq!(status, TaskStatus::Review);
        let task = fix.store.get_task(task_id).unwrap();
        assert!(task.approved);
        assert_eq!(task.status, TaskStatus::Review);
    }

    #[tokio::test]
    async fn run_review_loop_reworks_and_approves() {
        let mut fix = fixture();
        let task_id = add_task(&mut fix.store, "Rework feature");

        let worker = WorkerBackend::new("work.txt");
        // Round 0: CHANGES. Round 1: PASS.
        let rev = SequentialReviewBackend::new(vec![
            "VERDICT: CHANGES\n\n- Fix line 1".to_owned(),
            "VERDICT: PASS\n\nAll good now".to_owned(),
        ]);

        let mut backends: HashMap<&str, &dyn Backend> = HashMap::new();
        backends.insert("local", &worker);
        backends.insert("rev", &rev);

        let config = Config {
            repo: fix.repo.clone(),
            default_profile: "local".to_owned(),
            review: overseer_core::ReviewConfig {
                profiles: vec!["rev".to_owned()],
                max_reworks: 2,
                rework_profile: None,
            },
            profiles: vec![test_profile("local"), test_profile("rev")],
            learning: Default::default(),
            escalation: Default::default(),
        };

        run_task_with_backends(&mut fix.store, &config, &fix.state, task_id, &backends)
            .await
            .unwrap();

        let status =
            run_review_loop_with_backends(&mut fix.store, &config, &fix.state, task_id, &backends)
                .await
                .unwrap();

        assert_eq!(status, TaskStatus::Review);
        let task = fix.store.get_task(task_id).unwrap();
        assert!(task.approved);
        assert_eq!(task.round, 1);

        // Verify review files for round 0 and round 1
        assert!(fix.state.join("reviews/task-1/round-0/rev.md").is_file());
        assert!(fix.state.join("reviews/task-1/round-1/rev.md").is_file());
    }

    #[tokio::test]
    async fn run_review_loop_rework_limit_escalates() {
        let mut fix = fixture();
        let task_id = add_task(&mut fix.store, "Never approved");

        let worker = WorkerBackend::new("work.txt");
        // Round 0: CHANGES. Round 1: CHANGES (max_reworks = 1 reached).
        let rev = SequentialReviewBackend::new(vec![
            "VERDICT: CHANGES\n\n- Problem 1".to_owned(),
            "VERDICT: CHANGES\n\n- Problem 2".to_owned(),
        ]);

        let mut backends: HashMap<&str, &dyn Backend> = HashMap::new();
        backends.insert("local", &worker);
        backends.insert("rev", &rev);

        let config = Config {
            repo: fix.repo.clone(),
            default_profile: "local".to_owned(),
            review: overseer_core::ReviewConfig {
                profiles: vec!["rev".to_owned()],
                max_reworks: 1,
                rework_profile: None,
            },
            profiles: vec![test_profile("local"), test_profile("rev")],
            learning: Default::default(),
            escalation: Default::default(),
        };

        run_task_with_backends(&mut fix.store, &config, &fix.state, task_id, &backends)
            .await
            .unwrap();

        let status =
            run_review_loop_with_backends(&mut fix.store, &config, &fix.state, task_id, &backends)
                .await
                .unwrap();

        assert_eq!(status, TaskStatus::NeedsHuman);
        let task = fix.store.get_task(task_id).unwrap();
        assert!(!task.approved);
        assert_eq!(task.status, TaskStatus::NeedsHuman);
    }

    #[tokio::test]
    async fn rework_touching_sensitive_paths_escalates_to_needs_human() {
        let mut fix = fixture();
        let task_id = add_task(&mut fix.store, "rework sensitive");

        struct EscalatingWorker {
            counter: AtomicUsize,
        }
        #[async_trait]
        impl Backend for EscalatingWorker {
            async fn run(&self, req: &RunRequest) -> Result<RunOutcome, BackendError> {
                let n = self.counter.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    std::fs::write(req.workdir.join("work.txt"), "normal content\n").unwrap();
                } else {
                    std::fs::write(req.workdir.join("Cargo.toml"), "# modified in rework\n")
                        .unwrap();
                }
                Ok(RunOutcome {
                    output: format!("round {n}"),
                    usage: Usage::default(),
                    failure: None,
                })
            }
        }

        let worker = EscalatingWorker {
            counter: AtomicUsize::new(0),
        };
        let rev = SequentialReviewBackend::new(vec![
            "VERDICT: CHANGES\n\n- Fix something".to_owned(),
            "VERDICT: PASS\n\nShould not be called".to_owned(),
        ]);

        let mut backends: HashMap<&str, &dyn Backend> = HashMap::new();
        backends.insert("local", &worker);
        backends.insert("rev", &rev);

        let config = Config {
            repo: fix.repo.clone(),
            default_profile: "local".to_owned(),
            review: overseer_core::ReviewConfig {
                profiles: vec!["rev".to_owned()],
                max_reworks: 2,
                rework_profile: None,
            },
            profiles: vec![test_profile("local"), test_profile("rev")],
            learning: Default::default(),
            escalation: Default::default(),
        };

        run_task_with_backends(&mut fix.store, &config, &fix.state, task_id, &backends)
            .await
            .unwrap();

        let status =
            run_review_loop_with_backends(&mut fix.store, &config, &fix.state, task_id, &backends)
                .await
                .unwrap();

        assert_eq!(status, TaskStatus::NeedsHuman);
        let task = fix.store.get_task(task_id).unwrap();
        assert_eq!(task.status, TaskStatus::NeedsHuman);
        assert_eq!(task.reason.as_deref(), Some("escalation: Cargo.toml"));
        assert_eq!(rev.responses.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn run_review_loop_reviewer_escalate_sets_reason() {
        let mut fix = fixture();
        let task_id = add_task(&mut fix.store, "rework escalate");

        let worker = WorkerBackend::new("work.txt");
        let rev = SequentialReviewBackend::new(vec![
            "VERDICT: ESCALATE\n\n- Needs human approval for migration".to_owned(),
        ]);

        let mut backends: HashMap<&str, &dyn Backend> = HashMap::new();
        backends.insert("local", &worker);
        backends.insert("rev", &rev);

        let config = Config {
            repo: fix.repo.clone(),
            default_profile: "local".to_owned(),
            review: overseer_core::ReviewConfig {
                profiles: vec!["rev".to_owned()],
                max_reworks: 1,
                rework_profile: None,
            },
            profiles: vec![test_profile("local"), test_profile("rev")],
            learning: Default::default(),
            escalation: Default::default(),
        };

        run_task_with_backends(&mut fix.store, &config, &fix.state, task_id, &backends)
            .await
            .unwrap();

        let status =
            run_review_loop_with_backends(&mut fix.store, &config, &fix.state, task_id, &backends)
                .await
                .unwrap();

        assert_eq!(status, TaskStatus::NeedsHuman);
        let task = fix.store.get_task(task_id).unwrap();
        assert_eq!(task.status, TaskStatus::NeedsHuman);
        let reason = task.reason.as_deref().expect("expected escalation reason");
        assert!(
            reason.starts_with("escalation: reviewer:"),
            "expected prefix 'escalation: reviewer:', got {reason:?}"
        );
        assert!(
            reason.contains("Needs human approval for migration"),
            "expected reason to contain bullet text, got {reason:?}"
        );
    }

    #[test]
    fn loop_profile_names_include_the_task_profile() {
        let config = Config::from_toml_str(
            r#"
repo = "/r"
default_profile = "a"

[review]
profiles = ["rev"]

[[profiles]]
name = "a"
backend = "openai"
base_url = "http://127.0.0.1:1/v1"
model = "m"

[[profiles]]
name = "rev"
backend = "openai"
base_url = "http://127.0.0.1:1/v1"
model = "m"

[[profiles]]
name = "mine"
backend = "openai"
base_url = "http://127.0.0.1:1/v1"
model = "m"
"#,
        )
        .unwrap();
        assert_eq!(
            loop_profile_names(&config, Some("mine")),
            ["rev", "mine", "a"]
        );
        assert_eq!(loop_profile_names(&config, None), ["rev", "a"]);
        assert_eq!(loop_profile_names(&config, Some("rev")), ["rev", "a"]);
    }
}
