//! Runs a task end to end: claims it, prepares a worktree and branch, calls
//! the backend, records the run, commits the result and sets the task status.

pub mod doctor;
pub mod escalation;
pub mod landing;
pub mod learning;
mod lock;
mod recovery;
mod review;
mod router;
mod runner;
mod sources;
mod sync;
pub mod watch;
mod worker;
mod worktree;

#[cfg(test)]
mod test_support;

pub use doctor::{PlanEntry, ProfileReport, Report, Verdict, doctor_report};
pub use escalation::escalation_hits;
pub use landing::{LandOutcome, LandingError, land};
pub use learning::{
    RetrospectError, has_learning_signal, retrospect_task, retrospect_task_with_backend,
    retrospect_task_with_backends,
};
pub use lock::{WorkerLock, WorkerLockError, check_worker_status};
pub use recovery::{RecoveryError, recover_interrupted};
pub use review::{
    ReviewError, ReviewOutcome, ReviewerResult, aggregate_verdicts, build_reviewer_prompt,
    build_rework_prompt, overall_verdict, review_task, review_task_with_backends, run_review_loop,
    run_review_loop_with_backends,
};
pub use router::{RouterError, plan_attempts};
pub use runner::{
    RunSummary, RunnerError, build_system_prompt, continue_task, continue_task_with_backend,
    continue_task_with_backends, run_task, run_task_with_backend,
};
pub use sources::{SourceBuildError, SpecKitSource, build_sources};
pub use sync::{SyncError, SyncReport, sync_sources};
pub use watch::{SourceWatcher, WatchError, start};
pub use worker::{ShutdownSignal, WorkerError, WorkerOptions, WorkerStats, run_worker};
pub use worktree::{
    MergeError, Worktree, WorktreeError, branch_content_on_head, branch_name, diff, diff_stat,
    discard, merge, remove_worktree, worktree_path,
};
