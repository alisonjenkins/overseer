#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use assert_cmd::Command;

fn overseer(state_dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("overseer").unwrap();
    cmd.arg("--state-dir").arg(state_dir);
    cmd
}

fn stdout_of(state_dir: &Path, args: &[&str]) -> String {
    let output = overseer(state_dir)
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(output).unwrap()
}

fn stderr_of_success(state_dir: &Path, args: &[&str]) -> String {
    let output = overseer(state_dir)
        .args(args)
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    String::from_utf8(output).unwrap()
}

fn stderr_of_failure(state_dir: &Path, args: &[&str]) -> String {
    let output = overseer(state_dir)
        .args(args)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    String::from_utf8(output).unwrap()
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

fn write_program(path: &Path, body: &str) -> PathBuf {
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_path_buf()
}

struct Fixture {
    _root: tempfile::TempDir,
    repo: PathBuf,
    state: PathBuf,
}

fn setup() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Test User"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-q", "-m", "initial commit"]);
    let state = root.path().join("state");
    Fixture {
        _root: root,
        repo,
        state,
    }
}

fn write_config(state_dir: &Path, text: &str) {
    std::fs::write(state_dir.join("overseer.toml"), text).unwrap();
}

#[test]
fn cli_run_triggers_review_and_approves_task() {
    let fx = setup();
    stdout_of(&fx.state, &["init", "--repo", fx.repo.to_str().unwrap()]);

    let worker = write_program(
        &fx._root.path().join("worker"),
        "echo ok > file.txt\necho 'worker done'\n",
    );
    let rev = write_program(
        &fx._root.path().join("reviewer"),
        "echo 'VERDICT: PASS'\necho 'Looks clean'\n",
    );

    write_config(
        &fx.state,
        &format!(
            r#"repo = "{repo}"
default_profile = "worker"

[review]
profiles = ["reviewer"]
max_reworks = 2

[[profiles]]
name = "worker"
backend = "command"
command = ["{worker}"]
permissions = "write-workdir"

[[profiles]]
name = "reviewer"
backend = "command"
command = ["{rev}"]
permissions = "read"
"#,
            repo = fx.repo.display(),
            worker = worker.display(),
            rev = rev.display(),
        ),
    );

    stdout_of(&fx.state, &["task", "add", "my feature"]);
    let run_output = stdout_of(&fx.state, &["run", "1"]);

    assert!(run_output.contains("task: 1 -> review"), "{run_output}");
    assert!(
        run_output.contains("task 1 round 0: PASS (reviewers: reviewer=PASS)"),
        "{run_output}"
    );

    let show_output = stdout_of(&fx.state, &["task", "show", "1"]);
    assert!(show_output.contains("approved: true"), "{show_output}");
    assert!(show_output.contains("reviews:"), "{show_output}");
    assert!(show_output.contains("reviewer"), "{show_output}");
    assert!(show_output.contains("PASS"), "{show_output}");
}

#[test]
fn cli_manual_review_command() {
    let fx = setup();
    stdout_of(&fx.state, &["init", "--repo", fx.repo.to_str().unwrap()]);

    let worker = write_program(
        &fx._root.path().join("worker"),
        "echo ok > file.txt\necho 'worker done'\n",
    );
    let rev = write_program(
        &fx._root.path().join("reviewer"),
        "echo 'VERDICT: PASS'\necho 'Looks clean'\n",
    );

    // Initial config: review disabled
    write_config(
        &fx.state,
        &format!(
            r#"repo = "{repo}"
default_profile = "worker"

[[profiles]]
name = "worker"
backend = "command"
command = ["{worker}"]
permissions = "write-workdir"

[[profiles]]
name = "reviewer"
backend = "command"
command = ["{rev}"]
permissions = "read"
"#,
            repo = fx.repo.display(),
            worker = worker.display(),
            rev = rev.display(),
        ),
    );

    stdout_of(&fx.state, &["task", "add", "my feature"]);
    stdout_of(&fx.state, &["run", "1"]);

    let show1 = stdout_of(&fx.state, &["task", "show", "1"]);
    assert!(show1.contains("approved: false"), "{show1}");

    // Now configure review
    write_config(
        &fx.state,
        &format!(
            r#"repo = "{repo}"
default_profile = "worker"

[review]
profiles = ["reviewer"]
max_reworks = 2

[[profiles]]
name = "worker"
backend = "command"
command = ["{worker}"]
permissions = "write-workdir"

[[profiles]]
name = "reviewer"
backend = "command"
command = ["{rev}"]
permissions = "read"
"#,
            repo = fx.repo.display(),
            worker = worker.display(),
            rev = rev.display(),
        ),
    );

    let review_output = stdout_of(&fx.state, &["review", "1"]);
    assert!(
        review_output.contains("task 1 round 0: PASS (reviewers: reviewer=PASS)"),
        "{review_output}"
    );

    let show2 = stdout_of(&fx.state, &["task", "show", "1"]);
    assert!(show2.contains("approved: true"), "{show2}");
}

#[test]
fn cli_merge_warning_when_unapproved_with_reviews() {
    let fx = setup();
    stdout_of(&fx.state, &["init", "--repo", fx.repo.to_str().unwrap()]);

    let worker = write_program(
        &fx._root.path().join("worker"),
        "echo ok > file.txt\necho 'worker done'\n",
    );
    let rev = write_program(
        &fx._root.path().join("reviewer"),
        "echo 'VERDICT: CHANGES'\necho '- Problem'\n",
    );

    write_config(
        &fx.state,
        &format!(
            r#"repo = "{repo}"
default_profile = "worker"

[review]
profiles = ["reviewer"]
max_reworks = 0

[[profiles]]
name = "worker"
backend = "command"
command = ["{worker}"]
permissions = "write-workdir"

[[profiles]]
name = "reviewer"
backend = "command"
command = ["{rev}"]
permissions = "read"
"#,
            repo = fx.repo.display(),
            worker = worker.display(),
            rev = rev.display(),
        ),
    );

    stdout_of(&fx.state, &["task", "add", "unapproved feature"]);
    // Max reworks is 0 so it moves to needs-human
    stdout_of(&fx.state, &["run", "1"]);

    // Move status back to review to test merge warning from review status
    let mut store = overseer_store::Store::open(&fx.state.join("overseer.db")).unwrap();
    store
        .set_status(
            overseer_core::TaskId::new(1),
            overseer_core::TaskStatus::Review,
        )
        .unwrap();

    let stderr = stderr_of_success(&fx.state, &["merge", "1"]);
    assert!(
        stderr.contains("warning: task 1 has reviews and is not approved"),
        "{stderr}"
    );
}

#[test]
fn cli_review_refusal_when_not_in_review() {
    let fx = setup();
    stdout_of(&fx.state, &["init", "--repo", fx.repo.to_str().unwrap()]);
    stdout_of(&fx.state, &["task", "add", "backlog task"]);

    let stderr = stderr_of_failure(&fx.state, &["review", "1"]);
    assert!(
        stderr.contains("task 1 has status backlog; only tasks in review can be reviewed"),
        "{stderr}"
    );
}

#[test]
fn cli_review_refusal_when_review_disabled() {
    let fx = setup();
    stdout_of(&fx.state, &["init", "--repo", fx.repo.to_str().unwrap()]);

    let worker = write_program(
        &fx._root.path().join("worker"),
        "echo ok > file.txt\necho 'worker done'\n",
    );
    write_config(
        &fx.state,
        &format!(
            r#"repo = "{repo}"
default_profile = "worker"

[[profiles]]
name = "worker"
backend = "command"
command = ["{worker}"]
permissions = "write-workdir"
"#,
            repo = fx.repo.display(),
            worker = worker.display(),
        ),
    );

    stdout_of(&fx.state, &["task", "add", "feature"]);
    stdout_of(&fx.state, &["run", "1"]);

    let stderr = stderr_of_failure(&fx.state, &["review", "1"]);
    assert!(
        stderr.contains("review is disabled: no reviewer profiles configured in overseer.toml"),
        "{stderr}"
    );
}
