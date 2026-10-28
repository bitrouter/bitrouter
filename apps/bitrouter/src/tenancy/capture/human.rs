//! Explicit manual commands on the existing checked controller/actor path.
mod store;

use crate::{
    acp_trajectory::checkpoint::types::{AssessmentRevision, AssessmentSource, RevisionInput},
    core::evolution::{
        rubric,
        scoring::{RubricSubmission, ScoringReceipt},
    },
    harness::{
        acp::controlled::CheckpointReview,
        trajectory::owned::{Association, io},
    },
    storage::operation_budget::{Error, OperationBudget},
};
use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::operation::ErrorCode;
use sea_orm::DatabaseTransaction;

const INPUT_BYTES: usize = 512 * 1024;
const TEXT_BYTES: usize = 64 * 1024;
const VERSION: &str = "owned-manual-rubric-v1";

pub(crate) enum Input {
    Review(Option<String>),
    Submit(RubricSubmission),
    Retract(RevisionInput),
}
pub(crate) enum Reply {
    Review(Box<CheckpointReview>),
    Submitted(ScoringReceipt),
    Retracted(AssessmentRevision),
}
pub(crate) struct Request {
    native: String,
    input: Input,
}
fn id(value: &str) -> Result<()> {
    ensure!(
        !value.trim().is_empty() && value.len() <= 256,
        Error::Rejected(ErrorCode::InvalidRequest)
    );
    Ok(())
}
fn text(value: &str, maximum: usize) -> Result<()> {
    ensure!(
        value.len() <= maximum,
        Error::Rejected(ErrorCode::CapacityExceeded)
    );
    Ok(())
}
fn evaluation(value: &rubric::RubricEvaluation) -> Result<()> {
    ensure!(
        value.items.len() == rubric::library().len() && value.diagnostics.len() <= 128,
        Error::Rejected(ErrorCode::InvalidRequest)
    );
    text(&value.rubric_version, 256)?;
    text(&value.summary, TEXT_BYTES)?;
    let mut citations = value.violation_evidence.len();
    for item in &value.items {
        id(&item.criterion_id)?;
        text(&item.selection_reason, TEXT_BYTES)?;
        text(&item.explanation, TEXT_BYTES)?;
        citations = citations
            .checked_add(item.evidence.len())
            .ok_or(Error::Capacity)?;
    }
    for diagnostic in &value.diagnostics {
        id(&diagnostic.criterion_id)?;
        text(&diagnostic.explanation, TEXT_BYTES)?;
        citations = citations
            .checked_add(diagnostic.evidence.len())
            .ok_or(Error::Capacity)?;
    }
    ensure!(
        citations <= 1024,
        Error::Rejected(ErrorCode::CapacityExceeded)
    );
    for cite in value
        .items
        .iter()
        .flat_map(|item| &item.evidence)
        .chain(value.diagnostics.iter().flat_map(|item| &item.evidence))
        .chain(&value.violation_evidence)
    {
        id(&cite.node_id)?;
        text(&cite.digest, 256)?;
    }
    Ok(())
}
/// Pure bounded data normalization shared by the two authenticated host facades.
/// This neither constructs an Association nor supplies execution authority.
pub(in crate::tenancy) fn normalize(input: Input, principal: &str) -> Result<Input> {
    let author = format!("owned-human/{}", principal);
    id(&author)?;
    let normalized = match input {
        Input::Review(checkpoint) => {
            if let Some(value) = &checkpoint {
                id(value)?;
            }
            Input::Review(checkpoint.map(|value| value.into_boxed_str().into_string()))
        }
        Input::Submit(mut value) => {
            id(&value.submission_id)?;
            id(&value.checkpoint_id)?;
            if let Some(expected) = &value.expected_revision {
                id(expected)?;
            }
            text(&value.evaluator_id, 256)?;
            text(&value.evaluator_version, 256)?;
            ensure!(
                value.source == AssessmentSource::Human,
                Error::Rejected(ErrorCode::InvalidRequest)
            );
            evaluation(&value.evaluation)?;
            io::json(&value, INPUT_BYTES)
                .map_err(|_| Error::Rejected(ErrorCode::CapacityExceeded))?;
            value.evaluator_id = author;
            value.evaluator_version = VERSION.into();
            let bytes = io::json(&value, INPUT_BYTES)
                .map_err(|_| Error::Rejected(ErrorCode::CapacityExceeded))?;
            // Re-decode the bounded input so caller spare capacities are not retained.
            Input::Submit(serde_json::from_str(&bytes)?)
        }
        Input::Retract(mut value) => {
            id(&value.submission_id)?;
            id(&value.checkpoint_id)?;
            if let Some(expected) = &value.expected_revision {
                id(expected)?;
            }
            text(&value.evaluator_id, 256)?;
            text(&value.evaluator_version, 256)?;
            text(&value.reason, TEXT_BYTES)?;
            ensure!(
                value.source == AssessmentSource::Human
                    && value.assessment.is_none()
                    && !value.reason.trim().is_empty(),
                Error::Rejected(ErrorCode::InvalidRequest)
            );
            io::json(&value, INPUT_BYTES)
                .map_err(|_| Error::Rejected(ErrorCode::CapacityExceeded))?;
            value.evaluator_id = author;
            value.evaluator_version = VERSION.into();
            let bytes = io::json(&value, INPUT_BYTES)
                .map_err(|_| Error::Rejected(ErrorCode::CapacityExceeded))?;
            Input::Retract(serde_json::from_str(&bytes)?)
        }
    };
    Ok(normalized)
}

impl Request {
    pub(super) fn new(native: &str, input: Input, association: &Association) -> Result<Self> {
        id(native)?;
        let input = normalize(input, association.descriptor().principal.as_str())?;
        Ok(Self {
            native: native.to_owned(),
            input,
        })
    }
    pub(super) fn matches(&self, reply: &Reply) -> bool {
        match (&self.input, reply) {
            (Input::Review(checkpoint), Reply::Review(value)) => checkpoint
                .as_ref()
                .is_none_or(|id| id == &value.checkpoint.checkpoint_id),
            (Input::Submit(input), Reply::Submitted(value)) => {
                value.revision.input.submission_id == input.submission_id
                    && value.revision.input.checkpoint_id == input.checkpoint_id
                    && value.revision.input.evaluator_id == input.evaluator_id
            }
            (Input::Retract(input), Reply::Retracted(value)) => {
                value.input.submission_id == input.submission_id
                    && value.input.checkpoint_id == input.checkpoint_id
                    && value.input.evaluator_id == input.evaluator_id
                    && value.input.assessment.is_none()
            }
            _ => false,
        }
    }
    pub(in crate::tenancy) async fn execute(
        &self,
        tx: &DatabaseTransaction,
        association: &Association,
        budget: &OperationBudget<'_>,
    ) -> Result<(Reply, bool)> {
        store::execute(tx, association, &self.native, &self.input, budget).await
    }
}
