//! Runs a task end to end: claims it, prepares a worktree and branch, calls
//! the backend, records the run, commits the result and sets the task status.

mod lock;
mod recovery;
mod router;
mod runner;
mod worker;
mod worktree;

#[cfg(test)]
mod test_support;

pub use lock::{WorkerLock, WorkerLockError, check_worker_status};
pub use recovery::{RecoveryError, recover_interrupted};
pub use router::{RouterError, plan_attempts};
pub use runner::{RunSummary, RunnerError, run_task, run_task_with_backend};
pub use worker::{ShutdownSignal, WorkerError, WorkerOptions, WorkerStats, run_worker};
pub use worktree::{
    MergeError, Worktree, WorktreeError, branch_name, diff, diff_stat, discard, merge,
    remove_worktree, worktree_path,
};
