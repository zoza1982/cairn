//! Live Anthropic (Claude) Messages-API provider. Behind the `http` feature.
//!
//! Maps an [`LlmRequest`] onto `POST /v1/messages` (headers `x-api-key` + `anthropic-version`), parses
//! the first `tool_use` content block into an [`LlmResponse::ToolCall`], otherwise concatenates the
//! `text` blocks into [`LlmResponse::Text`]. Non-streaming. HTTP/transport failures and non-2xx
//! statuses map to [`ProviderError::Transport`] with a secret-free message (never the body or the
//! `api_key`); an unparseable success body maps to [`ProviderError::InvalidResponse`].

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use zeroize::Zeroizing;

use crate::provider::{LlmProvider, LlmRequest, LlmResponse, ProviderError, Role};
use crate::tools::input_schema_for;

/// Default public Anthropic API base URL.
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
/// The required `anthropic-version` header value (the Messages API is stable at this version).
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Output-token ceiling sent on every request (the Messages API requires `max_tokens`).
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// A live [`LlmProvider`] backed by Anthropic's Messages API (Claude).
///
/// Advertises tools natively ([`ToolSupport::Native`](crate::ToolSupport), the trait default). The
/// `reqwest::Client` is injected so the caller controls timeouts/proxies and construction stays
/// panic-free; the `base_url` is configurable for proxies, gateways, or tests.
///
/// The `api_key` is a plain credential (no `SecretString`, to keep the vault out of this crate's
/// dependency graph) but is held in [`Zeroizing`] so its bytes are wiped on drop, is never embedded in
/// an error, and is redacted by the hand-written [`Debug`] impl.
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: Zeroizing<String>,
    model: String,
    base_url: String,
}

// Hand-written so an accidental `#[derive(Debug)]` can never print the key, and so the type stays
// usable in `{:?}` contexts. The key is redacted unconditionally.
impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

impl AnthropicProvider {
    /// Create a provider against the public Anthropic endpoint.
    ///
    /// Give the injected `reqwest::Client` a request timeout — the default client has none, so a
    /// stalled connection would hang the agent turn forever.
    #[must_use]
    pub fn new(
        client: reqwest::Client,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self::with_base_url(client, api_key, model, DEFAULT_BASE_URL)
    }

    /// Create a provider against an explicit base URL (a proxy, gateway, or test server). A trailing
    /// slash is trimmed so the `/v1/messages` path joins cleanly.
    #[must_use]
    pub fn with_base_url(
        client: reqwest::Client,
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            client,
            api_key: Zeroizing::new(api_key.into()),
            model: model.into(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }
}

/// Build the Messages-API request body. Any `Role::System` message — and `req.system` — folds into the
/// top-level `system` field; `system`/`tools` are omitted when empty.
fn build_body(model: &str, req: LlmRequest) -> Value {
    let mut system_parts: Vec<String> = Vec::new();
    if let Some(s) = req.system.filter(|s| !s.is_empty()) {
        system_parts.push(s);
    }

    let mut messages: Vec<Value> = Vec::new();
    for m in req.messages {
        match m.role {
            Role::System => system_parts.push(m.text),
            Role::User => messages.push(json!({"role": "user", "content": m.text})),
            Role::Assistant => messages.push(json!({"role": "assistant", "content": m.text})),
        }
    }

    let tools: Vec<Value> = req
        .tools
        .into_iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": input_schema_for(&t.name)
                    .unwrap_or_else(|| json!({"type": "object"})),
            })
        })
        .collect();

    let mut body = json!({
        "model": model,
        "max_tokens": DEFAULT_MAX_TOKENS,
        "messages": messages,
    });
    if !system_parts.is_empty() {
        body["system"] = json!(system_parts.join("\n\n"));
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    body
}

/// One content block in a Messages-API response. Unknown block types (e.g. `thinking`) are ignored.
#[derive(Deserialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse { name: String, input: Value },
    #[serde(other)]
    Other,
}

/// The subset of the Messages-API response we consume. `stop_reason` lets us reject a reply that was
/// cut off at `max_tokens` rather than surfacing a truncated plan as if it were complete.
#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
}

/// Map a parsed response to [`LlmResponse`]: the first `tool_use` wins, otherwise the concatenated
/// `text`. A contentless reply is [`ProviderError::InvalidResponse`].
fn parse_response(resp: MessagesResponse) -> Result<LlmResponse, ProviderError> {
    let mut text = String::new();
    for block in resp.content {
        match block {
            ContentBlock::ToolUse { name, input } => {
                return Ok(LlmResponse::ToolCall { name, input })
            }
            ContentBlock::Text { text: t } => text.push_str(&t),
            ContentBlock::Other => {}
        }
    }
    if text.is_empty() {
        Err(ProviderError::InvalidResponse)
    } else {
        Ok(LlmResponse::Text(text))
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, ProviderError> {
        let body = build_body(&self.model, req);
        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", self.api_key.as_str())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await
            .map_err(|e| super::transport_error("Anthropic", &e))?;

        let status = resp.status();
        if !status.is_success() {
            // Status code only — never the response body or the api_key.
            return Err(ProviderError::Transport(format!(
                "Anthropic API returned HTTP {}",
                status.as_u16()
            )));
        }

        let parsed: MessagesResponse = resp
            .json()
            .await
            .map_err(|_| ProviderError::InvalidResponse)?;
        // A reply cut off at the output cap is incomplete (a truncated plan/answer), not usable.
        if parsed.stop_reason.as_deref() == Some("max_tokens") {
            return Err(ProviderError::InvalidResponse);
        }
        parse_response(parsed)
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn provider(server: &MockServer) -> AnthropicProvider {
        AnthropicProvider::with_base_url(
            reqwest::Client::new(),
            "test-key",
            "claude-opus-4-8",
            server.uri(),
        )
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

    #[tokio::test]
    async fn sends_correct_path_headers_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", ANTHROPIC_VERSION))
            // System message folds into top-level `system`; model + folded prompt present.
            .and(body_partial_json(json!({
                "model": "claude-opus-4-8",
                "system": "be terse",
                "messages": [{"role": "user", "content": "hello"}]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{"type": "text", "text": "hi"}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let resp = provider(&server).complete(user_request()).await.unwrap();
        assert!(matches!(resp, LlmResponse::Text(t) if t == "hi"));
    }

    #[tokio::test]
    async fn role_system_message_folds_into_top_level_system() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_partial_json(json!({"system": "policy text"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"content": [{"type": "text", "text": "ok"}]})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let req = LlmRequest {
            messages: vec![
                Message {
                    role: Role::System,
                    text: "policy text".to_owned(),
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

    #[tokio::test]
    async fn tool_use_block_becomes_tool_call() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [
                    {"type": "text", "text": "let me plan"},
                    {"type": "tool_use", "id": "toolu_1", "name": "propose_plan",
                     "input": {"summary": "x", "steps": []}}
                ]
            })))
            .mount(&server)
            .await;

        let resp = provider(&server).complete(user_request()).await.unwrap();
        match resp {
            LlmResponse::ToolCall { name, input } => {
                assert_eq!(name, "propose_plan");
                assert_eq!(input["summary"], "x");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn text_blocks_concatenate_into_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [
                    {"type": "text", "text": "foo "},
                    {"type": "thinking", "thinking": "ignored"},
                    {"type": "text", "text": "bar"}
                ]
            })))
            .mount(&server)
            .await;

        assert!(matches!(
            provider(&server).complete(user_request()).await.unwrap(),
            LlmResponse::Text(t) if t == "foo bar"
        ));
    }

    #[tokio::test]
    async fn http_401_maps_to_transport_without_leaking_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": {"type": "authentication_error", "message": "invalid x-api-key"}
            })))
            .mount(&server)
            .await;

        let err = provider(&server)
            .complete(user_request())
            .await
            .unwrap_err();
        match err {
            ProviderError::Transport(msg) => {
                assert!(msg.contains("401"), "msg: {msg}");
                assert!(!msg.contains("test-key"), "api key leaked: {msg}");
            }
            other => panic!("expected Transport, got {other:?}"),
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
    async fn empty_content_maps_to_invalid_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"content": []})))
            .mount(&server)
            .await;

        assert!(matches!(
            provider(&server).complete(user_request()).await,
            Err(ProviderError::InvalidResponse)
        ));
    }

    #[tokio::test]
    async fn req_system_and_role_system_both_fold_into_system() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"system": "policy\n\namendment"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"content": [{"type": "text", "text": "ok"}]})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let req = LlmRequest {
            system: Some("policy".to_owned()),
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

    #[tokio::test]
    async fn truncated_response_maps_to_invalid_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{"type": "text", "text": "partial..."}],
                "stop_reason": "max_tokens"
            })))
            .mount(&server)
            .await;

        assert!(matches!(
            provider(&server).complete(user_request()).await,
            Err(ProviderError::InvalidResponse)
        ));
    }

    #[test]
    fn trailing_slash_is_trimmed_and_tool_tier_is_native() {
        let p = AnthropicProvider::with_base_url(
            reqwest::Client::new(),
            "k",
            "claude-opus-4-8",
            "http://host:8080/",
        );
        assert_eq!(p.base_url, "http://host:8080");
        assert_eq!(p.model_id(), "claude-opus-4-8");
        // Anthropic relies on the trait default tier.
        assert_eq!(p.tool_support(), crate::ToolSupport::Native);
    }

    #[test]
    fn debug_redacts_the_api_key() {
        let p = AnthropicProvider::new(reqwest::Client::new(), "super-secret", "claude-opus-4-8");
        let dbg = format!("{p:?}");
        assert!(dbg.contains("[REDACTED]"));
        assert!(!dbg.contains("super-secret"), "key leaked via Debug: {dbg}");
    }

    /// Live smoke test, opt-in: requires `CAIRN_IT_AI` and `ANTHROPIC_API_KEY`. Never runs in the
    /// default (offline) suite.
    #[tokio::test]
    #[ignore = "live: set CAIRN_IT_AI=1 and ANTHROPIC_API_KEY"]
    async fn live_smoke() {
        if std::env::var("CAIRN_IT_AI").is_err() {
            return;
        }
        let Ok(key) = std::env::var("ANTHROPIC_API_KEY") else {
            return;
        };
        let provider =
            AnthropicProvider::new(reqwest::Client::new(), key, crate::DEFAULT_CLOUD_MODEL);
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
