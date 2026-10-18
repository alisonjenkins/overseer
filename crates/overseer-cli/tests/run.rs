#![allow(clippy::unwrap_used, clippy::expect_used)]

//! End-to-end checks of `init`, `task add`, `run` and `runs`. The model is a
//! wiremock server on 127.0.0.1; the repository and state directory are
//! temp directories, so nothing outside the test is touched.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use assert_cmd::Command;
use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const STARTER_BASE_URL: &str = "http://127.0.0.1:8080/v1";

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

/// A repository with one commit. Identity and signing are local to it.
fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "Overseer Test"]);
    git(dir, &["config", "user.email", "test@example.invalid"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("README.md"), "base\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "initial"]);
}

/// Point the starter profile at `base_url` instead of the real local port.
fn point_config_at(state_dir: &Path, base_url: &str) {
    let config = state_dir.join("overseer.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains(STARTER_BASE_URL), "{text}");
    std::fs::write(&config, text.replace(STARTER_BASE_URL, base_url)).unwrap();
}

/// A localhost port with nothing listening on it.
fn closed_port_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}/v1")
}

struct Setup {
    _root: tempfile::TempDir,
    repo: PathBuf,
    state: PathBuf,
}

fn setup() -> Setup {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo(&repo);
    let state = root.path().join("state");
    Setup {
        _root: root,
        repo,
        state,
    }
}

#[tokio::test]
async fn init_add_run_show_reaches_review_with_transcript() {
    let fx = setup();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({
            "messages": [
                {"role": "system", "content": "You are a careful senior engineer."},
                {"role": "user", "content": "Task 1: write parser\n\nhandle quotes"}
            ]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"role": "assistant", "content": "parser sketched"}}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 7}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let init = stdout_of(&fx.state, &["init", "--repo", fx.repo.to_str().unwrap()]);
    assert!(init.contains("wrote starter config"), "{init}");
    point_config_at(&fx.state, &format!("{}/v1", server.uri()));

    let id = stdout_of(
        &fx.state,
        &[
            "task",
            "add",
            "write parser",
            "--body",
            "handle quotes",
            "--profile",
            "local",
        ],
    );
    assert_eq!(id.trim(), "1");

    let run = stdout_of(&fx.state, &["run", "1"]);
    assert!(run.contains("run: 1"), "{run}");
    assert!(run.contains("task: 1 -> review"), "{run}");
    assert!(run.contains("outcome: succeeded"), "{run}");
    assert!(run.contains("branch: overseer/task-1"), "{run}");
    assert!(run.contains("tokens: input 12, output 7"), "{run}");

    let shown = stdout_of(&fx.state, &["task", "show", "1"]);
    assert!(shown.contains("status: review"), "{shown}");

    let transcript = fx.state.join("runs").join("1.md");
    assert!(transcript.is_file(), "missing {}", transcript.display());
    let text = std::fs::read_to_string(&transcript).unwrap();
    assert!(text.contains("parser sketched"), "{text}");

    let runs = stdout_of(&fx.state, &["runs", "1"]);
    assert!(runs.contains("succeeded"), "{runs}");
    assert!(runs.contains("overseer/task-1"), "{runs}");

    let branches = git(&fx.repo, &["branch", "--list", "overseer/task-1"]);
    assert!(branches.contains("overseer/task-1"), "{branches}");
}

#[tokio::test]
async fn unreachable_backend_blocks_the_task_and_records_why() {
    let fx = setup();
    stdout_of(&fx.state, &["init", "--repo", fx.repo.to_str().unwrap()]);
    point_config_at(&fx.state, &closed_port_url());
    stdout_of(&fx.state, &["task", "add", "write parser"]);

    let run = stdout_of(&fx.state, &["run", "1"]);
    assert!(run.contains("task: 1 -> blocked"), "{run}");
    assert!(run.contains("outcome: failed (unreachable)"), "{run}");

    let shown = stdout_of(&fx.state, &["task", "show", "1"]);
    assert!(shown.contains("status: blocked"), "{shown}");
    let runs = stdout_of(&fx.state, &["runs", "1"]);
    assert!(runs.contains("unreachable"), "{runs}");
}

#[tokio::test]
async fn init_never_overwrites_an_existing_config() {
    let fx = setup();
    stdout_of(&fx.state, &["init", "--repo", fx.repo.to_str().unwrap()]);
    let config = fx.state.join("overseer.toml");
    std::fs::write(&config, "# hand edited\n").unwrap();

    let again = stdout_of(&fx.state, &["init", "--repo", fx.repo.to_str().unwrap()]);

    assert!(again.contains("kept existing config"), "{again}");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "# hand edited\n");
}

#[tokio::test]
async fn task_add_rejects_a_profile_missing_from_the_config() {
    let fx = setup();
    stdout_of(&fx.state, &["init", "--repo", fx.repo.to_str().unwrap()]);

    let stderr = stderr_of_failure(&fx.state, &["task", "add", "x", "--profile", "ghost"]);
    assert!(
        stderr.contains("profile \"ghost\" is not defined"),
        "{stderr}"
    );
}

#[tokio::test]
async fn run_before_init_says_how_to_start() {
    let fx = setup();
    let stderr = stderr_of_failure(&fx.state, &["run", "1"]);
    assert!(stderr.contains("run `overseer init` first"), "{stderr}");
}

#[tokio::test]
async fn run_of_unknown_task_fails_with_the_store_reason() {
    let fx = setup();
    stdout_of(&fx.state, &["init", "--repo", fx.repo.to_str().unwrap()]);

    let stderr = stderr_of_failure(&fx.state, &["run", "42"]);
    assert!(stderr.contains("no task with id 42"), "{stderr}");
}
