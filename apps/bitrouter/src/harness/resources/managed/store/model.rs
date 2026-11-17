//! A model origin comes from the actual native journal after its preparing ACK.
//! Only this module constructs the binding used for the local harness step.

use super::local::{Journal, error};
use crate::{
    storage::checkpoint::{
        CheckpointBatchExt, ack_for_batch,
        snapshot::{ModelStep, SessionSnapshot},
    },
    tenancy::execution::session::Binding,
};
use bitrouter_ai::core::protocol::{CoreError, validate_id};
use bitrouter_ai::harness::inspection::AgentStatus;
use std::sync::Arc;

pub(crate) struct Origin {
    binding: Arc<Binding>,
}

impl Origin {
    pub(crate) fn binding(&self) -> &Arc<Binding> {
        &self.binding
    }
}

pub(super) fn origin(
    journal: &Journal,
    binding: &Arc<Binding>,
    agent: &str,
    step: &str,
) -> Result<Arc<Origin>, CoreError> {
    binding.check()?;
    let state = checked_snapshot(journal, binding)?;
    let (turn, step) = preparing(&state, agent, step)?;
    if journal.released {
        return Err(error("released native journal cannot authorize model work"));
    }
    for value in [
        turn.run_id.as_str(),
        agent,
        turn.agent_turn_id.as_str(),
        step.step_id.as_str(),
        step.decision_id.as_str(),
    ] {
        validate_id(value)?;
    }
    Ok(Arc::new(Origin {
        binding: binding.clone(),
    }))
}

fn preparing<'a>(
    state: &'a SessionSnapshot,
    agent: &str,
    step: &str,
) -> Result<
    (
        &'a crate::storage::checkpoint::snapshot::AgentTurn,
        &'a ModelStep,
    ),
    CoreError,
> {
    let agent = state
        .agents
        .get(agent)
        .ok_or_else(|| error("native model agent is absent"))?;
    let turn = agent
        .turn
        .as_ref()
        .ok_or_else(|| error("native model turn is absent"))?;
    let step = turn
        .steps
        .last()
        .filter(|value| value.step_id == step)
        .ok_or_else(|| error("native model step is not the current preparation"))?;
    if state
        .run
        .as_ref()
        .is_none_or(|run| run.run_id != turn.run_id)
        || turn.status != AgentStatus::ModelRunning
        || turn.cancellation_requested
        || turn.final_answer.is_some()
        || step.settled
        || step.interrupted
        || step.plan.is_some()
        || step.decision.is_some()
        || step.application.is_some()
        || step.count_plan.is_some()
        || !step.input_counts.is_empty()
        || !step.preparation_work.is_empty()
        || !step.attempts.is_empty()
    {
        return Err(error("native model step has already advanced or stopped"));
    }
    Ok((turn, step))
}

/// Validate the complete stored checkpoint and artifact closure, not row metadata
/// or a caller-supplied projection. This is shared by enrollment and the final ACK.
pub(super) fn checked_snapshot(
    journal: &Journal,
    binding: &Binding,
) -> Result<SessionSnapshot, CoreError> {
    binding.grant(&journal.grant)?;
    checked_contents(journal, Some(binding))
}

fn checked_contents(
    journal: &Journal,
    binding: Option<&Binding>,
) -> Result<SessionSnapshot, CoreError> {
    journal.grant.validate()?;
    journal.head.validate()?;
    if journal.format != 1
        || journal.limits != crate::harness::resources::managed::NativeResources::limits()
    {
        return Err(error("native model journal profile changed"));
    }
    let batch = journal
        .checkpoint
        .as_ref()
        .ok_or_else(|| error("native model requires an acknowledged checkpoint"))?;
    let payload = batch.decode(&journal.limits)?;
    let expected = ack_for_batch(batch, &payload);
    if journal.ack.as_ref() != Some(&expected)
        || journal.head != expected.head()
        || payload.identity.session_id != journal.grant.session_id
        || payload.identity.core_instance_id != journal.grant.core_instance_id
        || payload.identity.execution_epoch != journal.grant.execution_epoch
    {
        return Err(error(
            "native model checkpoint is not the acknowledged owner head",
        ));
    }
    journal.validate_model_artifacts(&payload.checkpoint.artifact_refs)?;
    super::choice::validate(journal, binding)?;
    let state: SessionSnapshot = serde_json::from_value(payload.checkpoint.state)
        .map_err(|_| error("native model checkpoint snapshot is invalid"))?;
    if state.session_id != journal.grant.session_id || state.manifest != journal.manifest {
        return Err(error(
            "native model checkpoint source differs from its journal",
        ));
    }
    crate::storage::checkpoint::snapshot::context::validation::validate(&state)?;
    Ok(state)
}
