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
fn skills_add_list_show_use_version_round_trip() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    overseer(&state).arg("init").assert().success();

    let v1_file = root.path().join("v1.md");
    std::fs::write(&v1_file, "Write tests first.\n").unwrap();

    let add_out = stdout_of(&state, &["skills", "add", "tdd", v1_file.to_str().unwrap()]);
    assert!(add_out.contains("tdd"));
    assert!(add_out.contains("v1"));

    let list_out = stdout_of(&state, &["skills", "list"]);
    assert!(list_out.contains("tdd"));
    assert!(list_out.contains("v1"));

    let show_out = stdout_of(&state, &["skills", "show", "tdd"]);
    assert_eq!(show_out.trim(), "Write tests first.");

    let v2_file = root.path().join("v2.md");
    std::fs::write(&v2_file, "Write tests first, always.\n").unwrap();

    let add_v2_out = stdout_of(&state, &["skills", "add", "tdd", v2_file.to_str().unwrap()]);
    assert!(add_v2_out.contains("v2"));

    let list_v2_out = stdout_of(&state, &["skills", "list"]);
    assert!(list_v2_out.contains("v2"));

    let show_curr_out = stdout_of(&state, &["skills", "show", "tdd"]);
    assert_eq!(show_curr_out.trim(), "Write tests first, always.");

    let show_v1_out = stdout_of(&state, &["skills", "show", "tdd", "--version", "1"]);
    assert_eq!(show_v1_out.trim(), "Write tests first.");

    let use_out = stdout_of(&state, &["skills", "use-version", "tdd", "1"]);
    assert!(use_out.contains("v1"));

    let show_rollback_out = stdout_of(&state, &["skills", "show", "tdd"]);
    assert_eq!(show_rollback_out.trim(), "Write tests first.");

    let list_rollback_out = stdout_of(&state, &["skills", "list"]);
    assert!(list_rollback_out.contains("v1"));
}

#[test]
fn skills_add_invalid_name_fails() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    overseer(&state).arg("init").assert().success();

    let file = root.path().join("test.md");
    std::fs::write(&file, "content").unwrap();

    let err = stderr_of_failure(
        &state,
        &["skills", "add", "Invalid_Name", file.to_str().unwrap()],
    );
    assert!(err.contains("invalid skill name"), "{err}");
}

#[test]
fn skills_show_unknown_fails() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    overseer(&state).arg("init").assert().success();

    let err = stderr_of_failure(&state, &["skills", "show", "nonexistent"]);
    assert!(
        err.contains("not found") || err.contains("no such"),
        "{err}"
    );
}

#[test]
fn skills_use_unknown_version_fails() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    overseer(&state).arg("init").assert().success();

    let file = root.path().join("test.md");
    std::fs::write(&file, "content").unwrap();
    stdout_of(&state, &["skills", "add", "tdd", file.to_str().unwrap()]);

    let err = stderr_of_failure(&state, &["skills", "use-version", "tdd", "99"]);
    assert!(err.contains("version 99"), "{err}");
}
