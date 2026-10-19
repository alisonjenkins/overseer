//! Automated landing of reviewed tasks via rebase, test command, and pull requests.

use std::path::Path;

use overseer_core::{Config, TaskId, TaskStatus, Verdict};
use overseer_store::{Store, StoreError};
use tokio::process::Command;

use crate::review::overall_verdict;
use crate::worktree::{branch_name, worktree_path};

#[derive(Debug, thiserror::Error)]
pub enum LandingError {
    #[error("{0}")]
    Refusal(String),
    #[error("test command `{command}` failed:\n{output}")]
    TestFailed { command: String, output: String },
    #[error("failed to start `git {command}`")]
    SpawnGit {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`git {command}` failed: {stderr}")]
    GitFailed { command: String, stderr: String },
    #[error("failed to start `gh {command}`")]
    SpawnGh {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`gh {command}` failed: {stderr}")]
    GhFailed { command: String, stderr: String },
    #[error("failed to start test command `{command}`")]
    SpawnTestCommand {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("store error: {0}")]
    Store(#[from] StoreError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandOutcome {
    PrOpened { url: String },
    Merged { commit: String },
}

async fn run_cmd(
    prog: &str,
    dir: &Path,
    args: &[&str],
) -> Result<std::process::Output, std::io::Error> {
    Command::new(prog)
        .current_dir(dir)
        .args(args)
        .output()
        .await
}

async fn gh(dir: &Path, args: &[&str]) -> Result<String, LandingError> {
    let command = args.join(" ");
    let out = run_cmd("gh", dir, args)
        .await
        .map_err(|source| LandingError::SpawnGh {
            command: command.clone(),
            source,
        })?;
    if !out.status.success() {
        return Err(LandingError::GhFailed {
            command,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

async fn git(dir: &Path, args: &[&str]) -> Result<String, LandingError> {
    let command = args.join(" ");
    let out = run_cmd("git", dir, args)
        .await
        .map_err(|source| LandingError::SpawnGit {
            command: command.clone(),
            source,
        })?;
    if !out.status.success() {
        return Err(LandingError::GitFailed {
            command,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn last_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines
        .iter()
        .skip(lines.len().saturating_sub(count))
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
}

async fn find_base_commit(
    repo: &Path,
    branch: &str,
    base_branch: &str,
) -> Result<String, LandingError> {
    for target in [base_branch, "HEAD"] {
        if let Ok(mb) = git(repo, &["merge-base", target, branch]).await {
            let t = mb.trim();
            if !t.is_empty() {
                return Ok(t.to_owned());
            }
        }
    }
    if let Ok(s) = git(
        repo,
        &[
            "config",
            "--get",
            &format!("branch.{branch}.overseer-base-commit"),
        ],
    )
    .await
    {
        let t = s.trim();
        if !t.is_empty() {
            return Ok(t.to_owned());
        }
    }
    Err(LandingError::Refusal(format!(
        "could not find merge base of {branch} and {base_branch}; refusing to land without checking for fixup commits"
    )))
}

pub async fn land(
    store: &mut Store,
    config: &Config,
    state_dir: &Path,
    task_id: TaskId,
) -> Result<LandOutcome, LandingError> {
    if config.landing.auto_merge && config.landing.test_command.is_none() {
        return Err(LandingError::Refusal(
            "test_command is required for auto-merge".to_owned(),
        ));
    }

    let task = store.get_task(task_id)?;
    if task.status != TaskStatus::Review {
        return Err(LandingError::Refusal(format!(
            "task {task_id} has status {}; only tasks in review can be landed",
            task.status
        )));
    }

    if !task.approved {
        let reviews = store.list_reviews(task_id)?;
        let latest_round = reviews.iter().map(|r| r.round).max();
        let latest_verdict = latest_round.and_then(|rnd| {
            let verdicts: Vec<Verdict> = reviews
                .iter()
                .filter(|r| r.round == rnd)
                .map(|r| r.verdict)
                .collect();
            (!verdicts.is_empty()).then(|| overall_verdict(&verdicts))
        });
        match latest_verdict {
            Some(Verdict::Pass) => {}
            Some(other) => {
                return Err(LandingError::Refusal(format!(
                    "task {task_id} latest review round has verdict {other}; only tasks with verdict PASS or approved tasks can be landed"
                )));
            }
            None => {
                return Err(LandingError::Refusal(format!(
                    "task {task_id} has no review records and is not approved"
                )));
            }
        }
    }

    let branch = branch_name(task_id);
    let wt_path = worktree_path(state_dir, task_id);
    if !wt_path.exists() {
        return Err(LandingError::Refusal(format!(
            "worktree for task {task_id} does not exist at {}",
            wt_path.display()
        )));
    }

    let base_commit = find_base_commit(&config.repo, &branch, &config.landing.base).await?;
    let range = format!("{base_commit}..{branch}");
    let log_output = git(&config.repo, &["log", &range, "--format=%s"]).await?;
    for line in log_output.lines() {
        let s = line.trim();
        if s.starts_with("fixup!") || s.starts_with("squash!") || s.starts_with("amend!") {
            return Err(LandingError::Refusal(format!(
                "task branch {branch} contains disallowed commit {s:?}; squash or rebase before landing"
            )));
        }
    }

    let (remote, base) = (&config.landing.remote, &config.landing.base);
    let mut attempt = 0usize;

    loop {
        git(&wt_path, &["fetch", remote]).await?;
        let rebase_target = format!("{remote}/{base}");
        let rebase_res = run_cmd("git", &wt_path, &["rebase", &rebase_target])
            .await
            .map_err(|source| LandingError::SpawnGit {
                command: format!("rebase {rebase_target}"),
                source,
            })?;

        if !rebase_res.status.success() {
            let diff_out = git(&wt_path, &["diff", "--name-only", "--diff-filter=U"])
                .await
                .unwrap_or_default();
            let mut files: Vec<String> = diff_out
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            if files.is_empty() {
                for line in String::from_utf8_lossy(&rebase_res.stdout)
                    .lines()
                    .chain(String::from_utf8_lossy(&rebase_res.stderr).lines())
                {
                    if let Some(rest) = line.strip_prefix("CONFLICT (content): Merge conflict in ")
                    {
                        let p = rest.trim();
                        if !p.is_empty() && !files.contains(&p.to_owned()) {
                            files.push(p.to_owned());
                        }
                    }
                }
            }
            let _ = run_cmd("git", &wt_path, &["rebase", "--abort"]).await;
            if files.is_empty() {
                files.push("unspecified files".to_owned());
            }
            return Err(LandingError::Refusal(format!(
                "rebase conflict in {}",
                files.join(", ")
            )));
        }

        if let Some(ref test_cmd) = config.landing.test_command {
            let test_res = Command::new("sh")
                .arg("-c")
                .arg(test_cmd)
                .current_dir(&wt_path)
                .output()
                .await
                .map_err(|source| LandingError::SpawnTestCommand {
                    command: test_cmd.clone(),
                    source,
                })?;
            if !test_res.status.success() {
                let combined = format!(
                    "{}{}",
                    String::from_utf8_lossy(&test_res.stdout),
                    String::from_utf8_lossy(&test_res.stderr)
                );
                return Err(LandingError::TestFailed {
                    command: test_cmd.clone(),
                    output: last_lines(&combined, 40),
                });
            }
        }

        git(&wt_path, &["push", "--force-with-lease", remote, &branch]).await?;

        let list_out = gh(
            &wt_path,
            &["pr", "list", "--head", &branch, "--json", "url"],
        )
        .await?;
        let existing_url = serde_json::from_str::<serde_json::Value>(&list_out)
            .ok()
            .and_then(|val| {
                val.as_array()?
                    .first()?
                    .get("url")?
                    .as_str()
                    .map(ToOwned::to_owned)
            });

        let pr_url = match existing_url {
            Some(url) => url,
            None => {
                let body = format!(
                    "{}\n\nProduced by overseer task {task_id}.",
                    task.body.trim()
                );
                gh(
                    &wt_path,
                    &[
                        "pr",
                        "create",
                        "--base",
                        base,
                        "--head",
                        &branch,
                        "--title",
                        &task.title,
                        "--body",
                        body.trim_start(),
                    ],
                )
                .await?
            }
        };

        if !config.landing.auto_merge {
            return Ok(LandOutcome::PrOpened { url: pr_url });
        }

        let push_target = format!("HEAD:{base}");
        match git(&wt_path, &["push", remote, &push_target]).await {
            Ok(_) => {
                let sha = git(&wt_path, &["rev-parse", "HEAD"])
                    .await?
                    .trim()
                    .to_owned();
                store.set_status(task_id, TaskStatus::Done)?;
                return Ok(LandOutcome::Merged { commit: sha });
            }
            Err(_) if attempt == 0 => {
                attempt = attempt.saturating_add(1);
                continue;
            }
            Err(err) => {
                return Err(LandingError::Refusal(format!(
                    "push to {remote}/{base} was rejected because base moved; aborting after retry: {err}"
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::PathBuf;

    use overseer_core::{NewReview, NewTask, Verdict};

    use super::*;
    use crate::test_support::{git_sync, init_repo};
    use crate::worktree::{commit_all, create};

    fn setup_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, Store, Config) {
        let root = tempfile::tempdir().unwrap();
        let (repo, bare, state) = (
            root.path().join("repo"),
            root.path().join("bare.git"),
            root.path().join("state"),
        );
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        std::fs::create_dir_all(&bare).unwrap();
        git_sync(&bare, &["init", "--bare", "-b", "main"]);
        git_sync(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git_sync(&repo, &["push", "-u", "origin", "main"]);
        std::fs::create_dir_all(&state).unwrap();
        let store = Store::open_in_memory().unwrap();
        let cfg = Config::from_toml_str(&format!(
            "repo = \"{}\"\ndefault_profile = \"local\"\n[landing]\nremote = \"origin\"\nbase = \"main\"\ntest_command = \"true\"\n[[profiles]]\nname = \"local\"\nbackend = \"openai\"\nbase_url = \"http://127.0.0.1:8080/v1\"\nmodel = \"local\"\n",
            repo.display()
        )).unwrap();
        (root, repo, state, store, cfg)
    }

    fn add_test_task(store: &mut Store, title: &str) -> overseer_core::Task {
        store
            .add_task(NewTask {
                title: title.to_owned(),
                body: String::new(),
                priority: 1,
                profile: None,
                depends_on: Vec::new(),
                ready: true,
                source: None,
                external_id: None,
                parent_task_id: None,
            })
            .unwrap()
    }

    #[tokio::test]
    async fn gate_refuses_when_task_not_in_review() {
        let (_root, _repo, state, mut store, config) = setup_fixture();
        let task = add_test_task(&mut store, "task 1");
        let err = land(&mut store, &config, &state, task.id)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, LandingError::Refusal(msg) if msg.contains("only tasks in review can be landed"))
        );
    }

    #[tokio::test]
    async fn gate_refuses_when_latest_review_verdict_not_pass() {
        let (_root, _repo, state, mut store, config) = setup_fixture();
        let task = add_test_task(&mut store, "task 2");
        store.set_status(task.id, TaskStatus::Review).unwrap();
        store
            .record_review(NewReview {
                task_id: task.id,
                round: 0,
                profile: "local".to_owned(),
                verdict: Verdict::Changes,
                path: PathBuf::from("review.md"),
            })
            .unwrap();

        let err = land(&mut store, &config, &state, task.id)
            .await
            .unwrap_err();
        assert!(matches!(&err, LandingError::Refusal(msg) if msg.contains("verdict CHANGES")));
    }

    #[tokio::test]
    async fn gate_refuses_when_fixup_commit_present() {
        let (_root, repo, state, mut store, config) = setup_fixture();
        let task = add_test_task(&mut store, "task 3");
        store.set_status(task.id, TaskStatus::Review).unwrap();
        store.set_approved(task.id, true).unwrap();

        let wt = create(&repo, &state, task.id).await.unwrap();
        std::fs::write(wt.path.join("file.txt"), "content\n").unwrap();
        commit_all(&wt.path, "fixup! fix something").await.unwrap();

        let err = land(&mut store, &config, &state, task.id)
            .await
            .unwrap_err();
        assert!(matches!(&err, LandingError::Refusal(msg) if msg.contains("fixup!")));
    }

    #[tokio::test]
    async fn gate_surfaces_test_command_failure_and_leaves_task_in_review() {
        let (_root, repo, state, mut store, mut config) = setup_fixture();
        let task = add_test_task(&mut store, "task 4");
        store.set_status(task.id, TaskStatus::Review).unwrap();
        store.set_approved(task.id, true).unwrap();

        let wt = create(&repo, &state, task.id).await.unwrap();
        std::fs::write(wt.path.join("file.txt"), "content\n").unwrap();
        commit_all(&wt.path, "add feature").await.unwrap();

        config.landing.test_command = Some("echo 'test failure details' >&2; exit 1".to_owned());

        let err = land(&mut store, &config, &state, task.id)
            .await
            .unwrap_err();
        if let LandingError::TestFailed { ref output, .. } = err {
            assert!(output.contains("test failure details"), "{output}");
        } else {
            assert!(
                matches!(err, LandingError::TestFailed { .. }),
                "expected TestFailed, got {err:?}"
            );
        }
        assert_eq!(store.get_task(task.id).unwrap().status, TaskStatus::Review);
    }

    #[tokio::test]
    async fn gate_refuses_when_auto_merge_true_without_test_command() {
        let (_root, _repo, state, mut store, mut config) = setup_fixture();
        let task = add_test_task(&mut store, "task 5");
        store.set_status(task.id, TaskStatus::Review).unwrap();
        store.set_approved(task.id, true).unwrap();

        config.landing.auto_merge = true;
        config.landing.test_command = None;

        let err = land(&mut store, &config, &state, task.id)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, LandingError::Refusal(msg) if msg.contains("test_command is required for auto-merge"))
        );
    }
}
