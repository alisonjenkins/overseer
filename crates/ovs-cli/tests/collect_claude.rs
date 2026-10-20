#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;

const MAIN: &str = concat!(
    r#"{"type":"user","timestamp":"2026-10-20T05:00:00.000Z","sessionId":"s1","cwd":"/tmp/proj","gitBranch":"main","message":{"role":"user","content":"fix the parser"}}"#,
    "\n",
    r#"{"type":"assistant","timestamp":"2026-10-20T05:00:01.000Z","sessionId":"s1","cwd":"/tmp/proj","message":{"id":"msg_1","model":"claude-opus-5-5","content":[{"type":"text","text":"Running tests"}],"usage":{"input_tokens":10,"output_tokens":5}}}"#,
    "\n",
    r#"{"type":"assistant","timestamp":"2026-10-20T05:00:01.000Z","sessionId":"s1","cwd":"/tmp/proj","message":{"id":"msg_1","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo test"}}],"usage":{"input_tokens":10,"output_tokens":5}}}"#,
    "\n",
    r#"{"type":"user","timestamp":"2026-10-20T05:00:02.000Z","sessionId":"s1","cwd":"/tmp/proj","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","is_error":false,"content":"ok"}]}}"#,
    "\n",
    r#"{"type":"system","subtype":"turn_duration","timestamp":"2026-10-20T05:00:03.000Z","sessionId":"s1","cwd":"/tmp/proj"}"#,
    "\n",
);

const SUB: &str = concat!(
    r#"{"type":"user","timestamp":"2026-10-20T05:00:01.500Z","sessionId":"s1","agentId":"a1","isSidechain":true,"cwd":"/tmp/proj","message":{"role":"user","content":"look up the schema"}}"#,
    "\n",
);

fn run(root: &Path, cursor: &Path) -> Vec<Value> {
    let output = Command::cargo_bin("ovs")
        .unwrap()
        .args(["collect", "claude", "--once", "--host", "test-host"])
        .arg("--root")
        .arg(root)
        .arg("--cursor")
        .arg(cursor)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn collects_main_and_subagent_transcripts_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("claude");
    let project = root.join("projects/-tmp-proj");
    fs::create_dir_all(project.join("s1/subagents")).unwrap();
    fs::write(project.join("s1.jsonl"), MAIN).unwrap();
    fs::write(project.join("s1/subagents/agent-a1.jsonl"), SUB).unwrap();
    let cursor = dir.path().join("state/cursor.json");

    let events = run(&root, &cursor);
    let seen: Vec<(&str, &str)> = events
        .iter()
        .map(|e| (e["session"].as_str().unwrap(), e["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(
        seen,
        [
            ("claude-code:s1", "session.started"),
            ("claude-code:s1", "prompt"),
            ("claude-code:s1", "message"),
            ("claude-code:s1", "usage"),
            ("claude-code:s1", "tool.called"),
            ("claude-code:s1", "tool.result"),
            ("claude-code:s1", "session.status"),
            ("claude-code:agent-a1", "session.started"),
            ("claude-code:agent-a1", "prompt"),
        ]
    );
    assert_eq!(events[8]["parent"], "claude-code:s1");
    assert!(
        events
            .iter()
            .all(|e| e["host"] == "test-host" && e["v"] == 1)
    );

    assert!(run(&root, &cursor).is_empty());
}

#[test]
fn rejects_a_bad_since() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("ovs")
        .unwrap()
        .args(["collect", "claude", "--once", "--since", "yesterday"])
        .arg("--root")
        .arg(dir.path())
        .arg("--cursor")
        .arg(dir.path().join("c.json"))
        .assert()
        .failure()
        .stderr(predicates::str::contains("--since is not an RFC 3339 time"));
}
