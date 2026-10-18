use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use overseer_core::{FailureKind, Profile, RunOutcome, RunRequest, Usage};
use tokio::io::AsyncWriteExt;

use crate::{Backend, BackendError, failed, tail};

/// How many characters of stderr are kept in the failure message.
const STDERR_EXCERPT_CHARS: usize = 500;

/// Runs an external program (a harness CLI such as `opencode run` or
/// `agy -p`, or any other tool) in the task worktree.
///
/// The prompt is appended as the final argument, or written to stdin when the
/// profile sets `prompt_stdin`. A system prompt, if any, is prepended to the
/// prompt text because a bare argv has no separate system channel. Usage is
/// not reported by commands and is always zero.
pub struct CommandBackend {
    profile_name: String,
    program: String,
    args: Vec<String>,
    prompt_stdin: bool,
    timeout: Duration,
}

impl CommandBackend {
    pub fn new(profile: &Profile) -> Result<Self, BackendError> {
        let argv = profile
            .command
            .as_deref()
            .and_then(|argv| argv.split_first())
            .ok_or_else(|| BackendError::InvalidProfile {
                profile: profile.name.clone(),
                field: "command",
            })?;
        let (program, args) = argv;
        Ok(Self {
            profile_name: profile.name.clone(),
            program: program.clone(),
            args: args.to_vec(),
            prompt_stdin: profile.prompt_stdin,
            timeout: Duration::from_secs(profile.timeout_secs),
        })
    }
}

#[async_trait]
impl Backend for CommandBackend {
    async fn run(&self, req: &RunRequest) -> Result<RunOutcome, BackendError> {
        if !req.workdir.is_dir() {
            return Err(BackendError::MissingWorkdir {
                path: req.workdir.clone(),
            });
        }
        let prompt = match &req.system {
            Some(system) => format!("{system}\n\n{}", req.prompt),
            None => req.prompt.clone(),
        };

        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(&self.args)
            .current_dir(&req.workdir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Dropping the child (on timeout or cancellation) kills it.
            .kill_on_drop(true);
        if self.prompt_stdin {
            command.stdin(Stdio::piped());
        } else {
            command.stdin(Stdio::null()).arg(&prompt);
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(
                    profile = %self.profile_name,
                    program = %self.program,
                    "command backend program not found"
                );
                return Ok(failed(FailureKind::Unreachable));
            }
            Err(source) => {
                return Err(BackendError::SpawnCommand {
                    program: self.program.clone(),
                    source,
                });
            }
        };

        // Feed stdin while the child's output is drained, so a child that
        // writes a lot before reading cannot deadlock against us.
        let stdin = child.stdin.take();
        let feed = async move {
            if let Some(mut stdin) = stdin {
                // A child may exit without reading its input; that is not a
                // failure of the run, so the write error is ignored.
                let _ = stdin.write_all(prompt.as_bytes()).await;
            }
        };
        let collect = async move {
            let ((), output) = tokio::join!(feed, child.wait_with_output());
            output
        };

        match tokio::time::timeout(self.timeout, collect).await {
            Err(_elapsed) => {
                tracing::warn!(profile = %self.profile_name, "command backend timed out");
                Ok(failed(FailureKind::Other(format!(
                    "timed out after {}s",
                    self.timeout.as_secs()
                ))))
            }
            Ok(Err(source)) => Ok(failed(FailureKind::Other(format!(
                "failed to wait for command: {source}"
            )))),
            Ok(Ok(output)) => Ok(classify(&output)),
        }
    }
}

fn classify(output: &std::process::Output) -> RunOutcome {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        return RunOutcome {
            output: stdout,
            usage: Usage::default(),
            failure: None,
        };
    }
    let code = output
        .status
        .code()
        .map_or_else(|| "signal".to_owned(), |code| code.to_string());
    let stderr = String::from_utf8_lossy(&output.stderr);
    RunOutcome {
        output: stdout,
        usage: Usage::default(),
        failure: Some(FailureKind::Other(format!(
            "exit {code}: {}",
            tail(&stderr, STDERR_EXCERPT_CHARS)
        ))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::PathBuf;
    use std::time::Instant;

    use overseer_core::{BackendKind, Permissions};

    use super::*;

    fn profile(argv: &[&str], prompt_stdin: bool, timeout_secs: u64) -> Profile {
        Profile {
            name: "agent".to_owned(),
            backend: BackendKind::Command,
            model: None,
            base_url: None,
            api_key_env: None,
            command: Some(argv.iter().map(|arg| (*arg).to_owned()).collect()),
            prompt_stdin,
            role_prompt: None,
            fallback: Vec::new(),
            permissions: Permissions::WriteWorkdir,
            timeout_secs,
            metered: false,
            permission_args: Default::default(),
        }
    }

    fn request(workdir: PathBuf, system: Option<&str>, prompt: &str) -> RunRequest {
        RunRequest {
            system: system.map(str::to_owned),
            prompt: prompt.to_owned(),
            workdir,
            permissions: Permissions::WriteWorkdir,
        }
    }

    #[tokio::test]
    async fn prompt_is_the_final_argument_after_system_text() {
        let dir = tempfile::tempdir().unwrap();
        let backend = CommandBackend::new(&profile(
            &["/bin/sh", "-c", "printf 'got:%s' \"$1\"", "sh"],
            false,
            10,
        ))
        .unwrap();
        let outcome = backend
            .run(&request(dir.path().to_path_buf(), Some("sys"), "fix it"))
            .await
            .unwrap();
        assert_eq!(outcome.output, "got:sys\n\nfix it");
        assert_eq!(outcome.failure, None);
        assert_eq!(outcome.usage, Usage::default());
    }

    #[tokio::test]
    async fn prompt_can_be_piped_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let backend = CommandBackend::new(&profile(&["/bin/sh", "-c", "cat"], true, 10)).unwrap();
        let outcome = backend
            .run(&request(dir.path().to_path_buf(), None, "from stdin"))
            .await
            .unwrap();
        assert_eq!(outcome.output, "from stdin");
        assert_eq!(outcome.failure, None);
    }

    #[tokio::test]
    async fn runs_in_the_workdir() {
        let dir = tempfile::tempdir().unwrap();
        let expected = std::fs::canonicalize(dir.path()).unwrap();
        let backend = CommandBackend::new(&profile(&["/bin/sh", "-c", "pwd"], false, 10)).unwrap();
        let outcome = backend
            .run(&request(dir.path().to_path_buf(), None, "ignored"))
            .await
            .unwrap();
        assert_eq!(outcome.output.trim(), expected.to_string_lossy());
    }

    #[tokio::test]
    async fn nonzero_exit_is_other_with_stderr_tail_and_keeps_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let backend = CommandBackend::new(&profile(
            &["/bin/sh", "-c", "echo partial; echo boom >&2; exit 3"],
            false,
            10,
        ))
        .unwrap();
        let outcome = backend
            .run(&request(dir.path().to_path_buf(), None, "x"))
            .await
            .unwrap();
        assert_eq!(outcome.output, "partial\n");
        let Some(FailureKind::Other(detail)) = outcome.failure else {
            panic!("expected Other, got {:?}", outcome.failure);
        };
        assert!(detail.starts_with("exit 3"), "{detail}");
        assert!(detail.contains("boom"), "{detail}");
    }

    #[tokio::test]
    async fn missing_program_is_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let backend =
            CommandBackend::new(&profile(&["overseer-test-no-such-program"], false, 10)).unwrap();
        let outcome = backend
            .run(&request(dir.path().to_path_buf(), None, "x"))
            .await
            .unwrap();
        assert_eq!(outcome.failure, Some(FailureKind::Unreachable));
    }

    #[tokio::test]
    async fn timeout_kills_the_child_and_fails_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let backend =
            CommandBackend::new(&profile(&["/bin/sh", "-c", "sleep 30"], false, 1)).unwrap();
        let started = Instant::now();
        let outcome = backend
            .run(&request(dir.path().to_path_buf(), None, "x"))
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        let Some(FailureKind::Other(detail)) = outcome.failure else {
            panic!("expected Other, got {:?}", outcome.failure);
        };
        assert!(detail.contains("timed out"), "{detail}");
    }

    #[tokio::test]
    async fn missing_workdir_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("not-created");
        let backend = CommandBackend::new(&profile(&["/bin/sh", "-c", "true"], false, 10)).unwrap();
        let err = backend
            .run(&request(gone.clone(), None, "x"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, BackendError::MissingWorkdir { path } if path == gone),
            "unexpected error"
        );
    }

    #[test]
    fn empty_command_is_an_invalid_profile() {
        let mut bad = profile(&["unused"], false, 10);
        bad.command = Some(Vec::new());
        let err = CommandBackend::new(&bad).err().unwrap();
        assert!(
            matches!(
                err,
                BackendError::InvalidProfile {
                    field: "command",
                    ..
                }
            ),
            "{err:?}"
        );
    }
}
