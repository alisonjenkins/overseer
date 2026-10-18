//! Task worktrees and branches, driven through the `git` binary.

use std::path::{Path, PathBuf};

use overseer_core::TaskId;
use tokio::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("failed to create directory {path}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to start `git {command}`")]
    SpawnGit {
        command: String,
        #[source]
        source: std::io::Error,
    },

    #[error("`git {command}` failed: {stderr}")]
    GitFailed { command: String, stderr: String },
}

/// A checked-out task branch in its own directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
}

pub fn branch_name(task_id: TaskId) -> String {
    format!("overseer/task-{task_id}")
}

pub fn worktree_path(state_dir: &Path, task_id: TaskId) -> PathBuf {
    state_dir.join("worktrees").join(format!("task-{task_id}"))
}

/// Create `<state_dir>/worktrees/task-<id>` on a new branch
/// `overseer/task-<id>`, starting from the repository's current HEAD.
pub async fn create(
    repo: &Path,
    state_dir: &Path,
    task_id: TaskId,
) -> Result<Worktree, WorktreeError> {
    let parent = state_dir.join("worktrees");
    tokio::fs::create_dir_all(&parent)
        .await
        .map_err(|source| WorktreeError::CreateDirectory {
            path: parent.clone(),
            source,
        })?;
    let path = worktree_path(state_dir, task_id);
    let branch = branch_name(task_id);
    let path_text = path.to_string_lossy().into_owned();
    git(
        repo,
        &["worktree", "add", "-b", &branch, &path_text, "HEAD"],
    )
    .await?;
    Ok(Worktree { path, branch })
}

/// Stage everything in `worktree` and commit it. Returns the new commit's
/// sha, or `None` when the worktree has no changes to commit.
pub async fn commit_all(worktree: &Path, message: &str) -> Result<Option<String>, WorktreeError> {
    git(worktree, &["add", "-A"]).await?;
    let pending = git(worktree, &["status", "--porcelain"]).await?;
    if pending.trim().is_empty() {
        return Ok(None);
    }
    git(worktree, &["commit", "-q", "-m", message]).await?;
    let sha = git(worktree, &["rev-parse", "HEAD"]).await?;
    Ok(Some(sha.trim().to_owned()))
}

/// Force-remove a worktree directory. The branch is left in place.
pub async fn remove_worktree(repo: &Path, worktree: &Path) -> Result<(), WorktreeError> {
    let path_text = worktree.to_string_lossy().into_owned();
    git(repo, &["worktree", "remove", "--force", &path_text]).await?;
    Ok(())
}

async fn git(dir: &Path, args: &[&str]) -> Result<String, WorktreeError> {
    let command = args.join(" ");
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .await
        .map_err(|source| WorktreeError::SpawnGit {
            command: command.clone(),
            source,
        })?;
    if !output.status.success() {
        return Err(WorktreeError::GitFailed {
            command,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::Path;

    use super::*;
    use crate::test_support::{git_sync, init_repo};

    #[test]
    fn branch_and_path_names_follow_the_task_id() {
        let id = TaskId::new(7);
        assert_eq!(branch_name(id), "overseer/task-7");
        assert_eq!(
            worktree_path(Path::new("/state"), id),
            PathBuf::from("/state/worktrees/task-7")
        );
    }

    #[tokio::test]
    async fn create_checks_out_a_new_branch_from_head() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let state = root.path().join("state");

        let worktree = create(&repo, &state, TaskId::new(3)).await.unwrap();

        assert_eq!(worktree.branch, "overseer/task-3");
        assert_eq!(worktree.path, state.join("worktrees").join("task-3"));
        assert!(worktree.path.join("README.md").is_file());
        let branch_head = git_sync(&repo, &["rev-parse", "refs/heads/overseer/task-3"]);
        let main_head = git_sync(&repo, &["rev-parse", "refs/heads/main"]);
        assert_eq!(branch_head, main_head);
    }

    #[tokio::test]
    async fn commit_all_commits_changes_and_returns_the_sha() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let worktree = create(&repo, &root.path().join("state"), TaskId::new(1))
            .await
            .unwrap();
        std::fs::write(worktree.path.join("parser.rs"), "fn parse() {}\n").unwrap();

        let sha = commit_all(&worktree.path, "overseer: task 1: write parser")
            .await
            .unwrap()
            .expect("changes were committed");

        let head = git_sync(&worktree.path, &["rev-parse", "HEAD"]);
        assert_eq!(sha, head.trim());
        let subject = git_sync(&worktree.path, &["log", "-1", "--format=%s"]);
        assert_eq!(subject.trim(), "overseer: task 1: write parser");
        let files = git_sync(
            &worktree.path,
            &["show", "--name-only", "--format=", "HEAD"],
        );
        assert_eq!(files.trim(), "parser.rs");
    }

    #[tokio::test]
    async fn commit_all_without_changes_returns_none() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let worktree = create(&repo, &root.path().join("state"), TaskId::new(2))
            .await
            .unwrap();
        let before = git_sync(&worktree.path, &["rev-parse", "HEAD"]);

        let sha = commit_all(&worktree.path, "nothing here").await.unwrap();

        assert_eq!(sha, None);
        assert_eq!(git_sync(&worktree.path, &["rev-parse", "HEAD"]), before);
    }

    #[tokio::test]
    async fn remove_worktree_deletes_the_directory_and_keeps_the_branch() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let worktree = create(&repo, &root.path().join("state"), TaskId::new(4))
            .await
            .unwrap();

        remove_worktree(&repo, &worktree.path).await.unwrap();

        assert!(!worktree.path.exists());
        let branches = git_sync(&repo, &["branch", "--list", "overseer/task-4"]);
        assert!(branches.contains("overseer/task-4"), "{branches}");
    }

    #[tokio::test]
    async fn creating_a_worktree_twice_is_a_git_failure() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let state = root.path().join("state");
        create(&repo, &state, TaskId::new(5)).await.unwrap();

        let err = create(&repo, &state, TaskId::new(5)).await.unwrap_err();
        assert!(matches!(err, WorktreeError::GitFailed { .. }), "{err:?}");
    }
}
