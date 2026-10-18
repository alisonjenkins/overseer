use std::time::Duration;

use async_trait::async_trait;
use overseer_core::{FailureKind, Profile, RunOutcome, RunRequest, Usage};
use serde_json::{Value, json};

use crate::{Backend, BackendError, failed, tail};

/// How many characters of an error body are kept in the failure message.
const BODY_EXCERPT_CHARS: usize = 300;

const TOOL_LOOP_SYSTEM_PROMPT: &str = "The working directory is the repository root. Use the provided tools to inspect and modify files as needed. Finish with a short summary of what you changed.";

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
    agent_tools: bool,
    max_steps: u32,
    context_chars: u32,
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
            agent_tools: profile.agent_tools,
            max_steps: profile.max_steps,
            context_chars: profile.context_chars,
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

    async fn send_chat_request(&self, body: &Value) -> Result<String, FailureKind> {
        let mut request = self.client.post(&self.endpoint).json(body);
        match &self.api_key {
            ApiKey::None => {}
            ApiKey::Present(key) => request = request.bearer_auth(key),
            ApiKey::MissingEnv(name) => {
                tracing::warn!(env_var = %name, "api key environment variable is not set");
                return Err(FailureKind::AuthRequired);
            }
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(err) if err.is_connect() || err.is_timeout() => {
                tracing::warn!(error = %err.without_url(), "openai endpoint unreachable");
                return Err(FailureKind::Unreachable);
            }
            Err(err) => {
                return Err(FailureKind::Other(format!(
                    "request failed: {}",
                    err.without_url()
                )));
            }
        };

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(FailureKind::AuthRequired);
        }
        if status == reqwest::StatusCode::PAYMENT_REQUIRED
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            return Err(FailureKind::QuotaExhausted);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(FailureKind::Other(format!(
                "status {}: {}",
                status.as_u16(),
                tail(&body, BODY_EXCERPT_CHARS)
            )));
        }

        match response.text().await {
            Ok(body) => Ok(body),
            Err(err) => Err(FailureKind::Other(format!(
                "failed to read response body: {}",
                err.without_url()
            ))),
        }
    }

    async fn run_agent_loop(&self, req: &RunRequest) -> Result<RunOutcome, BackendError> {
        let mut messages = Vec::new();
        let system_content = match &req.system {
            Some(sys) => format!("{sys}\n\n{TOOL_LOOP_SYSTEM_PROMPT}"),
            None => TOOL_LOOP_SYSTEM_PROMPT.to_owned(),
        };
        messages.push(json!({"role": "system", "content": system_content}));
        messages.push(json!({"role": "user", "content": req.prompt}));

        let tools = crate::tools::tool_definitions(req.permissions);

        let mut steps: u32 = 0;
        let mut total_usage = Usage::default();

        loop {
            if steps >= self.max_steps {
                return Ok(RunOutcome {
                    output: String::new(),
                    usage: total_usage,
                    failure: Some(FailureKind::Other("exceeded max_steps".to_owned())),
                });
            }
            steps = steps.saturating_add(1);

            if let Err(kind) = enforce_history_budget(&mut messages, self.context_chars as usize) {
                return Ok(RunOutcome {
                    output: String::new(),
                    usage: total_usage,
                    failure: Some(kind),
                });
            }

            let body = json!({
                "model": self.model,
                "messages": messages,
                "stream": false,
                "tools": tools,
            });

            let resp_text = match self.send_chat_request(&body).await {
                Ok(text) => text,
                Err(kind) => {
                    return Ok(RunOutcome {
                        output: String::new(),
                        usage: total_usage,
                        failure: Some(kind),
                    });
                }
            };

            let resp_val: Value = match serde_json::from_str(&resp_text) {
                Ok(val) => val,
                Err(_) => {
                    return Ok(RunOutcome {
                        output: String::new(),
                        usage: total_usage,
                        failure: Some(FailureKind::Other(format!(
                            "response is not JSON: {}",
                            tail(&resp_text, BODY_EXCERPT_CHARS)
                        ))),
                    });
                }
            };

            let input_tokens = resp_val
                .pointer("/usage/prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let output_tokens = resp_val
                .pointer("/usage/completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            total_usage.input_tokens = total_usage.input_tokens.saturating_add(input_tokens);
            total_usage.output_tokens = total_usage.output_tokens.saturating_add(output_tokens);

            let Some(choice_msg) = resp_val.pointer("/choices/0/message") else {
                return Ok(RunOutcome {
                    output: String::new(),
                    usage: total_usage,
                    failure: Some(FailureKind::Other(
                        "response has no choices[0].message".to_owned(),
                    )),
                });
            };

            let tool_calls = choice_msg.get("tool_calls").and_then(Value::as_array);
            match tool_calls {
                Some(calls) if !calls.is_empty() => {
                    let mut assistant_msg = choice_msg.clone();
                    if let Some(obj) = assistant_msg.as_object_mut() {
                        obj.remove("reasoning_content");
                    }
                    messages.push(assistant_msg);

                    for call in calls {
                        let call_id = call.get("id").and_then(Value::as_str).unwrap_or("");
                        let func_name = call
                            .pointer("/function/name")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let func_args = call
                            .pointer("/function/arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}");

                        let tool_result = crate::tools::execute_tool(
                            &req.workdir,
                            req.permissions,
                            func_name,
                            func_args,
                        )
                        .await;

                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "content": tool_result,
                        }));
                    }
                }
                _ => {
                    let Some(content) = choice_msg.get("content").and_then(Value::as_str) else {
                        return Ok(RunOutcome {
                            output: String::new(),
                            usage: total_usage,
                            failure: Some(FailureKind::Other(
                                "response has no choices[0].message.content".to_owned(),
                            )),
                        });
                    };
                    return Ok(RunOutcome {
                        output: content.to_owned(),
                        usage: total_usage,
                        failure: None,
                    });
                }
            }
        }
    }
}

#[async_trait]
impl Backend for OpenAiBackend {
    async fn run(&self, req: &RunRequest) -> Result<RunOutcome, BackendError> {
        if !self.agent_tools {
            let body = self.request_body(req);
            let resp_text = match self.send_chat_request(&body).await {
                Ok(text) => text,
                Err(kind) => return Ok(failed(kind, overseer_core::Usage::default())),
            };
            return Ok(parse_completion(&resp_text));
        }

        self.run_agent_loop(req).await
    }
}

fn parse_completion(body: &str) -> RunOutcome {
    let value: Value = match serde_json::from_str(body) {
        Ok(value) => value,
        Err(_) => {
            return failed(
                FailureKind::Other(format!(
                    "response is not JSON: {}",
                    tail(body, BODY_EXCERPT_CHARS)
                )),
                overseer_core::Usage::default(),
            );
        }
    };
    let Some(content) = value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
    else {
        return failed(
            FailureKind::Other("response has no choices[0].message.content".to_owned()),
            overseer_core::Usage::default(),
        );
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

const ELIDED_TOOL_OUTPUT: &str = "[earlier tool output elided to fit the context budget]";

fn total_message_chars(messages: &[Value]) -> usize {
    messages
        .iter()
        .filter_map(|m| m.get("content").and_then(Value::as_str))
        .map(|s| s.chars().count())
        .sum()
}

fn enforce_history_budget(messages: &mut [Value], budget: usize) -> Result<usize, FailureKind> {
    let mut total = total_message_chars(messages);
    let mut elided: usize = 0;
    if total > budget
        && let Some(last_ast) = messages
            .iter()
            .rposition(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
    {
        for msg in messages.get_mut(..last_ast).unwrap_or(&mut []) {
            if total <= budget {
                break;
            }
            if msg.get("role").and_then(Value::as_str) == Some("tool")
                && msg.get("content").and_then(Value::as_str) != Some(ELIDED_TOOL_OUTPUT)
                && let Some(obj) = msg.as_object_mut()
            {
                let old_len = obj
                    .get("content")
                    .and_then(Value::as_str)
                    .map_or(0, |s| s.chars().count());
                obj.insert("content".to_owned(), json!(ELIDED_TOOL_OUTPUT));
                elided = elided.saturating_add(1);
                total = total
                    .saturating_sub(old_len)
                    .saturating_add(ELIDED_TOOL_OUTPUT.chars().count());
            }
        }
    }
    if elided > 0 {
        tracing::info!(elided, "elided earlier tool output to fit context budget");
    }
    if total > budget {
        return Err(FailureKind::Other(
            "context budget exceeded after eliding tool output".to_owned(),
        ));
    }
    Ok(elided)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

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
            agent_tools: false,
            max_steps: 40,
            skills: Vec::new(),
            gpu: false,
            context_chars: 48000,
            input_tokens: 0,
            output_tokens: 0,
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

    fn agent_profile(base_url: &str, max_steps: u32, permissions: Permissions) -> Profile {
        let base = if base_url.ends_with("/v1") {
            base_url.to_owned()
        } else {
            format!("{base_url}/v1")
        };
        Profile {
            name: "local-agent".to_owned(),
            backend: BackendKind::Openai,
            model: Some("local".to_owned()),
            base_url: Some(base),
            api_key_env: None,
            command: None,
            prompt_flag: None,
            prompt_stdin: false,
            role_prompt: None,
            fallback: Vec::new(),
            permissions,
            timeout_secs: 5,
            metered: false,
            permission_args: Default::default(),
            env: Default::default(),
            agent_tools: true,
            max_steps,
            skills: Vec::new(),
            gpu: false,
            context_chars: 48000,
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    fn tool_call_completion(
        id: &str,
        name: &str,
        arguments: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
    ) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments
                        }
                    }]
                }
            }],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens
            }
        }))
    }

    fn final_completion(
        content: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
    ) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": content
                }
            }],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens
            }
        }))
    }

    async fn mount_tool_call(server: &MockServer, id: &str, tool: &str, args: &str) {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(tool_call_completion(id, tool, args, 10, 5))
            .up_to_n_times(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn tool_loop_write_file_then_final_answer() {
        let server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let workdir = temp.path();

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(tool_call_completion(
                "call_1",
                "write_file",
                r#"{"path": "hello.txt", "content": "world"}"#,
                10,
                5,
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(final_completion("created hello.txt", 15, 6))
            .mount(&server)
            .await;

        let backend = OpenAiBackend::from_parts(
            &agent_profile(&server.uri(), 40, Permissions::WriteWorkdir),
            ApiKey::None,
        )
        .unwrap();

        let req = RunRequest {
            system: Some("You are a helper.".to_owned()),
            prompt: "write hello.txt".to_owned(),
            workdir: workdir.to_path_buf(),
            permissions: Permissions::WriteWorkdir,
        };

        let outcome = backend.run(&req).await.unwrap();
        assert_eq!(outcome.failure, None);
        assert_eq!(outcome.output, "created hello.txt");
        assert_eq!(outcome.usage.input_tokens, 25);
        assert_eq!(outcome.usage.output_tokens, 11);
        assert_eq!(
            std::fs::read_to_string(workdir.join("hello.txt")).unwrap(),
            "world"
        );
    }

    #[tokio::test]
    async fn tool_loop_path_escape_returns_error_tool_message_and_continues() {
        let server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(tool_call_completion(
                "call_esc",
                "write_file",
                r#"{"path": "../escape.txt", "content": "evil"}"#,
                10,
                5,
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(final_completion("recovered from error", 20, 8))
            .mount(&server)
            .await;

        let backend = OpenAiBackend::from_parts(
            &agent_profile(&server.uri(), 40, Permissions::WriteWorkdir),
            ApiKey::None,
        )
        .unwrap();

        let req = RunRequest {
            system: None,
            prompt: "try escape".to_owned(),
            workdir: temp.path().to_path_buf(),
            permissions: Permissions::WriteWorkdir,
        };

        let outcome = backend.run(&req).await.unwrap();
        assert_eq!(outcome.failure, None);
        assert_eq!(outcome.output, "recovered from error");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let Some(second) = requests.get(1) else {
            panic!("expected second request");
        };
        let req2: Value = serde_json::from_slice(&second.body).unwrap();
        let tool_msg = req2
            .pointer("/messages/3")
            .expect("should have tool message at index 3");
        assert_eq!(tool_msg.get("role").unwrap(), "tool");
        assert_eq!(tool_msg.get("tool_call_id").unwrap(), "call_esc");
        let content = tool_msg.get("content").unwrap().as_str().unwrap();
        assert!(content.starts_with("error:"), "{content}");
    }

    #[tokio::test]
    async fn tool_loop_unknown_tool_returns_error_and_continues() {
        let server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(tool_call_completion(
                "call_unk",
                "nonexistent_tool",
                "{}",
                10,
                5,
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(final_completion("handled unknown tool", 15, 6))
            .mount(&server)
            .await;

        let backend = OpenAiBackend::from_parts(
            &agent_profile(&server.uri(), 40, Permissions::WriteWorkdir),
            ApiKey::None,
        )
        .unwrap();

        let req = RunRequest {
            system: None,
            prompt: "do something".to_owned(),
            workdir: temp.path().to_path_buf(),
            permissions: Permissions::WriteWorkdir,
        };

        let outcome = backend.run(&req).await.unwrap();
        assert_eq!(outcome.failure, None);
        assert_eq!(outcome.output, "handled unknown tool");

        let requests = server.received_requests().await.unwrap();
        let Some(second) = requests.get(1) else {
            panic!("expected second request");
        };
        let req2: Value = serde_json::from_slice(&second.body).unwrap();
        let tool_msg = req2
            .pointer("/messages/3")
            .expect("should have tool message at index 3");
        assert_eq!(tool_msg.get("role").unwrap(), "tool");
        let content = tool_msg.get("content").unwrap().as_str().unwrap();
        assert!(content.starts_with("error: unknown tool"), "{content}");
    }

    #[tokio::test]
    async fn tool_loop_bad_json_args_returns_error_and_continues() {
        let server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(tool_call_completion(
                "call_bad",
                "write_file",
                "{broken json",
                10,
                5,
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(final_completion("handled bad json", 15, 6))
            .mount(&server)
            .await;

        let backend = OpenAiBackend::from_parts(
            &agent_profile(&server.uri(), 40, Permissions::WriteWorkdir),
            ApiKey::None,
        )
        .unwrap();

        let req = RunRequest {
            system: None,
            prompt: "bad json test".to_owned(),
            workdir: temp.path().to_path_buf(),
            permissions: Permissions::WriteWorkdir,
        };

        let outcome = backend.run(&req).await.unwrap();
        assert_eq!(outcome.failure, None);
        assert_eq!(outcome.output, "handled bad json");

        let requests = server.received_requests().await.unwrap();
        let Some(second) = requests.get(1) else {
            panic!("expected second request");
        };
        let req2: Value = serde_json::from_slice(&second.body).unwrap();
        let tool_msg = req2
            .pointer("/messages/3")
            .expect("should have tool message at index 3");
        assert_eq!(tool_msg.get("role").unwrap(), "tool");
        let content = tool_msg.get("content").unwrap().as_str().unwrap();
        assert!(content.starts_with("error: invalid arguments"), "{content}");
    }

    #[tokio::test]
    async fn tool_loop_read_only_permission_hides_write_tools() {
        let server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(final_completion("read only answer", 10, 5))
            .mount(&server)
            .await;

        let backend = OpenAiBackend::from_parts(
            &agent_profile(&server.uri(), 40, Permissions::Read),
            ApiKey::None,
        )
        .unwrap();

        let req = RunRequest {
            system: None,
            prompt: "inspect repo".to_owned(),
            workdir: temp.path().to_path_buf(),
            permissions: Permissions::Read,
        };

        let outcome = backend.run(&req).await.unwrap();
        assert_eq!(outcome.output, "read only answer");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let Some(first) = requests.first() else {
            panic!("expected request");
        };
        let req1: Value = serde_json::from_slice(&first.body).unwrap();
        let tools = req1.get("tools").unwrap().as_array().unwrap();
        let tool_names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.pointer("/function/name").and_then(Value::as_str))
            .collect();
        assert_eq!(tool_names, ["list_dir", "read_file"]);
    }

    #[tokio::test]
    async fn tool_loop_max_steps_exceeded_fails() {
        let server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(tool_call_completion(
                "call_loop",
                "list_dir",
                r#"{"path": "."}"#,
                10,
                5,
            ))
            .mount(&server)
            .await;

        let backend = OpenAiBackend::from_parts(
            &agent_profile(&server.uri(), 2, Permissions::WriteWorkdir),
            ApiKey::None,
        )
        .unwrap();

        let req = RunRequest {
            system: None,
            prompt: "infinite loop".to_owned(),
            workdir: temp.path().to_path_buf(),
            permissions: Permissions::WriteWorkdir,
        };

        let outcome = backend.run(&req).await.unwrap();
        assert_eq!(
            outcome.failure,
            Some(FailureKind::Other("exceeded max_steps".to_owned()))
        );
        assert_eq!(outcome.usage.input_tokens, 20);
        assert_eq!(outcome.usage.output_tokens, 10);
    }

    #[tokio::test]
    async fn tool_loop_usage_summed_across_steps() {
        let server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(tool_call_completion(
                "call_step1",
                "list_dir",
                r#"{"path": "."}"#,
                100,
                25,
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(final_completion("finished", 150, 40))
            .mount(&server)
            .await;

        let backend = OpenAiBackend::from_parts(
            &agent_profile(&server.uri(), 40, Permissions::WriteWorkdir),
            ApiKey::None,
        )
        .unwrap();

        let req = RunRequest {
            system: None,
            prompt: "check usage".to_owned(),
            workdir: temp.path().to_path_buf(),
            permissions: Permissions::WriteWorkdir,
        };

        let outcome = backend.run(&req).await.unwrap();
        assert_eq!(outcome.failure, None);
        assert_eq!(outcome.usage.input_tokens, 250);
        assert_eq!(outcome.usage.output_tokens, 65);
    }

    #[tokio::test]
    async fn tool_loop_http_429_mid_loop_classified_quota_exhausted() {
        let server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(tool_call_completion(
                "call_1",
                "list_dir",
                r#"{"path": "."}"#,
                50,
                10,
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limit reached"))
            .mount(&server)
            .await;

        let backend = OpenAiBackend::from_parts(
            &agent_profile(&server.uri(), 40, Permissions::WriteWorkdir),
            ApiKey::None,
        )
        .unwrap();

        let req = RunRequest {
            system: None,
            prompt: "rate limit mid loop".to_owned(),
            workdir: temp.path().to_path_buf(),
            permissions: Permissions::WriteWorkdir,
        };

        let outcome = backend.run(&req).await.unwrap();
        assert_eq!(outcome.failure, Some(FailureKind::QuotaExhausted));
        assert_eq!(outcome.usage.input_tokens, 50);
        assert_eq!(outcome.usage.output_tokens, 10);
    }

    #[tokio::test]
    async fn history_budget_elides_oldest_tool_results() {
        let server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let workdir = temp.path();

        let content = ("x".repeat(60) + "\n").repeat(300);
        for name in ["f1.txt", "f2.txt", "f3.txt"] {
            std::fs::write(workdir.join(name), &content).unwrap();
        }

        mount_tool_call(&server, "call_1", "read_file", r#"{"path": "f1.txt"}"#).await;
        mount_tool_call(&server, "call_2", "read_file", r#"{"path": "f2.txt"}"#).await;
        mount_tool_call(&server, "call_3", "read_file", r#"{"path": "f3.txt"}"#).await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(final_completion("finished reading", 15, 6))
            .mount(&server)
            .await;

        let backend = OpenAiBackend::from_parts(
            &agent_profile(&server.uri(), 40, Permissions::WriteWorkdir),
            ApiKey::None,
        )
        .unwrap();

        let req = RunRequest {
            system: Some("You are a helper.".to_owned()),
            prompt: "read files".to_owned(),
            workdir: workdir.to_path_buf(),
            permissions: Permissions::WriteWorkdir,
        };

        let outcome = backend.run(&req).await.unwrap();
        assert_eq!(outcome.failure, None);
        assert_eq!(outcome.output, "finished reading");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 4);
        let req4: Value = serde_json::from_slice(&requests[3].body).unwrap();
        let messages = req4.get("messages").unwrap().as_array().unwrap();

        let total_chars: usize = messages
            .iter()
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .map(|s| s.chars().count())
            .sum();
        assert!(
            total_chars <= 48000,
            "total chars {total_chars} exceeds 48000"
        );

        assert_eq!(messages[1].get("role").unwrap(), "user");
        assert_eq!(messages[1].get("content").unwrap(), "read files");

        let tool1 = messages
            .iter()
            .find(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_1"))
            .unwrap();
        assert_eq!(tool1.get("role").unwrap(), "tool");
        assert_eq!(
            tool1.get("content").unwrap(),
            "[earlier tool output elided to fit the context budget]"
        );

        let tool3 = messages
            .iter()
            .find(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_3"))
            .unwrap();
        assert_eq!(tool3.get("role").unwrap(), "tool");
        let content3 = tool3.get("content").unwrap().as_str().unwrap();
        assert!(content3.contains("1\t") && !content3.contains("elided"));
    }

    #[tokio::test]
    async fn elision_keeps_tool_call_ids() {
        let server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let workdir = temp.path();

        let content = ("y".repeat(60) + "\n").repeat(300);
        for name in ["a.txt", "b.txt"] {
            std::fs::write(workdir.join(name), &content).unwrap();
        }

        mount_tool_call(
            &server,
            "call_keep_id_1",
            "read_file",
            r#"{"path": "a.txt"}"#,
        )
        .await;
        mount_tool_call(
            &server,
            "call_keep_id_2",
            "read_file",
            r#"{"path": "b.txt"}"#,
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(final_completion("done reading", 15, 6))
            .mount(&server)
            .await;

        let mut prof = agent_profile(&server.uri(), 40, Permissions::WriteWorkdir);
        prof.context_chars = 25000;
        let backend = OpenAiBackend::from_parts(&prof, ApiKey::None).unwrap();

        let req = RunRequest {
            system: None,
            prompt: "read a and b".to_owned(),
            workdir: workdir.to_path_buf(),
            permissions: Permissions::WriteWorkdir,
        };

        let outcome = backend.run(&req).await.unwrap();
        assert_eq!(outcome.failure, None);

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3);
        let req3: Value = serde_json::from_slice(&requests[2].body).unwrap();
        let messages = req3.get("messages").unwrap().as_array().unwrap();

        let tool1 = messages
            .iter()
            .find(|m| m.get("tool_call_id").and_then(Value::as_str) == Some("call_keep_id_1"))
            .unwrap();
        assert_eq!(tool1.get("role").unwrap(), "tool");
        assert_eq!(tool1.get("tool_call_id").unwrap(), "call_keep_id_1");
        assert_eq!(
            tool1.get("content").unwrap(),
            "[earlier tool output elided to fit the context budget]"
        );
    }

    #[tokio::test]
    async fn budget_error_when_still_too_large() {
        let server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        let mut prof = agent_profile(&server.uri(), 40, Permissions::WriteWorkdir);
        prof.context_chars = 50;
        let backend = OpenAiBackend::from_parts(&prof, ApiKey::None).unwrap();

        let req = RunRequest {
            system: Some("Detailed assistant instructions.".to_owned()),
            prompt: "this task description alone is longer than 50 characters".to_owned(),
            workdir: temp.path().to_path_buf(),
            permissions: Permissions::WriteWorkdir,
        };

        let outcome = backend.run(&req).await.unwrap();
        assert_eq!(
            outcome.failure,
            Some(FailureKind::Other(
                "context budget exceeded after eliding tool output".to_owned()
            ))
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 0);
    }
}
