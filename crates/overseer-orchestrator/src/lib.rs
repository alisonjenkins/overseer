//! Runs a task end to end: claims it, prepares a worktree and branch, calls
//! the backend, records the run, commits the result and sets the task status.

mod runner;
mod worktree;

#[cfg(test)]
mod test_support;

pub use runner::{RunSummary, RunnerError, run_task, run_task_with_backend};
pub use worktree::{Worktree, WorktreeError, branch_name, remove_worktree, worktree_path};
