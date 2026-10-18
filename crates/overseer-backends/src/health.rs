use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use overseer_core::{BackendKind, Permissions, Profile, RunOutcome, RunRequest};
use serde::{Deserialize, Serialize};

/// The health status of a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub ok: bool,
    pub detail: String,
}

/// Check whether a profile can run right now.
pub async fn check_profile(profile: &Profile, deep: bool) -> Health {
    check_profile_with(
        profile,
        deep,
        std::env::var_os("PATH").as_deref(),
        None,
        |k| std::env::var(k).ok().filter(|v| !v.is_empty()),
    )
    .await
}

pub(crate) async fn check_profile_with<F>(
    profile: &Profile,
    deep: bool,
    path_env: Option<&OsStr>,
    current_dir: Option<&Path>,
    env_lookup: F,
) -> Health
where
    F: Fn(&str) -> Option<String>,
{
    // Hard 5 s timeout for shallow check
    let shallow_res = tokio::time::timeout(
        Duration::from_secs(5),
        check_shallow(profile, path_env, current_dir, &env_lookup),
    )
    .await;

    let shallow = match shallow_res {
        Ok(h) => h,
        Err(_) => Health {
            ok: false,
            detail: "unreachable".to_owned(),
        },
    };

    if !shallow.ok || !deep {
        return shallow;
    }

    check_deep(profile).await
}

async fn check_shallow<F>(
    profile: &Profile,
    path_env: Option<&OsStr>,
    current_dir: Option<&Path>,
    env_lookup: &F,
) -> Health
where
    F: Fn(&str) -> Option<String>,
{
    match profile.backend {
        BackendKind::Openai => check_openai(profile, env_lookup).await,
        BackendKind::Command => check_command(profile, path_env, current_dir),
    }
}

async fn check_openai<F>(profile: &Profile, env_lookup: &F) -> Health
where
    F: Fn(&str) -> Option<String>,
{
    let base_url = match profile.base_url.as_deref() {
        Some(url) => url,
        None => {
            return Health {
                ok: false,
                detail: "missing base_url".to_owned(),
            };
        }
    };

    let mut auth_header: Option<String> = None;
    if let Some(var_name) = profile.api_key_env.as_deref() {
        match env_lookup(var_name) {
            Some(val) => {
                auth_header = Some(val);
            }
            None => {
                return Health {
                    ok: false,
                    detail: format!("api key variable {var_name} is not set"),
                };
            }
        }
    }

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            return Health {
                ok: false,
                detail: err.to_string(),
            };
        }
    };

    let endpoint = format!("{}/models", base_url.trim_end_matches('/'));
    let mut req_builder = client.get(&endpoint);
    if let Some(key) = auth_header {
        req_builder = req_builder.bearer_auth(key);
    }

    let response = match req_builder.send().await {
        Ok(resp) => resp,
        Err(err) if err.is_connect() || err.is_timeout() => {
            return Health {
                ok: false,
                detail: "unreachable".to_owned(),
            };
        }
        Err(err) => {
            return Health {
                ok: false,
                detail: format!("request failed: {}", err.without_url()),
            };
        }
    };

    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Health {
            ok: false,
            detail: "auth required".to_owned(),
        };
    }
    if status == reqwest::StatusCode::PAYMENT_REQUIRED
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        return Health {
            ok: false,
            detail: "quota exhausted".to_owned(),
        };
    }
    if status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let first_model_id = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|val| {
                val.pointer("/data/0/id")
                    .and_then(|v| v.as_str())
                    .or_else(|| val.pointer("/data/0").and_then(|v| v.as_str()))
                    .or_else(|| val.pointer("/models/0/name").and_then(|v| v.as_str()))
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_default();
        return Health {
            ok: true,
            detail: first_model_id,
        };
    }

    Health {
        ok: false,
        detail: format!("status {}", status.as_u16()),
    }
}

fn check_command(
    profile: &Profile,
    path_env: Option<&OsStr>,
    current_dir: Option<&Path>,
) -> Health {
    let argv = match &profile.command {
        Some(cmd) if !cmd.is_empty() => cmd,
        _ => {
            return Health {
                ok: false,
                detail: "missing command".to_owned(),
            };
        }
    };
    let Some(raw_cmd) = argv.first() else {
        return Health {
            ok: false,
            detail: "missing command".to_owned(),
        };
    };

    let expanded = crate::command::expand_tilde(raw_cmd);
    if expanded.contains('/') {
        let p = Path::new(&expanded);
        let resolved = if p.is_relative() {
            if let Some(base) = current_dir {
                base.join(p)
            } else {
                p.to_path_buf()
            }
        } else {
            p.to_path_buf()
        };
        match std::fs::metadata(&resolved) {
            Ok(meta) => {
                let is_exec = meta.is_file() && (meta.permissions().mode() & 0o111 != 0);
                if is_exec {
                    Health {
                        ok: true,
                        detail: expanded,
                    }
                } else {
                    Health {
                        ok: false,
                        detail: format!("not executable: {raw_cmd}"),
                    }
                }
            }
            Err(_) => Health {
                ok: false,
                detail: format!("not found: {raw_cmd}"),
            },
        }
    } else {
        let Some(path_var) = path_env else {
            return Health {
                ok: false,
                detail: format!("not found: {raw_cmd}"),
            };
        };

        let mut found_non_exec = false;
        for dir in std::env::split_paths(path_var) {
            let candidate = dir.join(&expanded);
            if let Ok(meta) = std::fs::metadata(&candidate)
                && meta.is_file()
            {
                if meta.permissions().mode() & 0o111 != 0 {
                    return Health {
                        ok: true,
                        detail: candidate.display().to_string(),
                    };
                } else {
                    found_non_exec = true;
                }
            }
        }

        if found_non_exec {
            Health {
                ok: false,
                detail: format!("not executable: {raw_cmd}"),
            }
        } else {
            Health {
                ok: false,
                detail: format!("not found: {raw_cmd}"),
            }
        }
    }
}

async fn check_deep(profile: &Profile) -> Health {
    let temp_dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(err) => {
            return Health {
                ok: false,
                detail: format!("failed to create temp dir: {err}"),
            };
        }
    };

    let backend = match crate::build_backend(profile) {
        Ok(b) => b,
        Err(err) => {
            return Health {
                ok: false,
                detail: err.to_string(),
            };
        }
    };

    let req = RunRequest {
        system: profile.role_prompt.clone(),
        prompt: "Reply with the single word ok.".to_owned(),
        workdir: temp_dir.path().to_path_buf(),
        permissions: Permissions::Read,
    };

    let deep_res = tokio::time::timeout(Duration::from_secs(120), backend.run(&req)).await;
    match deep_res {
        Err(_) => Health {
            ok: false,
            detail: "timed out after 120s".to_owned(),
        },
        Ok(Err(err)) => Health {
            ok: false,
            detail: err.to_string(),
        },
        Ok(Ok(RunOutcome {
            failure: Some(kind),
            ..
        })) => Health {
            ok: false,
            detail: kind.to_string(),
        },
        Ok(Ok(RunOutcome {
            failure: None,
            output,
            ..
        })) => Health {
            ok: true,
            detail: output.trim().to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use overseer_core::{BackendKind, Permissions, Profile};
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn openai_profile(base_url: &str) -> Profile {
        Profile {
            name: "local".to_owned(),
            backend: BackendKind::Openai,
            model: Some("local".to_owned()),
            base_url: Some(base_url.to_owned()),
            api_key_env: None,
            command: None,
            prompt_flag: None,
            prompt_stdin: false,
            role_prompt: None,
            fallback: Vec::new(),
            permissions: Permissions::WriteWorkdir,
            timeout_secs: 5,
            metered: false,
            permission_args: Default::default(),
            env: Default::default(),
            agent_tools: false,
            max_steps: 40,
            skills: Vec::new(),
            gpu: false,
        }
    }

    fn command_profile(argv: &[&str]) -> Profile {
        Profile {
            name: "cmd".to_owned(),
            backend: BackendKind::Command,
            model: None,
            base_url: None,
            api_key_env: None,
            command: Some(argv.iter().map(|s| (*s).to_owned()).collect()),
            prompt_flag: None,
            prompt_stdin: false,
            role_prompt: None,
            fallback: Vec::new(),
            permissions: Permissions::WriteWorkdir,
            timeout_secs: 5,
            metered: false,
            permission_args: Default::default(),
            env: Default::default(),
            agent_tools: false,
            max_steps: 40,
            skills: Vec::new(),
            gpu: false,
        }
    }

    fn write_script(path: &Path, body: &str, executable: bool) -> PathBuf {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        if executable {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        } else {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        path.to_path_buf()
    }

    fn closed_port_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        format!("http://127.0.0.1:{port}/v1")
    }

    #[tokio::test]
    async fn openai_models_2xx_reports_first_model_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "data": [
                    {"id": "llama-3-8b", "object": "model"},
                    {"id": "qwen-2.5", "object": "model"}
                ]
            })))
            .mount(&server)
            .await;

        let p = openai_profile(&format!("{}/v1", server.uri()));
        let health = check_profile(&p, false).await;
        assert_eq!(
            health,
            Health {
                ok: true,
                detail: "llama-3-8b".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn openai_models_2xx_with_empty_data_reports_empty_detail() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "data": []
            })))
            .mount(&server)
            .await;

        let p = openai_profile(&format!("{}/v1", server.uri()));
        let health = check_profile(&p, false).await;
        assert_eq!(
            health,
            Health {
                ok: true,
                detail: String::new(),
            }
        );
    }

    #[tokio::test]
    async fn openai_unauthorized_is_auth_required() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let p = openai_profile(&format!("{}/v1", server.uri()));
        let health = check_profile(&p, false).await;
        assert_eq!(
            health,
            Health {
                ok: false,
                detail: "auth required".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn openai_forbidden_is_auth_required() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let p = openai_profile(&format!("{}/v1", server.uri()));
        let health = check_profile(&p, false).await;
        assert_eq!(
            health,
            Health {
                ok: false,
                detail: "auth required".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn openai_rate_limit_is_quota_exhausted() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let p = openai_profile(&format!("{}/v1", server.uri()));
        let health = check_profile(&p, false).await;
        assert_eq!(
            health,
            Health {
                ok: false,
                detail: "quota exhausted".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn openai_500_is_status_500() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let p = openai_profile(&format!("{}/v1", server.uri()));
        let health = check_profile(&p, false).await;
        assert_eq!(
            health,
            Health {
                ok: false,
                detail: "status 500".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn openai_connection_refused_is_unreachable() {
        let p = openai_profile(&closed_port_url());
        let health = check_profile(&p, false).await;
        assert_eq!(
            health,
            Health {
                ok: false,
                detail: "unreachable".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn openai_timeout_is_unreachable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(6)))
            .mount(&server)
            .await;

        let p = openai_profile(&format!("{}/v1", server.uri()));
        let health = check_profile(&p, false).await;
        assert_eq!(
            health,
            Health {
                ok: false,
                detail: "unreachable".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn openai_missing_api_key_env_fails_without_network() {
        let mut p = openai_profile(&closed_port_url());
        p.api_key_env = Some("OVERSEER_TEST_NONEXISTENT_KEY_XYZ".to_owned());

        let health = check_profile_with(&p, false, None, None, |_| None).await;
        assert_eq!(
            health,
            Health {
                ok: false,
                detail: "api key variable OVERSEER_TEST_NONEXISTENT_KEY_XYZ is not set".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn openai_present_api_key_env_sends_bearer_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer my-secret-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": "gpt-4"}]
            })))
            .mount(&server)
            .await;

        let mut p = openai_profile(&format!("{}/v1", server.uri()));
        p.api_key_env = Some("MY_KEY".to_owned());

        let health = check_profile_with(&p, false, None, None, |k| {
            if k == "MY_KEY" {
                Some("my-secret-key".to_owned())
            } else {
                None
            }
        })
        .await;

        assert_eq!(
            health,
            Health {
                ok: true,
                detail: "gpt-4".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn command_absolute_path_executable_is_ok() {
        let temp = tempfile::tempdir().unwrap();
        let script = write_script(&temp.path().join("tool.sh"), "echo ok", true);
        let p = command_profile(&[script.to_str().unwrap()]);
        let health = check_profile(&p, false).await;
        assert_eq!(
            health,
            Health {
                ok: true,
                detail: script.display().to_string(),
            }
        );
    }

    #[tokio::test]
    async fn command_absolute_path_not_executable_is_reported() {
        let temp = tempfile::tempdir().unwrap();
        let script = write_script(&temp.path().join("not_exec.sh"), "echo ok", false);
        let raw = script.to_str().unwrap();
        let p = command_profile(&[raw]);
        let health = check_profile(&p, false).await;
        assert_eq!(
            health,
            Health {
                ok: false,
                detail: format!("not executable: {raw}"),
            }
        );
    }

    #[tokio::test]
    async fn command_absolute_path_missing_is_reported() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing.sh");
        let raw = missing.to_str().unwrap();
        let p = command_profile(&[raw]);
        let health = check_profile(&p, false).await;
        assert_eq!(
            health,
            Health {
                ok: false,
                detail: format!("not found: {raw}"),
            }
        );
    }

    #[tokio::test]
    async fn command_relative_path_executable_is_ok() {
        let temp = tempfile::tempdir().unwrap();
        let _script = write_script(&temp.path().join("rel_tool.sh"), "echo ok", true);

        let p = command_profile(&["./rel_tool.sh"]);
        let health = check_profile_with(&p, false, None, Some(temp.path()), |_| None).await;

        assert_eq!(
            health,
            Health {
                ok: true,
                detail: "./rel_tool.sh".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn command_relative_path_not_executable() {
        let temp = tempfile::tempdir().unwrap();
        let _script = write_script(&temp.path().join("rel_not_exec.sh"), "echo ok", false);

        let p = command_profile(&["./rel_not_exec.sh"]);
        let health = check_profile_with(&p, false, None, Some(temp.path()), |_| None).await;

        assert_eq!(
            health,
            Health {
                ok: false,
                detail: "not executable: ./rel_not_exec.sh".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn command_path_search_finds_executable() {
        let temp = tempfile::tempdir().unwrap();
        let script = write_script(&temp.path().join("my-custom-cli"), "echo ok", true);

        let p = command_profile(&["my-custom-cli"]);
        let health =
            check_profile_with(&p, false, Some(temp.path().as_os_str()), None, |_| None).await;

        assert_eq!(
            health,
            Health {
                ok: true,
                detail: script.display().to_string(),
            }
        );
    }

    #[tokio::test]
    async fn command_path_search_not_executable() {
        let temp = tempfile::tempdir().unwrap();
        let _script = write_script(&temp.path().join("unexecutable-cli"), "echo ok", false);

        let p = command_profile(&["unexecutable-cli"]);
        let health =
            check_profile_with(&p, false, Some(temp.path().as_os_str()), None, |_| None).await;

        assert_eq!(
            health,
            Health {
                ok: false,
                detail: "not executable: unexecutable-cli".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn command_path_search_missing() {
        let p = command_profile(&["overseer-surely-nonexistent-command-12345"]);
        let health = check_profile(&p, false).await;
        assert_eq!(
            health,
            Health {
                ok: false,
                detail: "not found: overseer-surely-nonexistent-command-12345".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn command_system_sh_found_in_path() {
        let p = command_profile(&["sh"]);
        let health = check_profile(&p, false).await;
        assert!(health.ok);
        assert!(health.detail.contains("sh"), "{}", health.detail);
    }

    #[tokio::test]
    async fn deep_skipped_when_shallow_fails() {
        let p = command_profile(&["overseer-surely-nonexistent-command-12345"]);
        let health = check_profile(&p, true).await;
        assert_eq!(
            health,
            Health {
                ok: false,
                detail: "not found: overseer-surely-nonexistent-command-12345".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn deep_runs_command_and_reports_output() {
        let temp = tempfile::tempdir().unwrap();
        let script = write_script(&temp.path().join("ok_tool.sh"), "printf ok", true);
        let p = command_profile(&[script.to_str().unwrap()]);

        let health = check_profile(&p, true).await;
        assert_eq!(
            health,
            Health {
                ok: true,
                detail: "ok".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn deep_runs_command_and_reports_failure_kind() {
        let temp = tempfile::tempdir().unwrap();
        let script = write_script(
            &temp.path().join("fail_tool.sh"),
            "echo 'rate limit reached' >&2\nexit 1",
            true,
        );
        let p = command_profile(&[script.to_str().unwrap()]);

        let health = check_profile(&p, true).await;
        assert_eq!(
            health,
            Health {
                ok: false,
                detail: "quota-exhausted".to_owned(),
            }
        );
    }
}
