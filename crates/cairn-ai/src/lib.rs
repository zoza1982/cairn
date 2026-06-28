//! Cairn's agentic AI core.
//!
//! Provider-agnostic LLM access (`provider`), the closed `tools` surface, and the
//! `plan`→confirm→execute state machine. The model only *proposes* a plan; approval and execution
//! are gated outside the model. This crate depends only on `cairn-broker` (+ its deps), never on the
//! vault or backends, so the AI can never reach a secret except through a brokered, journaled
//! operation. Concrete cloud/local providers and TUI wiring are layered on later. See `docs/LLD.md`
//! §10.

mod context;
mod plan;
mod provider;
mod tools;

pub use context::{
    looks_out_of_scope, wrap_untrusted, ConnectionView, PaneView, WorldSnapshot, SYSTEM_POLICY,
};
pub use plan::{
    Plan, PlanError, PlanState, PlanStep, ProposedPlan, ProposedStep, StepExecutor, StepStatus,
};
pub use provider::{
    LlmProvider, LlmRequest, LlmResponse, Message, MockProvider, ProviderError, Role, ToolDef,
    Usage, DEFAULT_CLOUD_MODEL,
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

/// Ask the provider for a plan and parse it. The model must respond with a `propose_plan` tool call;
/// any other response is [`AgentError::NoPlan`]. Unknown tools in the plan are rejected.
///
/// # Errors
/// See [`AgentError`].
pub async fn request_plan(provider: &dyn LlmProvider, req: LlmRequest) -> Result<Plan, AgentError> {
    match provider.complete(req).await? {
        LlmResponse::ToolCall { name, input } if name == "propose_plan" => {
            let proposed: ProposedPlan =
                serde_json::from_value(input).map_err(|_| AgentError::BadPlan)?;
            Ok(Plan::from_proposed(proposed)?)
        }
        _ => Err(AgentError::NoPlan),
    }
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
