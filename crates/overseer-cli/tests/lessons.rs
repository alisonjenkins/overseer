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

fn init_git_repo(path: &Path) {
    let run = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(path)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
    };
    run(&["init"]);
    run(&["config", "user.name", "Tester"]);
    run(&["config", "user.email", "tester@example.com"]);
    run(&["config", "commit.gpgsign", "false"]);
    std::fs::write(path.join("README.md"), "# Initial\n").unwrap();
    run(&["add", "README.md"]);
    run(&["commit", "-m", "init"]);
}

#[test]
fn lessons_list_show_apply_reject_round_trip() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    let state = root.path().join("state");
    overseer(&state).arg("init").assert().success();

    // Write config with learner profile
    let config_content = format!(
        "repo = \"{}\"\n\
         default_profile = \"runner\"\n\n\
         [learning]\n\
         profile = \"learner\"\n\
         skill = \"general\"\n\
         min_signal = true\n\n\
         [[profiles]]\n\
         name = \"runner\"\n\
         backend = \"command\"\n\
         command = [\"sh\", \"-c\", \"exit 1\"]\n\
         permissions = \"write-workdir\"\n\
         timeout_secs = 10\n\n\
         [[profiles]]\n\
         name = \"learner\"\n\
         backend = \"command\"\n\
         command = [\"sh\", \"-c\", \"echo 'Rule 1: Always test first.'\"]\n\
         permissions = \"read\"\n\
         timeout_secs = 10\n",
        repo.display()
    );
    std::fs::write(state.join("overseer.toml"), config_content).unwrap();

    // Add general v1 skill
    let v1_file = root.path().join("v1.md");
    std::fs::write(&v1_file, "Rule 0: Start here.\n").unwrap();
    stdout_of(
        &state,
        &["skills", "add", "general", v1_file.to_str().unwrap()],
    );

    // Add and run task
    let task_id = stdout_of(&state, &["task", "add", "failing task", "--ready"]);
    let id_num = task_id.trim();

    // Run the task - it will fail and block (exit 2)
    overseer(&state).args(["run", id_num]).assert().code(2);

    // Run manual retro
    let retro_out = stdout_of(&state, &["retro", id_num]);
    assert!(retro_out.contains("lesson 1 proposed for skill general"));

    // List lessons
    let list_out = stdout_of(&state, &["lessons", "list"]);
    assert!(list_out.contains("general"));
    assert!(list_out.contains("proposed"));
    assert!(list_out.contains("v1"));

    // Show lesson diff
    let show_out = stdout_of(&state, &["lessons", "show", "1"]);
    assert!(show_out.contains("+Rule 1: Always test first."));

    // Apply lesson
    let apply_out = stdout_of(&state, &["lessons", "apply", "1"]);
    assert!(apply_out.contains("applied lesson 1"));
    assert!(apply_out.contains("v2"));

    // Check skill was updated to v2
    let skill_show = stdout_of(&state, &["skills", "show", "general"]);
    assert_eq!(skill_show.trim(), "Rule 1: Always test first.");

    // Check list filters
    let applied_list = stdout_of(&state, &["lessons", "list", "--status", "applied"]);
    assert!(applied_list.contains("applied"));

    let proposed_list = stdout_of(&state, &["lessons", "list", "--status", "proposed"]);
    assert!(!proposed_list.contains("general"));
}

#[test]
fn lessons_apply_refuses_when_base_version_changed() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    let state = root.path().join("state");
    overseer(&state).arg("init").assert().success();

    let config_content = format!(
        "repo = \"{}\"\n\
         default_profile = \"runner\"\n\n\
         [learning]\n\
         profile = \"learner\"\n\
         skill = \"general\"\n\
         min_signal = true\n\n\
         [[profiles]]\n\
         name = \"runner\"\n\
         backend = \"command\"\n\
         command = [\"sh\", \"-c\", \"exit 1\"]\n\
         permissions = \"write-workdir\"\n\
         timeout_secs = 10\n\n\
         [[profiles]]\n\
         name = \"learner\"\n\
         backend = \"command\"\n\
         command = [\"sh\", \"-c\", \"echo 'Proposed update.'\"]\n\
         permissions = \"read\"\n\
         timeout_secs = 10\n",
        repo.display()
    );
    std::fs::write(state.join("overseer.toml"), config_content).unwrap();

    let v1_file = root.path().join("v1.md");
    std::fs::write(&v1_file, "V1.\n").unwrap();
    stdout_of(
        &state,
        &["skills", "add", "general", v1_file.to_str().unwrap()],
    );

    let task_id = stdout_of(&state, &["task", "add", "failing task", "--ready"]);
    let id_num = task_id.trim();
    overseer(&state).args(["run", id_num]).assert().code(2);
    stdout_of(&state, &["retro", id_num]);

    // Now update skill out from under the lesson to v2
    let v2_file = root.path().join("v2.md");
    std::fs::write(&v2_file, "V2 manual edit.\n").unwrap();
    stdout_of(
        &state,
        &["skills", "add", "general", v2_file.to_str().unwrap()],
    );

    // Apply should refuse because current version changed
    let err = stderr_of_failure(&state, &["lessons", "apply", "1"]);
    assert!(
        err.contains("current version is v2")
            || err.contains("refusing")
            || err.contains("changed"),
        "{err}"
    );
}

#[test]
fn lessons_reject_marks_rejected() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    let state = root.path().join("state");
    overseer(&state).arg("init").assert().success();

    let config_content = format!(
        "repo = \"{}\"\n\
         default_profile = \"runner\"\n\n\
         [learning]\n\
         profile = \"learner\"\n\
         skill = \"general\"\n\
         min_signal = true\n\n\
         [[profiles]]\n\
         name = \"runner\"\n\
         backend = \"command\"\n\
         command = [\"sh\", \"-c\", \"exit 1\"]\n\
         permissions = \"write-workdir\"\n\
         timeout_secs = 10\n\n\
         [[profiles]]\n\
         name = \"learner\"\n\
         backend = \"command\"\n\
         command = [\"sh\", \"-c\", \"echo 'Proposed update.'\"]\n\
         permissions = \"read\"\n\
         timeout_secs = 10\n",
        repo.display()
    );
    std::fs::write(state.join("overseer.toml"), config_content).unwrap();

    let task_id = stdout_of(&state, &["task", "add", "failing task", "--ready"]);
    let id_num = task_id.trim();
    overseer(&state).args(["run", id_num]).assert().code(2);
    stdout_of(&state, &["retro", id_num]);

    let reject_out = stdout_of(&state, &["lessons", "reject", "1"]);
    assert!(reject_out.contains("lesson 1 rejected"));

    let list_out = stdout_of(&state, &["lessons", "list", "--status", "rejected"]);
    assert!(list_out.contains("rejected"));
}
