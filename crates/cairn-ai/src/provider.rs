//! The provider-agnostic LLM abstraction.
//!
//! A [`LlmProvider`] turns a request (system prompt + messages + available tools) into either text
//! or a tool call. Concrete cloud (Claude) and local (Ollama) providers are added behind a feature
//! in a later step; this core defines the trait and a [`MockProvider`] so the agent logic is fully
//! testable offline. Current default cloud models (for the eventual Claude provider): Opus 4.8
//! (`claude-opus-4-8`), Sonnet 4.6 (`claude-sonnet-4-6`), Haiku 4.5 (`claude-haiku-4-5-20251001`).

use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::Mutex;

/// The default cloud model id (used by the eventual Claude provider).
pub const DEFAULT_CLOUD_MODEL: &str = "claude-opus-4-8";

/// A conversation role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// System / policy message.
    System,
    /// User message.
    User,
    /// Assistant message.
    Assistant,
}

/// A single message in a request.
#[derive(Debug, Clone)]
pub struct Message {
    /// The role.
    pub role: Role,
    /// The text content.
    pub text: String,
}

/// A tool definition advertised to the model.
#[derive(Debug, Clone)]
pub struct ToolDef {
    /// Tool name (must be in the closed set, see the `tools` module).
    pub name: String,
    /// Human description for the model.
    pub description: String,
}

/// A request to the model.
#[derive(Debug, Clone, Default)]
pub struct LlmRequest {
    /// Optional system prompt.
    pub system: Option<String>,
    /// Conversation messages.
    pub messages: Vec<Message>,
    /// Tools the model may call.
    pub tools: Vec<ToolDef>,
}

/// Token accounting for a response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Input tokens.
    pub input_tokens: u32,
    /// Output tokens.
    pub output_tokens: u32,
}

/// A model response: either free text or a tool call.
#[derive(Debug, Clone)]
pub enum LlmResponse {
    /// Plain text.
    Text(String),
    /// A request to call a tool with the given JSON input.
    ToolCall {
        /// Tool name.
        name: String,
        /// Tool input.
        input: serde_json::Value,
    },
}

/// Provider errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProviderError {
    /// A transport/HTTP failure (secret-free message).
    #[error("provider transport error: {0}")]
    Transport(String),
    /// The provider returned an unparseable or empty response.
    #[error("invalid provider response")]
    InvalidResponse,
}

/// The provider abstraction. Implemented by cloud, local, and mock providers.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Complete one turn.
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, ProviderError>;
    /// The model identifier.
    fn model_id(&self) -> &str;
}

/// A scripted provider for tests: returns queued responses in order.
pub struct MockProvider {
    model: String,
    responses: Mutex<VecDeque<LlmResponse>>,
}

impl MockProvider {
    /// Create a mock that will return `responses` in order.
    #[must_use]
    pub fn new(responses: Vec<LlmResponse>) -> Self {
        Self {
            model: "mock".to_owned(),
            responses: Mutex::new(responses.into()),
        }
    }

    /// Convenience: a mock that returns a single `propose_plan` tool call.
    #[must_use]
    pub fn proposing(plan: serde_json::Value) -> Self {
        Self::new(vec![LlmResponse::ToolCall {
            name: "propose_plan".to_owned(),
            input: plan,
        }])
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    async fn complete(&self, _req: LlmRequest) -> Result<LlmResponse, ProviderError> {
        self.responses
            .lock()
            .expect("mock provider mutex")
            .pop_front()
            .ok_or(ProviderError::InvalidResponse)
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_returns_scripted_responses_in_order() {
        let p = MockProvider::new(vec![
            LlmResponse::Text("hello".into()),
            LlmResponse::ToolCall {
                name: "list".into(),
                input: serde_json::json!({"path": "/"}),
            },
        ]);
        assert!(matches!(
            p.complete(LlmRequest::default()).await.unwrap(),
            LlmResponse::Text(t) if t == "hello"
        ));
        assert!(matches!(
            p.complete(LlmRequest::default()).await.unwrap(),
            LlmResponse::ToolCall { name, .. } if name == "list"
        ));
        // Exhausted.
        assert!(p.complete(LlmRequest::default()).await.is_err());
    }
}
