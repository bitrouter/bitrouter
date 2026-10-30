//! Local native review data; it contains evidence, never an authority handle.
use super::NativeSession;
use crate::{
    acp_trajectory::checkpoint::types::{AssessmentRevision, RevisionInput},
    core::evolution::{
        rubric::RubricEvaluation,
        scoring::{RubricSubmission, ScoringInput, ScoringReceipt},
    },
    tenancy::{capture::human::Input, session::assessment::Reply},
};
use bitrouter_ai::core::{
    checkpoint::DurableHead,
    protocol::{CoreError, ErrorCode},
};
use serde::Serialize;

/// The checked current native run and its rubric history. This is not an ACP checkpoint.
#[derive(Serialize)]
pub struct NativeRunReview {
    pub run_id: String,
    pub head: DurableHead,
    pub scoring: ScoringInput,
    pub previous: Option<RubricEvaluation>,
    pub prefill_revision: Option<String>,
    pub history: Vec<AssessmentRevision>,
}

impl NativeSession {
    /// Review the current acknowledged, completed native experiment run.
    /// Requires current ResourceRead and the session's original scope ceilings.
    /// No provider operation or implicit evaluator discovery is performed.
    pub async fn review_run(&self) -> Result<NativeRunReview, CoreError> {
        match self.native_assessment(Input::Review(None)).await? {
            Reply::Review(value) => Ok(*value),
            _ => Err(mismatch()),
        }
    }
    /// Submit a cited Human rubric with an explicit expected effective revision.
    /// The authenticated stable principal replaces caller-supplied evaluator identity.
    pub async fn submit_assessment(
        &self,
        input: RubricSubmission,
    ) -> Result<ScoringReceipt, CoreError> {
        match self.native_assessment(Input::Submit(input)).await? {
            Reply::Submitted(value) => Ok(value),
            _ => Err(mismatch()),
        }
    }
    /// Retract the current judgement without reviving an older assessment.
    /// The explicit head CAS and current EvaluationSubmit permission are required.
    pub async fn retract_assessment(
        &self,
        input: RevisionInput,
    ) -> Result<AssessmentRevision, CoreError> {
        match self.native_assessment(Input::Retract(input)).await? {
            Reply::Retracted(value) => Ok(value),
            _ => Err(mismatch()),
        }
    }
    /// Explicitly assess this completed native run using its retained evaluator.
    /// Acceptance spends one cumulative logical job of the immutable revision.
    /// Human corrections remain available while the judge is running. A late
    /// answer never replaces an intervening Human head. There is no auto-retry.
    pub async fn assess_run(
        &self,
        checkpoint: &str,
        expected_revision: Option<&str>,
    ) -> Result<(), CoreError> {
        self.owned_handle
            .as_ref()
            .ok_or_else(|| {
                CoreError::rejected(
                    ErrorCode::UnauthorizedScope,
                    "native paid assessment requires an authenticated session",
                )
            })?
            .assess_run(checkpoint, expected_revision)
            .await
    }
    async fn native_assessment(&self, input: Input) -> Result<Reply, CoreError> {
        self.owned_handle
            .as_ref()
            .ok_or_else(|| {
                CoreError::rejected(
                    ErrorCode::UnauthorizedScope,
                    "native assessment requires an authenticated session",
                )
            })?
            .assessment(input)
            .await
    }
}
fn mismatch() -> CoreError {
    CoreError::rejected(
        ErrorCode::CheckpointConflict,
        "native assessment reply does not match the operation",
    )
}
