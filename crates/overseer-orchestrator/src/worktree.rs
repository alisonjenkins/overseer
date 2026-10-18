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

#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    #[error("repository working tree is not clean; commit or stash changes before merging")]
    WorkingTreeDirty,

    #[error(
        "current branch is {current:?}, but task {task_id} branched from {expected:?}; switch to {expected:?} before merging"
    )]
    WrongBranch {
        task_id: TaskId,
        current: String,
        expected: String,
    },

    #[error("merge conflict in {}", paths.join(", "))]
    MergeConflict { paths: Vec<String> },

    #[error("failed to start `git {command}`")]
    SpawnGit {
        command: String,
        #[source]
        source: std::io::Error,
    },

    #[error("`git {command}` failed: {stderr}")]
    GitFailed { command: String, stderr: String },

    #[error("failed to remove worktree {path}")]
    RemoveWorktree {
        path: PathBuf,
        #[source]
        source: WorktreeError,
    },
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
    let head_commit = git(repo, &["rev-parse", "HEAD"])
        .await
        .ok()
        .map(|s| s.trim().to_owned());
    let head_branch = git(repo, &["symbolic-ref", "--short", "HEAD"])
        .await
        .ok()
        .map(|s| s.trim().to_owned());
    git(
        repo,
        &["worktree", "add", "-b", &branch, &path_text, "HEAD"],
    )
    .await?;
    if let Some(commit) = head_commit {
        let _ = git(
            repo,
            &[
                "config",
                &format!("branch.{branch}.overseer-base-commit"),
                &commit,
            ],
        )
        .await;
    }
    if let Some(base_branch) = head_branch {
        let _ = git(
            repo,
            &[
                "config",
                &format!("branch.{branch}.overseer-base"),
                &base_branch,
            ],
        )
        .await;
    }
    Ok(Worktree { path, branch })
}

/// Unified diff between the merge base and `branch`.
pub async fn diff(repo: &Path, branch: &str) -> Result<String, WorktreeError> {
    diff_internal(repo, branch, false).await
}

/// Diffstat summary between the merge base and `branch`.
pub async fn diff_stat(repo: &Path, branch: &str) -> Result<String, WorktreeError> {
    diff_internal(repo, branch, true).await
}

async fn diff_internal(repo: &Path, branch: &str, stat: bool) -> Result<String, WorktreeError> {
    let base = match git(repo, &["merge-base", "HEAD", branch]).await {
        Ok(output) if !output.trim().is_empty() => output.trim().to_owned(),
        _ => match git(
            repo,
            &[
                "config",
                "--get",
                &format!("branch.{branch}.overseer-base-commit"),
            ],
        )
        .await
        {
            Ok(output) if !output.trim().is_empty() => output.trim().to_owned(),
            _ => "HEAD".to_owned(),
        },
    };
    let range = format!("{base}...{branch}");
    let mut args = vec!["diff", "--no-ext-diff"];
    if stat {
        args.push("--stat");
    }
    args.push(&range);
    git(repo, &args).await
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

/// Drop every uncommitted change in `worktree`, tracked and untracked, so the
/// next attempt starts from the task branch's last commit.
pub async fn discard_changes(worktree: &Path) -> Result<(), WorktreeError> {
    git(worktree, &["reset", "--hard", "HEAD"]).await?;
    git(worktree, &["clean", "-fd"]).await?;
    Ok(())
}

/// Force-remove a worktree directory. The branch is left in place.
pub async fn remove_worktree(repo: &Path, worktree: &Path) -> Result<(), WorktreeError> {
    let path_text = worktree.to_string_lossy().into_owned();
    git(repo, &["worktree", "remove", "--force", &path_text]).await?;
    Ok(())
}

/// Merge a task branch into the repository's current branch.
///
/// Refuses if the working tree is dirty or the current branch differs from the
/// branch the task branched from. Runs `git merge-tree --write-tree` first and
/// returns a `MergeConflict` error listing conflicted paths without modifying
/// the repository. Otherwise performs a `--no-ff` merge, removes the task
/// worktree, and deletes the branch.
pub async fn merge(
    repo: &Path,
    worktree: Option<&Path>,
    branch: &str,
    task_id: TaskId,
    title: &str,
) -> Result<String, MergeError> {
    let status =
        git(repo, &["status", "--porcelain"])
            .await
            .map_err(|err| MergeError::GitFailed {
                command: "status --porcelain".to_owned(),
                stderr: err.to_string(),
            })?;
    if !status.trim().is_empty() {
        return Err(MergeError::WorkingTreeDirty);
    }

    let current_branch = git(repo, &["symbolic-ref", "--short", "HEAD"])
        .await
        .map_err(|err| MergeError::GitFailed {
            command: "symbolic-ref --short HEAD".to_owned(),
            stderr: err.to_string(),
        })?
        .trim()
        .to_owned();

    if let Some(expected) = base_branch_for(repo, branch).await
        && current_branch != expected
    {
        return Err(MergeError::WrongBranch {
            task_id,
            current: current_branch,
            expected,
        });
    }

    let command_name = format!("merge-tree --write-tree HEAD {branch}");
    let merge_tree_output = Command::new("git")
        .current_dir(repo)
        .args(["merge-tree", "--write-tree", "HEAD", branch])
        .output()
        .await
        .map_err(|source| MergeError::SpawnGit {
            command: command_name,
            source,
        })?;
    if !merge_tree_output.status.success() {
        let stdout = String::from_utf8_lossy(&merge_tree_output.stdout);
        let paths = parse_merge_conflicts(&stdout);
        return Err(MergeError::MergeConflict { paths });
    }

    let message = format!("overseer: merge task {task_id}: {title}");
    git(repo, &["merge", "--no-ff", "-m", &message, branch])
        .await
        .map_err(|err| MergeError::GitFailed {
            command: format!("merge --no-ff -m <message> {branch}"),
            stderr: err.to_string(),
        })?;

    let sha = git(repo, &["rev-parse", "HEAD"])
        .await
        .map_err(|err| MergeError::GitFailed {
            command: "rev-parse HEAD".to_owned(),
            stderr: err.to_string(),
        })?
        .trim()
        .to_owned();

    let target_wt = match worktree {
        Some(path) if path.exists() => Some(path.to_path_buf()),
        _ => find_worktree_for_branch(repo, branch).await,
    };
    if let Some(path) = target_wt
        && path.exists()
    {
        remove_worktree(repo, &path)
            .await
            .map_err(|source| MergeError::RemoveWorktree {
                path: path.clone(),
                source,
            })?;
    }

    git(repo, &["branch", "-d", branch])
        .await
        .map_err(|err| MergeError::GitFailed {
            command: format!("branch -d {branch}"),
            stderr: err.to_string(),
        })?;

    Ok(sha)
}

/// Remove a task worktree (if present) and force-delete the task branch.
pub async fn discard(
    repo: &Path,
    worktree: Option<&Path>,
    branch: &str,
) -> Result<(), WorktreeError> {
    let target_wt = match worktree {
        Some(path) if path.exists() => Some(path.to_path_buf()),
        _ => find_worktree_for_branch(repo, branch).await,
    };
    if let Some(path) = target_wt
        && path.exists()
    {
        remove_worktree(repo, &path).await?;
    }
    git(repo, &["branch", "-D", branch]).await?;
    Ok(())
}

async fn base_branch_for(repo: &Path, branch: &str) -> Option<String> {
    if let Ok(val) = git(
        repo,
        &["config", "--get", &format!("branch.{branch}.overseer-base")],
    )
    .await
    {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }

    let checkout_target = format!(" to {branch}");
    if let Ok(head_reflog) = git(repo, &["reflog", "show", "--format=%gs", "HEAD"]).await {
        for line in head_reflog.lines() {
            if let Some(rest) = line.strip_prefix("checkout: moving from ")
                && let Some((source, _)) = rest.split_once(&checkout_target)
            {
                let trimmed = source.trim();
                if !trimmed.is_empty() && trimmed != "HEAD" {
                    return Some(trimmed.to_owned());
                }
            }
        }
    }

    if let Ok(reflog) = git(repo, &["reflog", "show", "--format=%gs", branch]).await {
        for line in reflog.lines() {
            if let Some(rest) = line.strip_prefix("branch: Created from ") {
                let trimmed = rest.trim();
                if !trimmed.is_empty() && trimmed != "HEAD" {
                    return Some(trimmed.to_owned());
                }
            }
        }
    }

    if let Ok(output) = git(
        repo,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
    )
    .await
    {
        let branches: Vec<&str> = output
            .lines()
            .map(str::trim)
            .filter(|b| !b.is_empty() && !b.starts_with("overseer/"))
            .collect();
        if branches.len() == 1
            && let Some(b) = branches.first()
        {
            return Some((*b).to_owned());
        }

        if branches.contains(&"main") {
            return Some("main".to_owned());
        }
        if branches.contains(&"master") {
            return Some("master".to_owned());
        }

        let mut best_branch = None;
        let mut best_time = i64::MIN;
        for b in branches {
            if let Ok(mb) = git(repo, &["merge-base", b, branch]).await {
                let mb = mb.trim();
                if !mb.is_empty()
                    && let Ok(t_str) = git(repo, &["log", "-1", "--format=%ct", mb]).await
                    && let Ok(t) = t_str.trim().parse::<i64>()
                    && t > best_time
                {
                    best_time = t;
                    best_branch = Some(b.to_owned());
                }
            }
        }
        if best_branch.is_some() {
            return best_branch;
        }
    }

    None
}

fn parse_merge_conflicts(output: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in output.lines() {
        if let Some((_, path)) = line.split_once('\t') {
            let trimmed = path.trim();
            if !trimmed.is_empty() && !paths.contains(&trimmed.to_owned()) {
                paths.push(trimmed.to_owned());
            }
        } else if let Some(rest) = line.strip_prefix("CONFLICT (")
            && let Some((_, msg)) = rest.split_once(':')
        {
            let msg = msg.trim();
            let path = if let Some(p) = msg.strip_prefix("Merge conflict in ") {
                p.trim()
            } else {
                msg.split_whitespace().next().unwrap_or("").trim()
            };
            if !path.is_empty() && !paths.contains(&path.to_owned()) {
                paths.push(path.to_owned());
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

async fn find_worktree_for_branch(repo: &Path, branch: &str) -> Option<PathBuf> {
    let output = git(repo, &["worktree", "list", "--porcelain"]).await.ok()?;
    let target_ref = format!("refs/heads/{branch}");
    let mut current_wt: Option<PathBuf> = None;
    for line in output.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current_wt = Some(PathBuf::from(path.trim()));
        } else if let Some(b) = line.strip_prefix("branch ")
            && b.trim() == target_ref
        {
            return current_wt;
        }
    }
    None
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
    use crate::test_support::{git_sync, init_empty_repo, init_repo};

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
    async fn discard_changes_in_a_repo_with_an_empty_tree_removes_untracked_files() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_empty_repo(&repo);
        let worktree = create(&repo, &root.path().join("state"), TaskId::new(6))
            .await
            .unwrap();
        std::fs::write(worktree.path.join("scratch.txt"), "attempt output\n").unwrap();

        discard_changes(&worktree.path).await.unwrap();

        assert!(!worktree.path.join("scratch.txt").exists());
        let status = git_sync(&worktree.path, &["status", "--porcelain"]);
        assert!(status.trim().is_empty(), "{status}");
    }

    #[tokio::test]
    async fn discard_changes_restores_tracked_files_and_removes_untracked_ones() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let worktree = create(&repo, &root.path().join("state"), TaskId::new(7))
            .await
            .unwrap();
        std::fs::write(worktree.path.join("README.md"), "half-written\n").unwrap();
        std::fs::write(worktree.path.join("scratch.txt"), "attempt output\n").unwrap();

        discard_changes(&worktree.path).await.unwrap();

        let readme = std::fs::read_to_string(worktree.path.join("README.md")).unwrap();
        assert_eq!(readme, "base\n");
        assert!(!worktree.path.join("scratch.txt").exists());
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

    #[tokio::test]
    async fn diff_returns_unified_diff_against_merge_base() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let state = root.path().join("state");
        let worktree = create(&repo, &state, TaskId::new(10)).await.unwrap();

        std::fs::write(worktree.path.join("file.txt"), "hello from task\n").unwrap();
        commit_all(&worktree.path, "task 10 commit").await.unwrap();

        // Advance main with an independent commit
        std::fs::write(repo.join("other.txt"), "hello from main\n").unwrap();
        git_sync(&repo, &["add", "-A"]);
        git_sync(&repo, &["commit", "-m", "advance main"]);

        let unified = diff(&repo, &worktree.branch).await.unwrap();
        assert!(
            unified.contains("diff --git a/file.txt b/file.txt"),
            "{unified}"
        );
        assert!(unified.contains("+hello from task"), "{unified}");
        assert!(!unified.contains("other.txt"), "{unified}");

        let stat = diff_stat(&repo, &worktree.branch).await.unwrap();
        assert!(stat.contains("file.txt"), "{stat}");
        assert!(stat.contains("1 file changed"), "{stat}");
        assert!(!stat.contains("other.txt"), "{stat}");
    }

    #[tokio::test]
    async fn diff_without_changes_is_empty() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let state = root.path().join("state");
        let worktree = create(&repo, &state, TaskId::new(11)).await.unwrap();

        let unified = diff(&repo, &worktree.branch).await.unwrap();
        assert!(unified.trim().is_empty(), "{unified}");

        let stat = diff_stat(&repo, &worktree.branch).await.unwrap();
        assert!(stat.trim().is_empty(), "{stat}");
    }

    #[tokio::test]
    async fn merge_successful_merges_with_no_ff_and_cleans_up() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let state = root.path().join("state");
        let worktree = create(&repo, &state, TaskId::new(1)).await.unwrap();

        std::fs::write(worktree.path.join("feature.txt"), "feature output\n").unwrap();
        commit_all(&worktree.path, "task 1 commit").await.unwrap();

        let sha = merge(
            &repo,
            Some(&worktree.path),
            &worktree.branch,
            TaskId::new(1),
            "implement feature",
        )
        .await
        .unwrap();

        let head = git_sync(&repo, &["rev-parse", "HEAD"]);
        assert_eq!(sha, head.trim());

        let subject = git_sync(&repo, &["log", "-1", "--format=%s"]);
        assert_eq!(subject.trim(), "overseer: merge task 1: implement feature");

        let parent2 = git_sync(&repo, &["rev-parse", "HEAD^2"]);
        assert!(!parent2.trim().is_empty());

        assert!(!worktree.path.exists());
        let branches = git_sync(&repo, &["branch", "--list", "overseer/task-1"]);
        assert!(branches.trim().is_empty(), "{branches}");
        assert_eq!(
            std::fs::read_to_string(repo.join("feature.txt")).unwrap(),
            "feature output\n"
        );
    }

    #[tokio::test]
    async fn merge_refuses_when_working_tree_is_dirty() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let state = root.path().join("state");
        let worktree = create(&repo, &state, TaskId::new(2)).await.unwrap();

        std::fs::write(worktree.path.join("file.txt"), "task 2\n").unwrap();
        commit_all(&worktree.path, "task 2 commit").await.unwrap();

        std::fs::write(repo.join("dirty.txt"), "uncommitted\n").unwrap();

        let err = merge(
            &repo,
            Some(&worktree.path),
            &worktree.branch,
            TaskId::new(2),
            "title",
        )
        .await
        .unwrap_err();

        assert!(matches!(err, MergeError::WorkingTreeDirty), "{err:?}");
        assert!(!repo.join("file.txt").exists());
        assert!(worktree.path.exists());
    }

    #[tokio::test]
    async fn merge_refuses_when_on_wrong_branch() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let state = root.path().join("state");
        let worktree = create(&repo, &state, TaskId::new(3)).await.unwrap();

        git_sync(&repo, &["checkout", "-b", "other"]);

        let err = merge(
            &repo,
            Some(&worktree.path),
            &worktree.branch,
            TaskId::new(3),
            "title",
        )
        .await
        .unwrap_err();

        assert!(
            matches!(
                &err,
                MergeError::WrongBranch {
                    task_id,
                    current,
                    expected,
                } if *task_id == TaskId::new(3) && current == "other" && expected == "main"
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn merge_refuses_when_merge_conflict_and_leaves_tree_clean() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let state = root.path().join("state");
        let worktree = create(&repo, &state, TaskId::new(4)).await.unwrap();

        std::fs::write(worktree.path.join("README.md"), "task version\n").unwrap();
        commit_all(&worktree.path, "task 4 commit").await.unwrap();

        std::fs::write(repo.join("README.md"), "main version\n").unwrap();
        git_sync(&repo, &["commit", "-am", "main version commit"]);

        let err = merge(
            &repo,
            Some(&worktree.path),
            &worktree.branch,
            TaskId::new(4),
            "title",
        )
        .await
        .unwrap_err();

        let paths = match err {
            MergeError::MergeConflict { paths } => paths,
            _ => Vec::new(),
        };
        assert!(paths.contains(&"README.md".to_owned()), "{paths:?}");

        let status = git_sync(&repo, &["status", "--porcelain"]);
        assert!(status.trim().is_empty(), "{status}");
        assert_eq!(
            std::fs::read_to_string(repo.join("README.md")).unwrap(),
            "main version\n"
        );
        assert!(worktree.path.exists());
    }

    #[tokio::test]
    async fn discard_removes_worktree_and_deletes_branch() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let state = root.path().join("state");
        let worktree = create(&repo, &state, TaskId::new(5)).await.unwrap();

        discard(&repo, Some(&worktree.path), &worktree.branch)
            .await
            .unwrap();

        assert!(!worktree.path.exists());
        let branches = git_sync(&repo, &["branch", "--list", "overseer/task-5"]);
        assert!(branches.trim().is_empty(), "{branches}");
    }
}
