use std::time::Duration;

use async_trait::async_trait;
use overseer_core::{FailureKind, Profile, RunOutcome, RunRequest, Usage};
use serde_json::{Value, json};

use crate::{Backend, BackendError, failed, tail};

/// How many characters of an error body are kept in the failure message.
const BODY_EXCERPT_CHARS: usize = 300;

/// Where the bearer token for the server comes from.
#[derive(Clone)]
enum ApiKey {
    /// The profile names no key variable; no `Authorization` header is sent.
    None,
    Present(String),
    /// The profile names a variable that is unset or empty. Only the variable
    /// name is kept, never a value.
    MissingEnv(String),
}

/// Non-streaming client for any OpenAI-compatible `/chat/completions` server.
///
/// `reasoning_content` in responses is ignored: only `message.content` is
/// returned as the output.
pub struct OpenAiBackend {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: ApiKey,
}

impl OpenAiBackend {
    pub fn new(profile: &Profile) -> Result<Self, BackendError> {
        let api_key = match profile.api_key_env.as_deref() {
            None => ApiKey::None,
            Some(name) => match std::env::var(name) {
                Ok(value) if !value.is_empty() => ApiKey::Present(value),
                _ => ApiKey::MissingEnv(name.to_owned()),
            },
        };
        Self::from_parts(profile, api_key)
    }

    fn from_parts(profile: &Profile, api_key: ApiKey) -> Result<Self, BackendError> {
        let base_url = profile
            .base_url
            .as_deref()
            .ok_or_else(|| BackendError::InvalidProfile {
                profile: profile.name.clone(),
                field: "base_url",
            })?;
        let model = profile
            .model
            .clone()
            .ok_or_else(|| BackendError::InvalidProfile {
                profile: profile.name.clone(),
                field: "model",
            })?;
        // The client-level timeout covers connect, send and body read, so one
        // slow server cannot hold a run open indefinitely.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(profile.timeout_secs))
            .build()
            .map_err(|source| BackendError::BuildHttpClient {
                profile: profile.name.clone(),
                source,
            })?;
        Ok(Self {
            client,
            endpoint: format!("{}/chat/completions", base_url.trim_end_matches('/')),
            model,
            api_key,
        })
    }

    fn request_body(&self, req: &RunRequest) -> Value {
        let mut messages = Vec::new();
        if let Some(system) = &req.system {
            messages.push(json!({"role": "system", "content": system}));
        }
        messages.push(json!({"role": "user", "content": req.prompt}));
        json!({
            "model": self.model,
            "messages": messages,
            "stream": false,
        })
    }
}

#[async_trait]
impl Backend for OpenAiBackend {
    async fn run(&self, req: &RunRequest) -> Result<RunOutcome, BackendError> {
        let mut request = self
            .client
            .post(&self.endpoint)
            .json(&self.request_body(req));
        match &self.api_key {
            ApiKey::None => {}
            ApiKey::Present(key) => request = request.bearer_auth(key),
            ApiKey::MissingEnv(name) => {
                tracing::warn!(env_var = %name, "api key environment variable is not set");
                return Ok(failed(FailureKind::AuthRequired));
            }
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(err) if err.is_connect() || err.is_timeout() => {
                tracing::warn!(error = %err.without_url(), "openai endpoint unreachable");
                return Ok(failed(FailureKind::Unreachable));
            }
            Err(err) => {
                return Ok(failed(FailureKind::Other(format!(
                    "request failed: {}",
                    err.without_url()
                ))));
            }
        };

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Ok(failed(FailureKind::AuthRequired));
        }
        if status == reqwest::StatusCode::PAYMENT_REQUIRED
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            return Ok(failed(FailureKind::QuotaExhausted));
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Ok(failed(FailureKind::Other(format!(
                "status {}: {}",
                status.as_u16(),
                tail(&body, BODY_EXCERPT_CHARS)
            ))));
        }

        let body = match response.text().await {
            Ok(body) => body,
            Err(err) => {
                return Ok(failed(FailureKind::Other(format!(
                    "failed to read response body: {}",
                    err.without_url()
                ))));
            }
        };
        Ok(parse_completion(&body))
    }
}

fn parse_completion(body: &str) -> RunOutcome {
    let value: Value = match serde_json::from_str(body) {
        Ok(value) => value,
        Err(_) => {
            return failed(FailureKind::Other(format!(
                "response is not JSON: {}",
                tail(body, BODY_EXCERPT_CHARS)
            )));
        }
    };
    let Some(content) = value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
    else {
        return failed(FailureKind::Other(
            "response has no choices[0].message.content".to_owned(),
        ));
    };
    let usage = Usage {
        input_tokens: value
            .pointer("/usage/prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .pointer("/usage/completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    };
    RunOutcome {
        output: content.to_owned(),
        usage,
        failure: None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::net::TcpListener;
    use std::path::PathBuf;

    use overseer_core::{BackendKind, Permissions};
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn profile(base_url: &str, timeout_secs: u64) -> Profile {
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
            timeout_secs,
            metered: false,
            permission_args: Default::default(),
            env: Default::default(),
        }
    }

    fn request() -> RunRequest {
        RunRequest {
            system: Some("You are careful.".to_owned()),
            prompt: "fix the parser".to_owned(),
            workdir: PathBuf::from("/unused"),
            permissions: Permissions::WriteWorkdir,
        }
    }

    fn completion(content: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": content,
                    "reasoning_content": "private chain of thought"
                }
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 4}
        }))
    }

    /// A localhost port with nothing listening on it.
    fn closed_port_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        format!("http://127.0.0.1:{port}/v1")
    }

    async fn run_against(server: &MockServer, key: ApiKey) -> RunOutcome {
        let backend =
            OpenAiBackend::from_parts(&profile(&format!("{}/v1", server.uri()), 5), key).unwrap();
        backend.run(&request()).await.unwrap()
    }

    #[tokio::test]
    async fn success_returns_content_usage_and_ignores_reasoning() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .and(body_partial_json(json!({
                "model": "local",
                "stream": false,
                "messages": [
                    {"role": "system", "content": "You are careful."},
                    {"role": "user", "content": "fix the parser"}
                ]
            })))
            .respond_with(completion("done"))
            .expect(1)
            .mount(&server)
            .await;

        let outcome = run_against(&server, ApiKey::Present("test-key".to_owned())).await;
        assert_eq!(
            outcome,
            RunOutcome {
                output: "done".to_owned(),
                usage: Usage {
                    input_tokens: 12,
                    output_tokens: 4
                },
                failure: None,
            }
        );
    }

    #[tokio::test]
    async fn system_prompt_is_optional() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({
                "messages": [{"role": "user", "content": "fix the parser"}]
            })))
            .respond_with(completion("ok"))
            .expect(1)
            .mount(&server)
            .await;

        let backend = OpenAiBackend::from_parts(&profile(&server.uri(), 5), ApiKey::None).unwrap();
        let outcome = backend
            .run(&RunRequest {
                system: None,
                ..request()
            })
            .await
            .unwrap();
        assert_eq!(outcome.output, "ok");
    }

    #[tokio::test]
    async fn unauthorized_is_auth_required() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;
        let outcome = run_against(&server, ApiKey::None).await;
        assert_eq!(outcome.failure, Some(FailureKind::AuthRequired));
        assert!(outcome.output.is_empty());
    }

    #[tokio::test]
    async fn forbidden_is_auth_required() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let outcome = run_against(&server, ApiKey::None).await;
        assert_eq!(outcome.failure, Some(FailureKind::AuthRequired));
    }

    #[tokio::test]
    async fn rate_limit_is_quota_exhausted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
            .mount(&server)
            .await;
        let outcome = run_against(&server, ApiKey::None).await;
        assert_eq!(outcome.failure, Some(FailureKind::QuotaExhausted));
    }

    #[tokio::test]
    async fn payment_required_is_quota_exhausted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(402))
            .mount(&server)
            .await;
        let outcome = run_against(&server, ApiKey::None).await;
        assert_eq!(outcome.failure, Some(FailureKind::QuotaExhausted));
    }

    #[tokio::test]
    async fn server_error_is_other_with_status_and_body_excerpt() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("model crashed"))
            .mount(&server)
            .await;
        let outcome = run_against(&server, ApiKey::None).await;
        let Some(FailureKind::Other(detail)) = outcome.failure else {
            panic!("expected Other, got {:?}", outcome.failure);
        };
        assert!(detail.contains("status 500"), "{detail}");
        assert!(detail.contains("model crashed"), "{detail}");
    }

    #[tokio::test]
    async fn connection_refused_is_unreachable() {
        let backend =
            OpenAiBackend::from_parts(&profile(&closed_port_url(), 5), ApiKey::None).unwrap();
        let outcome = backend.run(&request()).await.unwrap();
        assert_eq!(outcome.failure, Some(FailureKind::Unreachable));
    }

    #[tokio::test]
    async fn slow_server_hits_profile_timeout_and_is_unreachable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(completion("late").set_delay(Duration::from_secs(5)))
            .mount(&server)
            .await;
        let backend = OpenAiBackend::from_parts(&profile(&server.uri(), 1), ApiKey::None).unwrap();
        let outcome = backend.run(&request()).await.unwrap();
        assert_eq!(outcome.failure, Some(FailureKind::Unreachable));
    }

    #[tokio::test]
    async fn missing_api_key_variable_fails_without_sending_a_request() {
        // The endpoint is closed, so a request would be Unreachable. Getting
        // AuthRequired proves nothing was sent.
        let backend = OpenAiBackend::from_parts(
            &profile(&closed_port_url(), 5),
            ApiKey::MissingEnv("OVERSEER_TEST_UNSET_KEY".to_owned()),
        )
        .unwrap();
        let outcome = backend.run(&request()).await.unwrap();
        assert_eq!(outcome.failure, Some(FailureKind::AuthRequired));
    }

    #[tokio::test]
    async fn response_without_choices_is_other() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"choices": []})))
            .mount(&server)
            .await;
        let outcome = run_against(&server, ApiKey::None).await;
        assert!(matches!(outcome.failure, Some(FailureKind::Other(_))));
    }

    #[tokio::test]
    async fn missing_base_url_is_an_invalid_profile() {
        let mut bad = profile("http://unused", 5);
        bad.base_url = None;
        let err = OpenAiBackend::from_parts(&bad, ApiKey::None).err().unwrap();
        assert!(
            matches!(
                err,
                BackendError::InvalidProfile {
                    field: "base_url",
                    ..
                }
            ),
            "{err:?}"
        );
    }
}
