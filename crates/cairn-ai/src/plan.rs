//! The plan → confirm → execute state machine.
//!
//! The model only *proposes* a plan; execution is gated by explicit approval that the model has no
//! tool for. Approval rules enforce the safety invariant: a plan may be **bulk-approved only if
//! every step is `Safe`/`Recoverable`**; any `Irreversible` step (delete, exec) must be approved
//! individually. A step failure aborts the remainder (no rollback). See LLD §10.3.

use crate::tools::{allows_bulk_approve, capability_for, Capability};
use async_trait::async_trait;
use serde::Deserialize;

/// Per-step lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    /// Not yet approved.
    Pending,
    /// Approved for execution.
    Approved,
    /// Rejected by the user.
    Rejected,
    /// Executed successfully.
    Done,
    /// Execution failed.
    Failed,
}

/// Overall plan lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanState {
    /// Proposed, awaiting approval.
    Proposed,
    /// Executing approved steps.
    Executing,
    /// All steps completed.
    Done,
    /// A step failed; execution stopped.
    Failed,
    /// Abandoned before completion.
    Aborted,
}

/// One concrete step of a plan.
#[derive(Debug, Clone)]
pub struct PlanStep {
    /// The tool to invoke.
    pub tool: String,
    /// The tool input (validated/executed downstream).
    pub input: serde_json::Value,
    /// A human-readable description shown in the confirm UI.
    pub description: String,
    /// The step's capability (verb + reversibility).
    pub capability: Capability,
    /// Current status.
    pub status: StepStatus,
    /// On failure, a redacted message describing why (set by [`Plan::execute`]).
    pub error: Option<String>,
}

/// A proposed, then executable, plan.
#[derive(Debug, Clone)]
pub struct Plan {
    /// One-line summary of intent.
    pub summary: String,
    /// Ordered steps.
    pub steps: Vec<PlanStep>,
    /// Lifecycle state.
    pub state: PlanState,
}

/// A step as proposed by the model (before capability resolution).
#[derive(Debug, Clone, Deserialize)]
pub struct ProposedStep {
    /// Tool name.
    pub tool: String,
    /// Tool input.
    #[serde(default)]
    pub input: serde_json::Value,
    /// Description.
    #[serde(default)]
    pub description: String,
}

/// The model's `propose_plan` payload.
#[derive(Debug, Clone, Deserialize)]
pub struct ProposedPlan {
    /// Summary of intent.
    #[serde(default)]
    pub summary: String,
    /// Proposed steps.
    pub steps: Vec<ProposedStep>,
}

/// Errors from plan construction/approval/execution.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlanError {
    /// The model named a tool outside the closed set.
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    /// Bulk approval was attempted on a plan containing an irreversible step.
    #[error("bulk approve refused: plan contains an irreversible step")]
    BulkApproveRefused,
    /// Execution was attempted with unapproved steps.
    #[error("cannot execute: step {0} is not approved")]
    NotApproved(usize),
    /// A step index was out of range.
    #[error("no such step: {0}")]
    NoSuchStep(usize),
}

/// Executes a single approved step (wired to the broker/backends by the caller).
#[async_trait]
pub trait StepExecutor {
    /// Execute `step`. Returns `Err(message)` on failure.
    async fn execute(&self, step: &PlanStep) -> Result<(), String>;
}

impl Plan {
    /// Build a plan from the model's proposal, resolving each tool's capability. Rejects any tool
    /// outside the closed set.
    ///
    /// # Errors
    /// [`PlanError::UnknownTool`] if a step names an unknown tool.
    pub fn from_proposed(proposed: ProposedPlan) -> Result<Self, PlanError> {
        let mut steps = Vec::with_capacity(proposed.steps.len());
        for s in proposed.steps {
            let capability =
                capability_for(&s.tool).ok_or_else(|| PlanError::UnknownTool(s.tool.clone()))?;
            steps.push(PlanStep {
                tool: s.tool,
                input: s.input,
                description: s.description,
                capability,
                status: StepStatus::Pending,
                error: None,
            });
        }
        Ok(Self {
            summary: proposed.summary,
            steps,
            state: PlanState::Proposed,
        })
    }

    /// Whether the entire plan may be approved at once (all steps `Safe`/`Recoverable`).
    #[must_use]
    pub fn can_bulk_approve(&self) -> bool {
        self.steps.iter().all(|s| allows_bulk_approve(s.capability))
    }

    /// Whether every step has been approved — the precondition for [`execute`](Self::execute).
    /// False for an empty plan (there is nothing to run).
    #[must_use]
    pub fn is_all_approved(&self) -> bool {
        !self.steps.is_empty() && self.steps.iter().all(|s| s.status == StepStatus::Approved)
    }

    /// The index of the next step still awaiting a decision, searching forward from `after` and
    /// wrapping to the start. `None` when no step is pending.
    #[must_use]
    pub fn next_pending_from(&self, after: usize) -> Option<usize> {
        let start = after.saturating_add(1);
        self.steps
            .iter()
            .skip(start)
            .position(|s| s.status == StepStatus::Pending)
            .map(|rel| rel + start)
            .or_else(|| {
                self.steps
                    .iter()
                    .position(|s| s.status == StepStatus::Pending)
            })
    }

    /// Approve every step at once.
    ///
    /// # Errors
    /// [`PlanError::BulkApproveRefused`] if any step is irreversible.
    pub fn approve_all(&mut self) -> Result<(), PlanError> {
        if !self.can_bulk_approve() {
            return Err(PlanError::BulkApproveRefused);
        }
        for s in &mut self.steps {
            s.status = StepStatus::Approved;
        }
        Ok(())
    }

    /// Approve a single step (the path required for irreversible steps).
    ///
    /// # Errors
    /// [`PlanError::NoSuchStep`] if the index is out of range.
    pub fn approve_step(&mut self, i: usize) -> Result<(), PlanError> {
        self.steps
            .get_mut(i)
            .ok_or(PlanError::NoSuchStep(i))?
            .status = StepStatus::Approved;
        Ok(())
    }

    /// Reject a single step.
    ///
    /// # Errors
    /// [`PlanError::NoSuchStep`] if the index is out of range.
    pub fn reject_step(&mut self, i: usize) -> Result<(), PlanError> {
        self.steps
            .get_mut(i)
            .ok_or(PlanError::NoSuchStep(i))?
            .status = StepStatus::Rejected;
        Ok(())
    }

    /// Abort the plan.
    pub fn abort(&mut self) {
        self.state = PlanState::Aborted;
    }

    /// Execute the plan: every step must be `Approved`. Steps run in order; the first failure sets
    /// the plan to [`PlanState::Failed`] and stops. On success the plan is [`PlanState::Done`].
    ///
    /// # Errors
    /// [`PlanError::NotApproved`] if any step is not approved before execution.
    pub async fn execute<E: StepExecutor + Sync>(&mut self, exec: &E) -> Result<(), PlanError> {
        for (i, s) in self.steps.iter().enumerate() {
            if s.status != StepStatus::Approved {
                return Err(PlanError::NotApproved(i));
            }
        }
        self.state = PlanState::Executing;
        for i in 0..self.steps.len() {
            match exec.execute(&self.steps[i]).await {
                Ok(()) => self.steps[i].status = StepStatus::Done,
                Err(msg) => {
                    self.steps[i].status = StepStatus::Failed;
                    self.steps[i].error = Some(msg);
                    self.state = PlanState::Failed;
                    return Ok(());
                }
            }
        }
        self.state = PlanState::Done;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn proposed(steps: &[(&str, &str)]) -> ProposedPlan {
        ProposedPlan {
            summary: "test".into(),
            steps: steps
                .iter()
                .map(|(tool, desc)| ProposedStep {
                    tool: (*tool).into(),
                    input: serde_json::json!({}),
                    description: (*desc).into(),
                })
                .collect(),
        }
    }

    struct MockExec {
        executed: Mutex<Vec<String>>,
        fail_at: Option<usize>,
    }
    impl MockExec {
        fn new(fail_at: Option<usize>) -> Self {
            Self {
                executed: Mutex::new(Vec::new()),
                fail_at,
            }
        }
    }
    #[async_trait]
    impl StepExecutor for MockExec {
        async fn execute(&self, step: &PlanStep) -> Result<(), String> {
            let mut ex = self.executed.lock().unwrap();
            let idx = ex.len();
            ex.push(step.tool.clone());
            if self.fail_at == Some(idx) {
                return Err("boom".into());
            }
            Ok(())
        }
    }

    #[test]
    fn unknown_tool_is_rejected() {
        let err = Plan::from_proposed(proposed(&[("read_secret", "x")])).unwrap_err();
        assert_eq!(err, PlanError::UnknownTool("read_secret".into()));
    }

    #[test]
    fn safe_plan_allows_bulk_approve() {
        let p = Plan::from_proposed(proposed(&[("list", ""), ("copy", "")])).unwrap();
        assert!(p.can_bulk_approve());
    }

    #[test]
    fn irreversible_plan_refuses_bulk_approve() {
        let mut p = Plan::from_proposed(proposed(&[("copy", ""), ("delete", "")])).unwrap();
        assert!(!p.can_bulk_approve());
        assert_eq!(p.approve_all().unwrap_err(), PlanError::BulkApproveRefused);
    }

    #[test]
    fn is_all_approved_tracks_step_status() {
        let mut p = Plan::from_proposed(proposed(&[("list", ""), ("copy", "")])).unwrap();
        assert!(!p.is_all_approved());
        p.approve_step(0).unwrap();
        assert!(!p.is_all_approved());
        p.approve_step(1).unwrap();
        assert!(p.is_all_approved());
        // An empty plan is never "all approved".
        let empty = Plan::from_proposed(proposed(&[])).unwrap();
        assert!(!empty.is_all_approved());
    }

    #[test]
    fn next_pending_from_advances_then_wraps() {
        let mut p =
            Plan::from_proposed(proposed(&[("list", ""), ("copy", ""), ("move", "")])).unwrap();
        // From step 0, the next pending is 1.
        assert_eq!(p.next_pending_from(0), Some(1));
        // Approve step 1; from 1 it should skip to 2.
        p.approve_step(1).unwrap();
        assert_eq!(p.next_pending_from(1), Some(2));
        // From the last step it wraps back to the first still-pending (0).
        assert_eq!(p.next_pending_from(2), Some(0));
        // Once all approved, there is no pending step.
        p.approve_all().unwrap();
        assert_eq!(p.next_pending_from(0), None);
    }

    #[tokio::test]
    async fn bulk_approved_plan_executes_all() {
        let mut p = Plan::from_proposed(proposed(&[("list", ""), ("copy", "")])).unwrap();
        p.approve_all().unwrap();
        let exec = MockExec::new(None);
        p.execute(&exec).await.unwrap();
        assert_eq!(p.state, PlanState::Done);
        assert!(p.steps.iter().all(|s| s.status == StepStatus::Done));
        assert_eq!(exec.executed.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn unapproved_step_blocks_execution() {
        let mut p = Plan::from_proposed(proposed(&[("copy", "")])).unwrap();
        let exec = MockExec::new(None);
        assert_eq!(
            p.execute(&exec).await.unwrap_err(),
            PlanError::NotApproved(0)
        );
        assert!(exec.executed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn irreversible_step_requires_individual_approval_then_runs() {
        let mut p = Plan::from_proposed(proposed(&[("copy", ""), ("delete", "")])).unwrap();
        // Bulk refused; approve each individually.
        p.approve_step(0).unwrap();
        p.approve_step(1).unwrap();
        let exec = MockExec::new(None);
        p.execute(&exec).await.unwrap();
        assert_eq!(p.state, PlanState::Done);
    }

    #[tokio::test]
    async fn failure_aborts_remaining_steps() {
        let mut p =
            Plan::from_proposed(proposed(&[("copy", ""), ("copy", ""), ("copy", "")])).unwrap();
        p.approve_all().unwrap();
        let exec = MockExec::new(Some(1)); // fail the second step
        p.execute(&exec).await.unwrap();
        assert_eq!(p.state, PlanState::Failed);
        assert_eq!(p.steps[0].status, StepStatus::Done);
        assert_eq!(p.steps[1].status, StepStatus::Failed);
        assert_eq!(p.steps[2].status, StepStatus::Approved); // never ran
        assert_eq!(exec.executed.lock().unwrap().len(), 2); // third not attempted
    }
}
