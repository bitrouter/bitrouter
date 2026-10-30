//! A model origin comes from the actual native journal after its preparing ACK.
//! Serializable facts below are data; only this module can construct an Origin.
pub(crate) mod completed;

use super::local::{Journal, error};
use crate::{
    storage::checkpoint::{
        CheckpointBatchExt, ack_for_batch, sha256,
        snapshot::{ModelStep, SessionSnapshot},
    },
    tenancy::execution::session::Binding,
};
use bitrouter_ai::core::{
    checkpoint::DurableHead,
    protocol::{CoreError, ModelMode, validate_id},
};
use bitrouter_ai::harness::inspection::AgentStatus;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Retained immutable input of one actual Core step, never an authority constructor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StepFacts {
    pub run: String,
    pub run_model: String,
    pub run_selection: ModelMode,
    pub agent: String,
    pub turn: String,
    pub step: String,
    pub decision: String,
    pub model: String,
    pub selection: ModelMode,
    pub epoch: u64,
    pub preparing_head: DurableHead,
    pub input_digest: String,
}

pub(crate) struct Origin {
    binding: Arc<Binding>,
    facts: StepFacts,
}

impl Origin {
    pub(crate) fn binding(&self) -> &Arc<Binding> {
        &self.binding
    }
    pub(crate) fn facts(&self) -> &StepFacts {
        &self.facts
    }
    /// The actor calls this after the scoped, length-bounded whole-row read.
    /// A later ACK may retain the same unexecuted step; it cannot replace its input.
    pub(crate) fn validate_loaded(&self, bytes: &[u8]) -> Result<(), CoreError> {
        let journal: Journal = serde_json::from_slice(bytes)
            .map_err(|_| error("invalid native model source journal"))?;
        let state = checked_snapshot(&journal, &self.binding)?;
        if journal.released
            || journal.head.state_revision < self.facts.preparing_head.state_revision
            || journal.head.event_seq < self.facts.preparing_head.event_seq
            || (journal.head.state_revision == self.facts.preparing_head.state_revision
                && journal.head != self.facts.preparing_head)
        {
            return Err(error(
                "native model source moved outside its acknowledged origin",
            ));
        }
        let (turn, step) = preparing(&state, &self.facts.agent, &self.facts.step)?;
        if state.run.as_ref().is_none_or(|run| {
            run.input.model != self.facts.run_model
                || run.input.routing.model != self.facts.run_selection
        }) || turn.run_id != self.facts.run
            || turn.agent_turn_id != self.facts.turn
            || turn.input.model != self.facts.model
            || turn.input.routing.model != self.facts.selection
            || step.decision_id != self.facts.decision
            || journal.grant.execution_epoch != self.facts.epoch
            || input_digest(step)? != self.facts.input_digest
        {
            return Err(error("native model source identity changed"));
        }
        Ok(())
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
    let run = state
        .run
        .as_ref()
        .ok_or_else(|| error("native model run is absent"))?;
    let facts = StepFacts {
        run: turn.run_id.clone(),
        run_model: run.input.model.clone(),
        run_selection: run.input.routing.model,
        agent: agent.to_owned(),
        turn: turn.agent_turn_id.clone(),
        step: step.step_id.clone(),
        decision: step.decision_id.clone(),
        model: turn.input.model.clone(),
        selection: turn.input.routing.model,
        epoch: journal.grant.execution_epoch,
        preparing_head: journal.head.clone(),
        input_digest: input_digest(step)?,
    };
    for value in [
        &facts.run,
        &facts.agent,
        &facts.turn,
        &facts.step,
        &facts.decision,
    ] {
        validate_id(value)?;
    }
    Ok(Arc::new(Origin {
        binding: binding.clone(),
        facts,
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

fn input_digest(step: &ModelStep) -> Result<String, CoreError> {
    // Mutable plan/attempt/terminal fields never alter the prepared input identity.
    let value = (
        &step.step_id,
        &step.decision_id,
        step.context_revision,
        step.signal_revision,
        &step.manifest,
        &step.materials,
        step.input_state_revision,
        &step.input_history,
        &step.reconstructed_from,
    );
    let bytes = crate::managed_store::owned::encode(&value)
        .map_err(|_| error("native model input exceeds its bounded source"))?;
    Ok(sha256(&bytes))
}

/// One validated journal decode reused across all pending experiment links.
pub(crate) struct Readback {
    state: SessionSnapshot,
    head: DurableHead,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AckFacts {
    pub head: DurableHead,
    pub terminal_step: String,
    pub planned: bool,
    pub interrupted: bool,
}
impl Readback {
    /// No execution authority is minted. The caller holds H's current read grant
    /// and the actual stopped-host slot throughout acquisition and publication.
    pub(crate) fn stopped(
        loaded: &crate::managed_store::owned::Loaded,
        run: &str,
    ) -> Result<Self, CoreError> {
        let journal: Journal = serde_json::from_slice(&loaded.journal)
            .map_err(|_| error("invalid stopped native journal"))?;
        let state = checked_contents(&journal, None)?;
        use crate::storage::checkpoint::snapshot::RunStatus;
        if !journal.released
            || journal.grant.session_id != loaded.descriptor.session
            || journal.grant.harness_id != loaded.descriptor.harness
            || state.run.as_ref().is_none_or(|value| {
                value.run_id != run
                    || !matches!(
                        value.status,
                        RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
                    )
            })
        {
            return Err(error("native source has no confirmed terminal release"));
        }
        Ok(Self {
            state,
            head: journal.head,
        })
    }

    pub(crate) fn load(binding: &Binding, bytes: &[u8]) -> Result<Self, CoreError> {
        let journal: Journal =
            serde_json::from_slice(bytes).map_err(|_| error("invalid native ACK journal"))?;
        Ok(Self {
            state: checked_snapshot(&journal, binding)?,
            head: journal.head,
        })
    }
    pub(crate) fn settled(
        &self,
        facts: &StepFacts,
        execution: &str,
    ) -> Result<Option<AckFacts>, CoreError> {
        let Some(turn) = self
            .state
            .agents
            .get(&facts.agent)
            .and_then(|agent| agent.turn.as_ref())
        else {
            return Ok(None);
        };
        if turn.run_id != facts.run || turn.agent_turn_id != facts.turn {
            return Ok(None);
        }
        let Some(step) = turn.steps.iter().find(|step| step.step_id == facts.step) else {
            return Ok(None);
        };
        if step.decision_id != facts.decision
            || input_digest(step)? != facts.input_digest
            || self.head.state_revision < facts.preparing_head.state_revision
            || self.head.execution_epoch != facts.epoch
        {
            return Err(error("native ACK does not retain its model origin"));
        }
        // One controlled pipeline may reconstruct context before it sends.
        // The settled rejected source is not the final output ACK: follow the
        // actual immutable reconstruction chain in this same acknowledged turn.
        let mut terminal = step;
        let mut visited = std::collections::BTreeSet::new();
        loop {
            if !visited.insert(&terminal.step_id) {
                return Err(error("native ACK reconstruction cycle"));
            }
            if terminal
                .plan
                .as_ref()
                .is_some_and(|plan| plan.request_id != execution)
            {
                return Err(error("native ACK belongs to another execution"));
            }
            let mut descendants = turn.steps.iter().filter(|candidate| {
                candidate.reconstructed_from.as_deref() == Some(terminal.step_id.as_str())
            });
            let next = descendants.next();
            if descendants.next().is_some() {
                return Err(error("native ACK reconstruction fork"));
            }
            match next {
                Some(next) => terminal = next,
                None => break,
            }
        }
        if !terminal.settled && !terminal.interrupted {
            return Ok(None);
        }
        Ok(Some(AckFacts {
            head: self.head.clone(),
            terminal_step: terminal.step_id.clone(),
            planned: terminal.plan.is_some(),
            interrupted: terminal.interrupted,
        }))
    }
}
