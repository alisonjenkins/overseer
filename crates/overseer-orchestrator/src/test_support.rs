//! Helpers for tests that need a real git repository. Every repository is
//! created inside a temp directory and configured locally, so tests never read
//! or change the user's git configuration.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

/// Run git synchronously in `dir`, asserting success, and return stdout.
pub fn git_sync(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
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

/// Initialise `dir` as a repository on `main` with one commit containing
/// `README.md`. `dir` must already exist.
pub fn init_repo(dir: &Path) {
    git_sync(dir, &["init", "-q", "-b", "main"]);
    git_sync(dir, &["config", "user.name", "Overseer Test"]);
    git_sync(dir, &["config", "user.email", "test@example.invalid"]);
    git_sync(dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("README.md"), "base\n").unwrap();
    git_sync(dir, &["add", "-A"]);
    git_sync(dir, &["commit", "-q", "-m", "initial"]);
}
