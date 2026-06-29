//! Cairn's agentic AI core.
//!
//! Provider-agnostic LLM access (`provider`), the closed `tools` surface, and the
//! `plan`→confirm→execute state machine. The model only *proposes* a plan; approval and execution
//! are gated outside the model. This crate depends only on `cairn-broker-api` — the secret-free credential boundary, never on the
//! vault or backends, so the AI cannot even name a secret-returning API; a dependency-closure test
//! enforces this (RFC-0008). Concrete providers and TUI wiring are layered on later. See `docs/LLD.md`
//! §10.

mod context;
mod degrade;
mod plan;
mod provider;
mod tools;

use degrade::{decode_plan, encode_request, DegradeError};

pub use context::{
    looks_out_of_scope, wrap_untrusted, ConnectionView, PaneView, WorldSnapshot, SYSTEM_POLICY,
};
pub use plan::{
    Plan, PlanError, PlanState, PlanStep, ProposedPlan, ProposedStep, StepExecutor, StepStatus,
};
pub use provider::{
    LlmProvider, LlmRequest, LlmResponse, Message, MockProvider, ProviderError, Role, ToolDef,
    ToolSupport, Usage, DEFAULT_CLOUD_MODEL,
};
pub use tools::{allows_bulk_approve, capability_for, Capability, Reversibility, Verb, TOOLS};

/// Errors from the high-level agent flow.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AgentError {
    /// The provider failed.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// The proposed plan was invalid (e.g. unknown tool).
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// The model responded but did not propose a plan.
    #[error("model did not propose a plan")]
    NoPlan,
    /// The `propose_plan` payload could not be parsed.
    #[error("malformed plan payload")]
    BadPlan,
}

impl From<DegradeError> for AgentError {
    fn from(e: DegradeError) -> Self {
        match e {
            DegradeError::NotAPlanCall | DegradeError::NoJson => AgentError::NoPlan,
            DegradeError::BadJson => AgentError::BadPlan,
        }
    }
}

/// Ask the provider for a plan and parse it, **adapting to the provider's tool-calling tier**
/// ([`LlmProvider::tool_support`]): native tool calls, a JSON-object instruction, or a fenced-block
/// instruction for the weakest models (see the `degrade` module). Unknown tools in the plan are
/// rejected.
///
/// # Errors
/// See [`AgentError`].
pub async fn request_plan(provider: &dyn LlmProvider, req: LlmRequest) -> Result<Plan, AgentError> {
    let tier = provider.tool_support();
    let resp = provider.complete(encode_request(tier, req)).await?;
    let payload = decode_plan(tier, &resp)?;
    let proposed: ProposedPlan =
        serde_json::from_value(payload).map_err(|_| AgentError::BadPlan)?;
    Ok(Plan::from_proposed(proposed)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_plan_parses_proposal() {
        let provider = MockProvider::proposing(serde_json::json!({
            "summary": "archive logs",
            "steps": [
                {"tool": "list", "input": {"path": "/logs"}, "description": "list logs"},
                {"tool": "copy", "input": {}, "description": "copy to s3"},
                {"tool": "delete", "input": {}, "description": "remove originals"}
            ]
        }));
        let plan = request_plan(&provider, LlmRequest::default())
            .await
            .unwrap();
        assert_eq!(plan.steps.len(), 3);
        assert!(!plan.can_bulk_approve()); // contains a delete
        assert_eq!(plan.state, PlanState::Proposed);
    }

    #[tokio::test]
    async fn request_plan_degrades_across_tool_tiers() {
        let plan_obj = serde_json::json!({
            "summary": "tidy up",
            "steps": [{"tool": "list", "input": {"path": "/"}, "description": "list"}]
        });
        // Native: structured tool call.
        let native = MockProvider::proposing(plan_obj.clone());
        assert_eq!(
            request_plan(&native, LlmRequest::default())
                .await
                .unwrap()
                .steps
                .len(),
            1
        );
        // JsonSchema: a bare JSON object in the reply text.
        let schema = MockProvider::new(vec![LlmResponse::Text(format!("{plan_obj}"))])
            .with_support(ToolSupport::JsonSchema);
        assert_eq!(
            request_plan(&schema, LlmRequest::default())
                .await
                .unwrap()
                .steps
                .len(),
            1
        );
        // Text: a fenced ```json block.
        let text = MockProvider::new(vec![LlmResponse::Text(format!("```json\n{plan_obj}\n```"))])
            .with_support(ToolSupport::Text);
        assert_eq!(
            request_plan(&text, LlmRequest::default())
                .await
                .unwrap()
                .steps
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn request_plan_reports_malformed_payload() {
        // A balanced JSON object that isn't a valid plan (missing `steps`) → BadPlan, not NoPlan.
        let provider = MockProvider::new(vec![LlmResponse::Text("{\"summary\": \"x\"}".into())])
            .with_support(ToolSupport::JsonSchema);
        let err = request_plan(&provider, LlmRequest::default())
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::BadPlan), "got {err:?}");
    }

    #[tokio::test]
    async fn request_plan_rejects_unknown_tool() {
        let provider = MockProvider::proposing(serde_json::json!({
            "steps": [{"tool": "exfiltrate_secret", "input": {}, "description": "evil"}]
        }));
        let err = request_plan(&provider, LlmRequest::default())
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::Plan(PlanError::UnknownTool(_))));
    }

    #[tokio::test]
    async fn non_plan_response_is_error() {
        let provider = MockProvider::new(vec![LlmResponse::Text("hi".into())]);
        assert!(matches!(
            request_plan(&provider, LlmRequest::default()).await,
            Err(AgentError::NoPlan)
        ));
    }
}
