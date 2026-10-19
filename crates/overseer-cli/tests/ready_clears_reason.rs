#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use assert_cmd::Command;
use overseer_core::{NewTask, TaskStatus};
use overseer_store::Store;

fn overseer(state_dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("overseer").unwrap();
    cmd.env("OVERSEER_STATE_DIR", state_dir);
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
fn ready_cli_clears_block_reason() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");

    overseer(&state).arg("init").assert().success();

    let mut store = Store::open(&state.join("overseer.db")).unwrap();
    let task = store
        .add_task(NewTask {
            title: "blocked task".to_owned(),
            ..NewTask::default()
        })
        .unwrap();
    store
        .set_status_with_reason(
            task.id,
            TaskStatus::Blocked,
            Some("split into 2 subtasks: 1, 2"),
        )
        .unwrap();
    drop(store);

    let id = task.id.to_string();
    let out = stdout_of(&state, &["task", "ready", &id]);
    assert!(out.contains(&format!("task {id} ready")), "{out}");

    let shown = stdout_of(&state, &["task", "show", &id]);
    assert!(shown.contains("status: ready"), "{shown}");
    assert!(!shown.contains("reason:"), "{shown}");

    let store = Store::open(&state.join("overseer.db")).unwrap();
    let fetched = store.get_task(task.id).unwrap();
    assert_eq!(fetched.status, TaskStatus::Ready);
    assert_eq!(fetched.reason, None);
}
