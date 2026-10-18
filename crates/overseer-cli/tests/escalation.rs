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
fn task_show_prints_reason_when_set() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");

    overseer(&state).arg("init").assert().success();

    overseer(&state)
        .args(["task", "add", "sensitive task"])
        .assert()
        .success();

    let mut store = overseer_store::Store::open(&state.join("overseer.db")).unwrap();
    store
        .set_status_with_reason(
            TaskId::new(1),
            TaskStatus::NeedsHuman,
            Some("escalation: Cargo.toml"),
        )
        .unwrap();

    let output = overseer(&state)
        .args(["task", "show", "1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();
    assert!(
        stdout.contains("reason: escalation: Cargo.toml"),
        "expected reason in stdout: {stdout}"
    );
}

#[test]
fn merge_refuses_needs_human_task_without_approve() {
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
        .args(["task", "add", "sensitive task"])
        .assert()
        .success();

    git(&repo, &["checkout", "-q", "-b", "overseer/task-1"]);
    std::fs::write(repo.join("Cargo.toml"), "# config\n").unwrap();
    git(&repo, &["add", "Cargo.toml"]);
    git(&repo, &["commit", "-q", "-m", "edit cargo"]);
    git(&repo, &["checkout", "-q", "main"]);

    let mut store = overseer_store::Store::open(&state.join("overseer.db")).unwrap();
    store
        .set_status_with_reason(
            TaskId::new(1),
            TaskStatus::NeedsHuman,
            Some("escalation: Cargo.toml"),
        )
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
        stderr.contains("only tasks in review can be merged"),
        "{stderr}"
    );
    assert!(stderr.contains("needs-human"), "{stderr}");
}

#[test]
fn approve_refuses_when_not_in_needs_human() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");

    overseer(&state).arg("init").assert().success();

    overseer(&state)
        .args(["task", "add", "backlog task"])
        .assert()
        .success();

    let output = overseer(&state)
        .args(["approve", "1"])
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).unwrap();
    assert!(
        stderr.contains("only tasks in needs-human can be approved"),
        "{stderr}"
    );
}

#[test]
fn approve_refuses_when_no_escalation_reason() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");

    overseer(&state).arg("init").assert().success();

    overseer(&state)
        .args(["task", "add", "blocked task"])
        .assert()
        .success();

    let mut store = overseer_store::Store::open(&state.join("overseer.db")).unwrap();
    store
        .set_status_with_reason(
            TaskId::new(1),
            TaskStatus::NeedsHuman,
            Some("quota exhausted"),
        )
        .unwrap();

    let output = overseer(&state)
        .args(["approve", "1"])
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).unwrap();
    assert!(stderr.contains("escalation reason"), "{stderr}");
}

#[test]
fn approve_moves_needs_human_task_to_review_and_allows_merge() {
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
        .args(["task", "add", "sensitive task"])
        .assert()
        .success();

    git(&repo, &["checkout", "-q", "-b", "overseer/task-1"]);
    std::fs::write(repo.join("Cargo.toml"), "# config\n").unwrap();
    git(&repo, &["add", "Cargo.toml"]);
    git(&repo, &["commit", "-q", "-m", "edit cargo"]);
    git(&repo, &["checkout", "-q", "main"]);

    let mut store = overseer_store::Store::open(&state.join("overseer.db")).unwrap();
    store
        .set_status_with_reason(
            TaskId::new(1),
            TaskStatus::NeedsHuman,
            Some("escalation: Cargo.toml"),
        )
        .unwrap();

    // Verify merge refuses before approve
    overseer(&state).args(["merge", "1"]).assert().code(2);

    // Approve the escalation
    let approve_out = overseer(&state)
        .args(["approve", "1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(
        String::from_utf8(approve_out)
            .unwrap()
            .contains("task 1 approved")
    );

    // Verify task status and approval
    let task = overseer_store::Store::open(&state.join("overseer.db"))
        .unwrap()
        .get_task(TaskId::new(1))
        .unwrap();
    assert_eq!(task.status, TaskStatus::Review);
    assert!(task.approved);
    assert!(
        task.reason
            .as_deref()
            .unwrap_or("")
            .contains("escalation: Cargo.toml")
    );

    // Now merge should succeed!
    let output = overseer(&state)
        .args(["merge", "1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let sha = String::from_utf8(output).unwrap();
    assert!(!sha.trim().is_empty());

    // Task is now Done
    let done_task = overseer_store::Store::open(&state.join("overseer.db"))
        .unwrap()
        .get_task(TaskId::new(1))
        .unwrap();
    assert_eq!(done_task.status, TaskStatus::Done);
}

#[test]
fn status_counts_needs_human() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");

    overseer(&state).arg("init").assert().success();

    overseer(&state)
        .args(["task", "add", "task 1"])
        .assert()
        .success();

    let mut store = overseer_store::Store::open(&state.join("overseer.db")).unwrap();
    store
        .set_status_with_reason(
            TaskId::new(1),
            TaskStatus::NeedsHuman,
            Some("escalation: Cargo.toml"),
        )
        .unwrap();

    let output = overseer(&state)
        .arg("status")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();
    assert!(
        stdout
            .lines()
            .any(|l| l.starts_with("needs-human") && l.ends_with('1')),
        "{stdout}"
    );
}
