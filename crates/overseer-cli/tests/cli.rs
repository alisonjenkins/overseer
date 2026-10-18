#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

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

#[test]
fn init_add_list_show_round_trip() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");

    overseer(&state).arg("init").assert().success();
    assert!(state.join("overseer.db").is_file());

    let first = stdout_of(
        &state,
        &[
            "task",
            "add",
            "write parser",
            "--body",
            "handle quotes",
            "--priority",
            "2",
        ],
    );
    assert_eq!(first.trim(), "1");

    let second = stdout_of(
        &state,
        &[
            "task",
            "add",
            "write tests",
            "--priority",
            "1",
            "--depends-on",
            "1",
        ],
    );
    assert_eq!(second.trim(), "2");

    let list = stdout_of(&state, &["task", "list"]);
    let tests_row = list.find("write tests").expect("list shows second task");
    let parser_row = list.find("write parser").expect("list shows first task");
    assert!(
        tests_row < parser_row,
        "lower priority number must be listed first:\n{list}"
    );

    let backlog = stdout_of(&state, &["task", "list", "--status", "backlog"]);
    assert!(backlog.contains("write parser") && backlog.contains("write tests"));

    let done = stdout_of(&state, &["task", "list", "--status", "done"]);
    assert!(!done.contains("write parser"), "done list was:\n{done}");

    let shown = stdout_of(&state, &["task", "show", "2"]);
    assert!(shown.contains("title: write tests"), "{shown}");
    assert!(shown.contains("status: backlog"), "{shown}");
    assert!(shown.contains("priority: 1"), "{shown}");
    assert!(shown.contains("depends_on: 1"), "{shown}");

    let shown_first = stdout_of(&state, &["task", "show", "1"]);
    assert!(shown_first.contains("handle quotes"), "{shown_first}");
}

#[test]
fn show_unknown_task_fails_with_message() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    overseer(&state).arg("init").assert().success();

    let stderr = stderr_of_failure(&state, &["task", "show", "999"]);
    assert!(stderr.contains("no task with id 999"), "{stderr}");
}

#[test]
fn task_commands_require_init() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("never-initialised");

    let stderr = stderr_of_failure(&state, &["task", "list"]);
    assert!(stderr.contains("run `overseer init` first"), "{stderr}");
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

#[test]
fn task_add_with_ready_flag_creates_ready_task() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    overseer(&state).arg("init").assert().success();

    let id = stdout_of(&state, &["task", "add", "fast task", "--ready"]);
    assert_eq!(id.trim(), "1");

    let shown = stdout_of(&state, &["task", "show", "1"]);
    assert!(shown.contains("status: ready"), "{shown}");
}

#[test]
fn task_ready_moves_backlog_and_blocked_to_ready() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    overseer(&state).arg("init").assert().success();

    let id = stdout_of(&state, &["task", "add", "my task"]);
    assert_eq!(id.trim(), "1");
    let shown = stdout_of(&state, &["task", "show", "1"]);
    assert!(shown.contains("status: backlog"), "{shown}");

    let out = stdout_of(&state, &["task", "ready", "1"]);
    assert!(out.contains("task 1 ready"), "{out}");

    let shown = stdout_of(&state, &["task", "show", "1"]);
    assert!(shown.contains("status: ready"), "{shown}");

    // Marking already ready task fails
    let err = stderr_of_failure(&state, &["task", "ready", "1"]);
    assert!(err.contains("cannot be marked ready"), "{err}");
}

#[test]
fn config_starter_prints_template_to_stdout_without_state_dir() {
    let mut cmd = Command::cargo_bin("overseer").unwrap();
    let output = cmd
        .args(["config", "starter"])
        .env_remove("OVERSEER_STATE_DIR")
        .env_remove("XDG_STATE_HOME")
        .env_remove("HOME")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let out = String::from_utf8(output).unwrap();
    assert!(out.contains("default_profile = \"local\""), "{out}");
    assert!(out.contains("[[profiles]]"), "{out}");
    assert!(out.contains("name = \"local\""), "{out}");
    assert!(out.contains("name = \"claude\""), "{out}");
    assert!(out.contains("name = \"gemini\""), "{out}");
}

#[test]
fn config_starter_uses_explicit_repo() {
    let mut cmd = Command::cargo_bin("overseer").unwrap();
    let output = cmd
        .args(["config", "starter", "--repo", "/custom/test/repo"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let out = String::from_utf8(output).unwrap();
    assert!(out.contains("repo = \"/custom/test/repo\""), "{out}");
}

#[test]
fn config_path_prints_config_file_path() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");

    let out = stdout_of(&state, &["config", "path"]);
    assert_eq!(
        out.trim(),
        state.join("overseer.toml").display().to_string()
    );
}
