#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn overseer(state_dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("overseer").unwrap();
    cmd.arg("--state-dir").arg(state_dir);
    cmd
}

fn write_script(path: &Path, body: &str) -> PathBuf {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_path_buf()
}

fn closed_port_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}/v1")
}

#[tokio::test]
async fn doctor_usable_default_plan_exits_0() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{"id": "local-model"}]
        })))
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let state_dir = temp.path().join("state");
    let script = write_script(&temp.path().join("fake_tool.sh"), "printf ok");

    overseer(&state_dir).arg("init").assert().success();

    let config_content = format!(
        r#"repo = "{}"
default_profile = "local"

[[profiles]]
name = "local"
backend = "openai"
base_url = "{}/v1"
model = "local"

[[profiles]]
name = "cmd"
backend = "command"
command = ["{}"]
"#,
        temp.path().display(),
        server.uri(),
        script.display()
    );
    std::fs::write(state_dir.join("overseer.toml"), config_content).unwrap();

    let output = overseer(&state_dir)
        .arg("doctor")
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();

    let out = String::from_utf8(output).unwrap();
    assert!(out.contains("PROFILE"), "{out}");
    assert!(out.contains("BACKEND"), "{out}");
    assert!(out.contains("HEALTH"), "{out}");
    assert!(out.contains("METERED"), "{out}");
    assert!(out.contains("DETAIL"), "{out}");

    assert!(out.contains("local"), "{out}");
    assert!(out.contains("openai"), "{out}");
    assert!(out.contains("local-model"), "{out}");

    assert!(out.contains("cmd"), "{out}");
    assert!(out.contains("command"), "{out}");

    assert!(out.contains("default plan: local"), "{out}");
    assert!(out.contains("credits-low: off"), "{out}");
    assert!(out.contains("verdict: usable"), "{out}");
}

#[test]
fn doctor_unusable_default_plan_exits_2() {
    let temp = tempfile::tempdir().unwrap();
    let state_dir = temp.path().join("state");
    overseer(&state_dir).arg("init").assert().success();

    let config_content = format!(
        r#"repo = "{}"
default_profile = "broken"

[[profiles]]
name = "broken"
backend = "openai"
base_url = "{}"
model = "broken"
"#,
        temp.path().display(),
        closed_port_url()
    );
    std::fs::write(state_dir.join("overseer.toml"), config_content).unwrap();

    let output = overseer(&state_dir)
        .arg("doctor")
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();

    let out = String::from_utf8(output).unwrap();
    assert!(out.contains("broken"), "{out}");
    assert!(out.contains("unhealthy"), "{out}");
    assert!(out.contains("unreachable"), "{out}");
    assert!(out.contains("default plan: broken (unhealthy)"), "{out}");
    assert!(out.contains("verdict: NOTHING USABLE"), "{out}");
}

#[tokio::test]
async fn doctor_deep_flag_runs_backend() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "local-model"}]
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"role": "assistant", "content": "ok"}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2}
        })))
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let state_dir = temp.path().join("state");

    overseer(&state_dir).arg("init").assert().success();

    let config_content = format!(
        r#"repo = "{}"
default_profile = "local"

[[profiles]]
name = "local"
backend = "openai"
base_url = "{}/v1"
model = "local"
"#,
        temp.path().display(),
        server.uri()
    );
    std::fs::write(state_dir.join("overseer.toml"), config_content).unwrap();

    let output = overseer(&state_dir)
        .args(["doctor", "--deep"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();

    let out = String::from_utf8(output).unwrap();
    assert!(out.contains("local"), "{out}");
    assert!(out.contains("ok"), "{out}");
    assert!(out.contains("verdict: usable"), "{out}");
}

#[test]
fn doctor_credits_low_mode_annotates_plan() {
    let temp = tempfile::tempdir().unwrap();
    let state_dir = temp.path().join("state");
    let script = write_script(&temp.path().join("local.sh"), "printf ok");

    overseer(&state_dir).arg("init").assert().success();

    // Turn credits-low on
    overseer(&state_dir)
        .args(["mode", "credits-low", "on"])
        .assert()
        .success();

    let config_content = format!(
        r#"repo = "{}"
default_profile = "metered"

[[profiles]]
name = "metered"
backend = "command"
command = ["{}"]
metered = true
fallback = ["unmetered"]

[[profiles]]
name = "unmetered"
backend = "command"
command = ["{}"]
metered = false
"#,
        temp.path().display(),
        script.display(),
        script.display()
    );
    std::fs::write(state_dir.join("overseer.toml"), config_content).unwrap();

    let output = overseer(&state_dir)
        .arg("doctor")
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();

    let out = String::from_utf8(output).unwrap();
    assert!(out.contains("credits-low: on"), "{out}");
    // Metered was dropped from default plan
    assert!(out.contains("default plan: unmetered"), "{out}");
    assert!(out.contains("verdict: usable"), "{out}");
}
