//! Live HTTP LLM providers (behind the non-default `http` feature): Anthropic (Claude) and Ollama.
//!
//! These are the concrete network transports for the provider-agnostic
//! [`LlmProvider`](crate::LlmProvider) trait; the offline `MockProvider` and the tool-degradation logic
//! remain the default, hermetic path. `cairn-ai` still depends only on `cairn-broker-api` — never the
//! vault or secrets — so the `api_key` is an ordinary credential (held in `zeroize::Zeroizing`, wiped
//! on drop), never a `cairn-secrets` `SecretString`. Errors are secret-free and never embed the key.
//! Both providers are non-streaming.

mod anthropic;
mod ollama;

pub use anthropic::AnthropicProvider;
pub use ollama::OllamaProvider;

use crate::provider::ProviderError;

/// Map a `reqwest` transport failure to a secret-free [`ProviderError::Transport`]. Only the failure
/// *kind* is surfaced (timeout / connect / …), never the URL, headers, or body — so the `api_key` can
/// never reach a log line, while production still gets enough signal to tell a timeout from a refused
/// connection. The full `reqwest::Error` is deliberately discarded.
pub(super) fn transport_error(provider: &str, e: &reqwest::Error) -> ProviderError {
    let kind = if e.is_timeout() {
        "timed out"
    } else if e.is_connect() {
        "connection failed"
    } else if e.is_request() {
        "request error"
    } else if e.is_body() || e.is_decode() {
        "response read error"
    } else {
        "transport failure"
    };
    ProviderError::Transport(format!("{provider}: {kind}"))
}
