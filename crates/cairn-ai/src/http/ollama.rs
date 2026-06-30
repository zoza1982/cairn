//! Live Ollama provider for local models. Behind the `http` feature.
//!
//! Maps an [`LlmRequest`] onto `POST /api/chat` (non-streaming). HTTP/transport failures and non-2xx
//! statuses map to [`ProviderError::Transport`] (secret-free message); an unparseable success body maps
//! to [`ProviderError::InvalidResponse`].

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::provider::{LlmProvider, LlmRequest, LlmResponse, ProviderError, Role, ToolSupport};

/// Default Ollama endpoint (the daemon's local HTTP server).
const DEFAULT_BASE_URL: &str = "http://localhost:11434";

/// A live [`LlmProvider`] backed by a local Ollama daemon (`POST /api/chat`, non-streaming).
///
/// # Tool-calling tier
/// Ollama advertises [`ToolSupport::JsonSchema`], **not** `Native`. Ollama's native `tools` field is
/// honored only by a subset of pulled models and behaves inconsistently across the wide zoo a user may
/// run, so the agent degrades to a prompted bare-JSON object that any instruction-following model can
/// produce (see the `degrade` module). This is the robust default: it never depends on a model's
/// function-calling support. A capable model that *does* return a native `tool_calls` entry is still
/// surfaced faithfully as an [`LlmResponse::ToolCall`].
///
/// Carries no secret (a local daemon needs no API key), so a derived `Debug` is safe.
#[derive(Debug)]
pub struct OllamaProvider {
    client: reqwest::Client,
    model: String,
    base_url: String,
}

impl OllamaProvider {
    /// Create a provider against the default local daemon (`http://localhost:11434`).
    ///
    /// Give the injected `reqwest::Client` a request timeout — the default client has none, so a
    /// frozen daemon would hang the agent turn forever.
    #[must_use]
    pub fn new(client: reqwest::Client, model: impl Into<String>) -> Self {
        Self::with_base_url(client, model, DEFAULT_BASE_URL)
    }

    /// Create a provider against an explicit base URL (a remote daemon or a test server). A trailing
    /// slash is trimmed so the `/api/chat` path joins cleanly.
    #[must_use]
    pub fn with_base_url(
        client: reqwest::Client,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            client,
            model: model.into(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }
}

/// The wire role string Ollama expects for each [`Role`].
fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

/// Build the `/api/chat` request body. `req.system` (when present) leads as a `system` message;
/// `stream` is `false` so the daemon returns one JSON object. Tools are not advertised — the
/// `JsonSchema` tier folds the plan instruction into the prompt instead (see the type docs).
fn build_body(model: &str, req: LlmRequest) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(s) = req.system.filter(|s| !s.is_empty()) {
        messages.push(json!({"role": "system", "content": s}));
    }
    // Unlike `AnthropicProvider` (which folds `Role::System` into a top-level `system` field), Ollama's
    // /api/chat takes system turns inline as `system`-role messages, so we pass them straight through.
    for m in req.messages {
        messages.push(json!({"role": role_str(m.role), "content": m.text}));
    }
    json!({"model": model, "messages": messages, "stream": false})
}

/// A native tool call in an Ollama chat message.
#[derive(Deserialize)]
struct ToolCall {
    function: ToolFunction,
}

/// The function name + parsed arguments of a native tool call.
#[derive(Deserialize)]
struct ToolFunction {
    name: String,
    #[serde(default)]
    arguments: Value,
}

/// The assistant message in an `/api/chat` response. `Option` (not `#[serde(default)]` on the bare
/// type) so an explicit JSON `null` — which a proxy may emit for an omitted field — coalesces to
/// "absent" instead of failing the whole parse.
#[derive(Deserialize)]
struct ChatMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
}

/// The subset of the `/api/chat` response we consume.
#[derive(Deserialize)]
struct ChatResponse {
    message: Option<ChatMessage>,
}

/// Map a parsed response to [`LlmResponse`]: a native `tool_calls` entry (if any) wins, otherwise the
/// message `content`. A missing message or empty content is [`ProviderError::InvalidResponse`].
fn parse_response(resp: ChatResponse) -> Result<LlmResponse, ProviderError> {
    let Some(message) = resp.message else {
        return Err(ProviderError::InvalidResponse);
    };
    if let Some(call) = message.tool_calls.unwrap_or_default().into_iter().next() {
        return Ok(LlmResponse::ToolCall {
            name: call.function.name,
            input: call.function.arguments,
        });
    }
    match message.content {
        Some(content) if !content.is_empty() => Ok(LlmResponse::Text(content)),
        _ => Err(ProviderError::InvalidResponse),
    }
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, ProviderError> {
        let body = build_body(&self.model, req);
        let resp = self
            .client
            .post(format!("{}/api/chat", self.base_url))
            .json(&body)
            .send()
            .await
            .map_err(|e| super::transport_error("Ollama", &e))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(ProviderError::Transport(format!(
                "Ollama returned HTTP {}",
                status.as_u16()
            )));
        }

        let parsed: ChatResponse = resp
            .json()
            .await
            .map_err(|_| ProviderError::InvalidResponse)?;
        parse_response(parsed)
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn tool_support(&self) -> ToolSupport {
        ToolSupport::JsonSchema
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn provider(server: &MockServer) -> OllamaProvider {
        OllamaProvider::with_base_url(reqwest::Client::new(), "llama3.1", server.uri())
    }

    fn user_request() -> LlmRequest {
        LlmRequest {
            system: Some("be terse".to_owned()),
            messages: vec![Message {
                role: Role::User,
                text: "hello".to_owned(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn advertises_json_schema_tier() {
        let p = OllamaProvider::new(reqwest::Client::new(), "llama3.1");
        assert_eq!(p.tool_support(), ToolSupport::JsonSchema);
        assert_eq!(p.model_id(), "llama3.1");
    }

    #[tokio::test]
    async fn sends_correct_path_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(header("content-type", "application/json"))
            .and(body_partial_json(json!({
                "model": "llama3.1",
                "stream": false,
                "messages": [
                    {"role": "system", "content": "be terse"},
                    {"role": "user", "content": "hello"}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "hi"}, "done": true
            })))
            .expect(1)
            .mount(&server)
            .await;

        assert!(matches!(
            provider(&server).complete(user_request()).await.unwrap(),
            LlmResponse::Text(t) if t == "hi"
        ));
    }

    #[tokio::test]
    async fn content_becomes_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "{\"summary\": \"x\", \"steps\": []}"}
            })))
            .mount(&server)
            .await;

        assert!(matches!(
            provider(&server).complete(user_request()).await.unwrap(),
            LlmResponse::Text(t) if t.contains("summary")
        ));
    }

    #[tokio::test]
    async fn native_tool_call_becomes_tool_call() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [
                        {"function": {"name": "propose_plan", "arguments": {"summary": "x"}}}
                    ]
                }
            })))
            .mount(&server)
            .await;

        match provider(&server).complete(user_request()).await.unwrap() {
            LlmResponse::ToolCall { name, input } => {
                assert_eq!(name, "propose_plan");
                assert_eq!(input["summary"], "x");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_500_maps_to_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        assert!(matches!(
            provider(&server).complete(user_request()).await,
            Err(ProviderError::Transport(_))
        ));
    }

    #[tokio::test]
    async fn malformed_success_body_maps_to_invalid_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        assert!(matches!(
            provider(&server).complete(user_request()).await,
            Err(ProviderError::InvalidResponse)
        ));
    }

    #[tokio::test]
    async fn missing_message_maps_to_invalid_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"done": true})))
            .mount(&server)
            .await;

        assert!(matches!(
            provider(&server).complete(user_request()).await,
            Err(ProviderError::InvalidResponse)
        ));
    }

    #[tokio::test]
    async fn null_tool_calls_and_present_content_yields_text() {
        // An explicit `null` for `tool_calls` must not fail the parse — it means "no tool calls".
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "hi", "tool_calls": null}
            })))
            .mount(&server)
            .await;

        assert!(matches!(
            provider(&server).complete(user_request()).await.unwrap(),
            LlmResponse::Text(t) if t == "hi"
        ));
    }

    #[tokio::test]
    async fn role_system_message_passes_through_inline() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({
                "messages": [
                    {"role": "system", "content": "be terse"},
                    {"role": "system", "content": "amendment"},
                    {"role": "user", "content": "go"}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "ok"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let req = LlmRequest {
            system: Some("be terse".to_owned()),
            messages: vec![
                Message {
                    role: Role::System,
                    text: "amendment".to_owned(),
                },
                Message {
                    role: Role::User,
                    text: "go".to_owned(),
                },
            ],
            ..Default::default()
        };
        assert!(matches!(
            provider(&server).complete(req).await.unwrap(),
            LlmResponse::Text(t) if t == "ok"
        ));
    }

    #[test]
    fn trailing_slash_is_trimmed() {
        let p =
            OllamaProvider::with_base_url(reqwest::Client::new(), "llama3.1", "http://host:11434/");
        assert_eq!(p.base_url, "http://host:11434");
    }

    /// Live smoke test, opt-in: requires `CAIRN_IT_AI` plus a reachable local daemon; the model is
    /// taken from `OLLAMA_MODEL` (defaulting to `llama3.1`). Never runs in the default suite.
    #[tokio::test]
    #[ignore = "live: set CAIRN_IT_AI=1 and run a local Ollama daemon"]
    async fn live_smoke() {
        if std::env::var("CAIRN_IT_AI").is_err() {
            return;
        }
        let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "llama3.1".to_owned());
        let provider = OllamaProvider::new(reqwest::Client::new(), model);
        let req = LlmRequest {
            messages: vec![Message {
                role: Role::User,
                text: "Reply with the single word: ok".to_owned(),
            }],
            ..Default::default()
        };
        let resp = provider.complete(req).await.expect("live call failed");
        assert!(matches!(
            resp,
            LlmResponse::Text(_) | LlmResponse::ToolCall { .. }
        ));
    }
}
