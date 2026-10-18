use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use overseer_core::{FailureKind, Permissions, Profile, RunOutcome, RunRequest, Usage};
use tokio::io::AsyncWriteExt;

use crate::{Backend, BackendError, failed, tail};

/// How many characters of stderr are kept in the failure message.
const STDERR_EXCERPT_CHARS: usize = 500;

/// Output phrases that mean the harness needs a fresh login or key.
const AUTH_PHRASES: [&str; 5] = [
    "authentication required",
    "not logged in",
    "please run /login",
    "invalid api key",
    "unauthorized",
];

/// Output phrases that mean the account has run out of quota or credits.
const QUOTA_PHRASES: [&str; 8] = [
    "quota",
    "rate limit",
    "usage limit",
    "credit balance is too low",
    "out of credits",
    "resource exhausted",
    "429",
    "limit reached",
];

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
    prompt_flag: Option<String>,
    timeout: Duration,
    ceiling: Permissions,
    permission_args: BTreeMap<Permissions, Vec<String>>,
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
            prompt_flag: profile.prompt_flag.clone(),
            timeout: Duration::from_secs(profile.timeout_secs),
            ceiling: profile.permissions,
            permission_args: profile.permission_args.clone(),
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

        let effective = self.ceiling.min(req.permissions);
        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(&self.args)
            .args(self.permission_args.get(&effective).into_iter().flatten())
            .current_dir(&req.workdir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Dropping the child (on timeout or cancellation) kills it.
            .kill_on_drop(true);
        if self.prompt_stdin {
            command.stdin(Stdio::piped());
        } else {
            if let Some(flag) = &self.prompt_flag {
                command.arg(flag);
            }
            command.stdin(Stdio::null()).arg(&prompt);
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(source) => {
                tracing::warn!(
                    profile = %self.profile_name,
                    program = %self.program,
                    error = %source,
                    "command backend could not start its program"
                );
                return Ok(failed(FailureKind::Unreachable));
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
    let stderr = String::from_utf8_lossy(&output.stderr);
    let failure = if output.status.success() && !stdout.trim().is_empty() {
        None
    } else {
        Some(classify_failure(&stdout, &stderr, output.status.code()))
    };
    RunOutcome {
        output: stdout,
        usage: Usage::default(),
        failure,
    }
}

/// Decide why a command run failed from what it printed. Phrase matches win
/// over the exit code, so a quota message is routable even when the program
/// exits non-zero with a generic status.
fn classify_failure(stdout: &str, stderr: &str, exit_code: Option<i32>) -> FailureKind {
    let haystack = format!("{stdout}\n{stderr}").to_lowercase();
    if AUTH_PHRASES.iter().any(|phrase| haystack.contains(phrase)) {
        return FailureKind::AuthRequired;
    }
    if QUOTA_PHRASES.iter().any(|phrase| haystack.contains(phrase)) {
        return FailureKind::QuotaExhausted;
    }
    if exit_code == Some(0) && stdout.trim().is_empty() {
        return FailureKind::Other("empty output".to_owned());
    }
    let code = exit_code.map_or_else(|| "signal".to_owned(), |code| code.to_string());
    FailureKind::Other(format!(
        "exit {code}: {}",
        tail(stderr, STDERR_EXCERPT_CHARS)
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::PathBuf;
    use std::time::Instant;

    use overseer_core::{BackendKind, Permissions};

    use super::FailureKind::{AuthRequired, Other, QuotaExhausted};
    use super::*;

    fn profile(argv: &[&str], prompt_stdin: bool, timeout_secs: u64) -> Profile {
        Profile {
            name: "agent".to_owned(),
            backend: BackendKind::Command,
            model: None,
            base_url: None,
            api_key_env: None,
            command: Some(argv.iter().map(|arg| (*arg).to_owned()).collect()),
            prompt_flag: None,
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

    #[tokio::test]
    async fn permission_args_for_the_effective_level_go_before_the_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = profile(
            &[
                "/bin/sh",
                "-c",
                "for a in \"$@\"; do printf '[%s]' \"$a\"; done",
                "sh",
            ],
            false,
            10,
        );
        agent.permission_args.insert(
            Permissions::WriteWorkdir,
            vec!["--mode".to_owned(), "accept-edits".to_owned()],
        );
        let backend = CommandBackend::new(&agent).unwrap();
        let outcome = backend
            .run(&request(dir.path().to_path_buf(), None, "fix it"))
            .await
            .unwrap();
        assert_eq!(outcome.output, "[--mode][accept-edits][fix it]");
    }

    #[tokio::test]
    async fn permission_args_are_clamped_to_the_profile_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = profile(
            &[
                "/bin/sh",
                "-c",
                "for a in \"$@\"; do printf '[%s]' \"$a\"; done",
                "sh",
            ],
            false,
            10,
        );
        agent.permissions = Permissions::WriteWorkdir;
        agent
            .permission_args
            .insert(Permissions::WriteShell, vec!["--yes".to_owned()]);
        let backend = CommandBackend::new(&agent).unwrap();
        let mut req = request(dir.path().to_path_buf(), None, "p");
        req.permissions = Permissions::WriteShell;
        let outcome = backend.run(&req).await.unwrap();
        assert_eq!(
            outcome.output, "[p]",
            "write-shell args must not leak past the ceiling"
        );
    }

    #[tokio::test]
    async fn non_executable_program_is_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("not-executable");
        std::fs::write(&program, "#!/bin/sh\necho hi\n").unwrap();
        let program = program.to_string_lossy().into_owned();
        let backend = CommandBackend::new(&profile(&[program.as_str()], false, 10)).unwrap();
        let outcome = backend
            .run(&request(dir.path().to_path_buf(), None, "x"))
            .await
            .unwrap();
        assert_eq!(outcome.failure, Some(FailureKind::Unreachable));
    }

    #[test]
    fn classify_failure_maps_known_phrases_case_insensitively() {
        let cases = [
            ("", "Error: Authentication Required", AuthRequired),
            ("", "You are NOT LOGGED IN", AuthRequired),
            ("Please run /login to continue", "", AuthRequired),
            ("", "Invalid API Key", AuthRequired),
            ("", "401 Unauthorized", AuthRequired),
            ("", "Quota exceeded for this month", QuotaExhausted),
            ("", "Rate Limit reached", QuotaExhausted),
            ("", "your usage limit was hit", QuotaExhausted),
            ("", "Credit balance is too low", QuotaExhausted),
            ("", "you are out of credits", QuotaExhausted),
            ("", "Resource Exhausted", QuotaExhausted),
            ("", "HTTP 429 Too Many Requests", QuotaExhausted),
            ("", "monthly limit reached", QuotaExhausted),
        ];
        for (stdout, stderr, expected) in cases {
            assert_eq!(
                classify_failure(stdout, stderr, Some(1)),
                expected,
                "stdout {stdout:?}, stderr {stderr:?}"
            );
        }
    }

    #[test]
    fn classify_failure_keeps_other_failures_with_exit_code_and_stderr() {
        assert_eq!(
            classify_failure("", "segfault", Some(139)),
            Other("exit 139: segfault".to_owned())
        );
        assert_eq!(
            classify_failure("", "bad flag", None),
            Other("exit signal: bad flag".to_owned())
        );
    }

    #[test]
    fn exit_zero_with_empty_output_is_an_other_failure() {
        assert_eq!(
            classify_failure("  \n", "", Some(0)),
            Other("empty output".to_owned())
        );
    }

    #[tokio::test]
    async fn exit_zero_with_empty_output_fails_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let backend = CommandBackend::new(&profile(&["/bin/sh", "-c", "true"], false, 10)).unwrap();
        let outcome = backend
            .run(&request(dir.path().to_path_buf(), None, "x"))
            .await
            .unwrap();
        assert_eq!(outcome.failure, Some(Other("empty output".to_owned())));
    }

    #[tokio::test]
    async fn quota_message_on_stderr_is_quota_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let backend = CommandBackend::new(&profile(
            &["/bin/sh", "-c", "echo 'rate limit hit' >&2; exit 1"],
            false,
            10,
        ))
        .unwrap();
        let outcome = backend
            .run(&request(dir.path().to_path_buf(), None, "x"))
            .await
            .unwrap();
        assert_eq!(outcome.failure, Some(QuotaExhausted));
    }

    fn write_script(path: &std::path::Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_path_buf()
    }

    #[tokio::test]
    async fn prompt_flag_appends_flag_and_prompt_after_permission_args() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("recorded_argv.txt");
        let script = write_script(
            &dir.path().join("fake_cli.sh"),
            &format!(
                r#"for a in "$@"; do printf '[%s]' "$a"; done > "{}"
echo "fake ran"
"#,
                log.display()
            ),
        );
        let mut agent = profile(
            &["/bin/sh", script.to_str().unwrap(), "subcmd", "--verbose"],
            false,
            10,
        );
        agent.prompt_flag = Some("-p".to_owned());
        agent.permission_args.insert(
            Permissions::WriteWorkdir,
            vec!["--mode".to_owned(), "accept-edits".to_owned()],
        );
        let backend = CommandBackend::new(&agent).unwrap();
        let outcome = backend
            .run(&request(dir.path().to_path_buf(), None, "the prompt"))
            .await
            .unwrap();
        assert_eq!(outcome.failure, None);
        let recorded = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            recorded,
            "[subcmd][--verbose][--mode][accept-edits][-p][the prompt]"
        );
    }

    #[tokio::test]
    async fn prompt_flag_ignored_in_stdin_mode() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("recorded_stdin_argv.txt");
        let script = write_script(
            &dir.path().join("fake_stdin_cli.sh"),
            &format!(
                r#"for a in "$@"; do printf '[%s]' "$a"; done > "{}"
cat > /dev/null
echo "fake ran"
"#,
                log.display()
            ),
        );
        let mut agent = profile(&["/bin/sh", script.to_str().unwrap()], true, 10);
        agent.prompt_flag = Some("-p".to_owned());
        let backend = CommandBackend::new(&agent).unwrap();
        let outcome = backend
            .run(&request(dir.path().to_path_buf(), None, "from stdin"))
            .await
            .unwrap();
        assert_eq!(outcome.failure, None);
        let recorded = std::fs::read_to_string(&log).unwrap();
        assert_eq!(recorded, "");
    }
}
