#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use overseer_core::{TaskId, TaskStatus};

fn overseer(state_dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("overseer").unwrap();
    cmd.arg("--state-dir").arg(state_dir);
    cmd
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = StdCommand::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn init_git_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "Overseer Test"]);
    git(dir, &["config", "user.email", "test@example.invalid"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("README.md"), "initial\n").unwrap();
    git(dir, &["add", "README.md"]);
    git(dir, &["commit", "-q", "-m", "initial commit"]);
}

#[test]
fn diff_prints_unified_diff_and_stat() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    let state = root.path().join("state");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    let repo_path = repo.to_string_lossy().into_owned();
    overseer(&state)
        .args(["init", "--repo", &repo_path])
        .assert()
        .success();

    // Create a task row
    overseer(&state)
        .args(["task", "add", "write parser"])
        .assert()
        .success();

    // Create task branch and commit made by test
    git(&repo, &["checkout", "-q", "-b", "overseer/task-1"]);
    std::fs::write(repo.join("parser.rs"), "fn parse() {}\n").unwrap();
    git(&repo, &["add", "parser.rs"]);
    git(&repo, &["commit", "-q", "-m", "implement parser"]);
    git(&repo, &["checkout", "-q", "main"]);

    // Move task to review in store
    let mut store = overseer_store::Store::open(&state.join("overseer.db")).unwrap();
    store
        .set_status(TaskId::new(1), TaskStatus::Review)
        .unwrap();

    let diff_out = overseer(&state)
        .args(["diff", "1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let diff_str = String::from_utf8(diff_out).unwrap();
    assert!(
        diff_str.contains("diff --git a/parser.rs b/parser.rs"),
        "{diff_str}"
    );
    assert!(diff_str.contains("+fn parse() {}"), "{diff_str}");

    let stat_out = overseer(&state)
        .args(["diff", "1", "--stat"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stat_str = String::from_utf8(stat_out).unwrap();
    assert!(stat_str.contains("parser.rs"), "{stat_str}");
    assert!(stat_str.contains("1 file changed"), "{stat_str}");
}

#[test]
fn merge_refuses_when_not_in_review() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    let state = root.path().join("state");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    let repo_path = repo.to_string_lossy().into_owned();
    overseer(&state)
        .args(["init", "--repo", &repo_path])
        .assert()
        .success();

    overseer(&state)
        .args(["task", "add", "backlog task"])
        .assert()
        .success();

    let output = overseer(&state)
        .args(["merge", "1"])
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).unwrap();
    assert!(
        stderr.contains("only tasks in review can be merged"),
        "{stderr}"
    );
}

#[test]
fn merge_refuses_when_working_tree_is_dirty() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    let state = root.path().join("state");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    let repo_path = repo.to_string_lossy().into_owned();
    overseer(&state)
        .args(["init", "--repo", &repo_path])
        .assert()
        .success();

    overseer(&state)
        .args(["task", "add", "task to merge"])
        .assert()
        .success();

    git(&repo, &["checkout", "-q", "-b", "overseer/task-1"]);
    std::fs::write(repo.join("file.txt"), "hello\n").unwrap();
    git(&repo, &["add", "file.txt"]);
    git(&repo, &["commit", "-q", "-m", "add file"]);
    git(&repo, &["checkout", "-q", "main"]);

    let mut store = overseer_store::Store::open(&state.join("overseer.db")).unwrap();
    store
        .set_status(TaskId::new(1), TaskStatus::Review)
        .unwrap();

    // Dirty the repo working tree
    std::fs::write(repo.join("dirty.txt"), "uncommitted\n").unwrap();

    let output = overseer(&state)
        .args(["merge", "1"])
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).unwrap();
    assert!(stderr.contains("working tree is not clean"), "{stderr}");
}

#[test]
fn merge_refuses_when_on_wrong_branch() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    let state = root.path().join("state");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    let repo_path = repo.to_string_lossy().into_owned();
    overseer(&state)
        .args(["init", "--repo", &repo_path])
        .assert()
        .success();

    overseer(&state)
        .args(["task", "add", "task to merge"])
        .assert()
        .success();

    git(&repo, &["checkout", "-q", "-b", "overseer/task-1"]);
    std::fs::write(repo.join("file.txt"), "hello\n").unwrap();
    git(&repo, &["add", "file.txt"]);
    git(&repo, &["commit", "-q", "-m", "add file"]);
    git(&repo, &["checkout", "-q", "main"]);
    git(&repo, &["checkout", "-q", "-b", "feature-x"]);

    let mut store = overseer_store::Store::open(&state.join("overseer.db")).unwrap();
    store
        .set_status(TaskId::new(1), TaskStatus::Review)
        .unwrap();

    let output = overseer(&state)
        .args(["merge", "1"])
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).unwrap();
    assert!(
        stderr.contains("current branch is \"feature-x\""),
        "{stderr}"
    );
}

#[test]
fn merge_refuses_on_conflict_without_touching_repo() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    let state = root.path().join("state");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    let repo_path = repo.to_string_lossy().into_owned();
    overseer(&state)
        .args(["init", "--repo", &repo_path])
        .assert()
        .success();

    overseer(&state)
        .args(["task", "add", "conflicting task"])
        .assert()
        .success();

    git(&repo, &["checkout", "-q", "-b", "overseer/task-1"]);
    std::fs::write(repo.join("README.md"), "task version\n").unwrap();
    git(&repo, &["commit", "-q", "-am", "task edit"]);
    git(&repo, &["checkout", "-q", "main"]);

    // Main advances with conflicting change
    std::fs::write(repo.join("README.md"), "main version\n").unwrap();
    git(&repo, &["commit", "-q", "-am", "main edit"]);

    let mut store = overseer_store::Store::open(&state.join("overseer.db")).unwrap();
    store
        .set_status(TaskId::new(1), TaskStatus::Review)
        .unwrap();

    let output = overseer(&state)
        .args(["merge", "1"])
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).unwrap();
    assert!(stderr.contains("merge conflict in README.md"), "{stderr}");

    // Working tree is untouched
    let status = git(&repo, &["status", "--porcelain"]);
    assert!(status.trim().is_empty(), "{status}");
    assert_eq!(
        std::fs::read_to_string(repo.join("README.md")).unwrap(),
        "main version\n"
    );
}

#[test]
fn merge_succeeds_and_sets_done() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    let state = root.path().join("state");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    let repo_path = repo.to_string_lossy().into_owned();
    overseer(&state)
        .args(["init", "--repo", &repo_path])
        .assert()
        .success();

    overseer(&state)
        .args(["task", "add", "write tests"])
        .assert()
        .success();

    git(&repo, &["checkout", "-q", "-b", "overseer/task-1"]);
    std::fs::write(repo.join("test.rs"), "fn test_foo() {}\n").unwrap();
    git(&repo, &["add", "test.rs"]);
    git(&repo, &["commit", "-q", "-m", "add test"]);
    git(&repo, &["checkout", "-q", "main"]);

    let mut store = overseer_store::Store::open(&state.join("overseer.db")).unwrap();
    store
        .set_status(TaskId::new(1), TaskStatus::Review)
        .unwrap();

    let output = overseer(&state)
        .args(["merge", "1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let sha = String::from_utf8(output).unwrap().trim().to_owned();
    let head = git(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
    assert_eq!(sha, head);

    let subject = git(&repo, &["log", "-1", "--format=%s"]);
    assert_eq!(subject.trim(), "overseer: merge task 1: write tests");

    // Task is now done in store
    let task = store.get_task(TaskId::new(1)).unwrap();
    assert_eq!(task.status, TaskStatus::Done);

    // Task branch was deleted
    let branches = git(&repo, &["branch", "--list", "overseer/task-1"]);
    assert!(branches.trim().is_empty(), "{branches}");
}

#[test]
fn discard_refuses_when_not_review_or_blocked() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    let state = root.path().join("state");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    let repo_path = repo.to_string_lossy().into_owned();
    overseer(&state)
        .args(["init", "--repo", &repo_path])
        .assert()
        .success();

    overseer(&state)
        .args(["task", "add", "ready task"])
        .assert()
        .success();

    let mut store = overseer_store::Store::open(&state.join("overseer.db")).unwrap();
    store.set_status(TaskId::new(1), TaskStatus::Ready).unwrap();

    let output = overseer(&state)
        .args(["discard", "1"])
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).unwrap();
    assert!(
        stderr.contains("only tasks in review or blocked can be discarded"),
        "{stderr}"
    );
}

#[test]
fn discard_succeeds_for_review_and_blocked_tasks() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    let state = root.path().join("state");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    let repo_path = repo.to_string_lossy().into_owned();
    overseer(&state)
        .args(["init", "--repo", &repo_path])
        .assert()
        .success();

    overseer(&state)
        .args(["task", "add", "review task"])
        .assert()
        .success();
    overseer(&state)
        .args(["task", "add", "blocked task"])
        .assert()
        .success();

    git(&repo, &["checkout", "-q", "-b", "overseer/task-1"]);
    std::fs::write(repo.join("discard1.txt"), "d1\n").unwrap();
    git(&repo, &["add", "discard1.txt"]);
    git(&repo, &["commit", "-q", "-m", "d1"]);

    git(&repo, &["checkout", "-q", "-b", "overseer/task-2"]);
    std::fs::write(repo.join("discard2.txt"), "d2\n").unwrap();
    git(&repo, &["add", "discard2.txt"]);
    git(&repo, &["commit", "-q", "-m", "d2"]);
    git(&repo, &["checkout", "-q", "main"]);

    let mut store = overseer_store::Store::open(&state.join("overseer.db")).unwrap();
    store
        .set_status(TaskId::new(1), TaskStatus::Review)
        .unwrap();
    store
        .set_status(TaskId::new(2), TaskStatus::Blocked)
        .unwrap();

    overseer(&state).args(["discard", "1"]).assert().success();
    let task1 = store.get_task(TaskId::new(1)).unwrap();
    assert_eq!(task1.status, TaskStatus::Backlog);
    let b1 = git(&repo, &["branch", "--list", "overseer/task-1"]);
    assert!(b1.trim().is_empty(), "{b1}");

    overseer(&state).args(["discard", "2"]).assert().success();
    let task2 = store.get_task(TaskId::new(2)).unwrap();
    assert_eq!(task2.status, TaskStatus::Backlog);
    let b2 = git(&repo, &["branch", "--list", "overseer/task-2"]);
    assert!(b2.trim().is_empty(), "{b2}");
}
