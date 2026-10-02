//! Durable cost exposure from acknowledged execution records. This inventory
//! retains retired work; it neither charges callers nor invents provider bills.

use std::collections::BTreeMap;

use bitrouter_sdk::language_model::native_accounting::NativeTokenCost;
use serde::{Deserialize, Serialize};

use crate::core::protocol::{CoreError, ErrorCode};
use crate::core::session::{AgentStatus, AgentTurn, ModelStep, SessionSnapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostWorkKind {
    /// Includes App transforms, routing preparation and bound request checks.
    Preparation,
    PreparationCallback,
    ProviderAttempt,
    InputCount,
    ContextValidation,
    ContextRebuild,
    WorkspaceTool,
    MaterialFetch,
    AuthenticationPreparation,
    Authentication,
    AuthenticationRefresh,
    HttpDispatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostWorkState {
    /// Dispatch may or may not have occurred. Absence of an outcome is not zero.
    IntentRecorded,
    /// Execution evidence is durable, independently of whether money is known.
    OutcomeRecorded,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostWork {
    pub agent_id: String,
    pub agent_turn_id: String,
    /// Material fetching precedes model-step admission and has no step ID.
    pub step_id: Option<String>,
    /// SDK billing identity, never a harness material request ID.
    pub request_id: Option<String>,
    pub kind: CostWorkKind,
    pub state: CostWorkState,
    /// Provider operation time when observed, excluding checkpoint waits.
    /// Missing measurements, including preparation/tool time, remain unknown.
    pub elapsed_ms: Option<u64>,
    /// An estimate of model tokens only; never a reported or reconciled charge.
    pub token_estimate: Option<NativeTokenCost>,
    /// Total expenditure remains unknown even when a token estimate is known.
    /// A successful operation, including a denied tool, is not a billing receipt.
    pub unknown_cost_reason: String,
}

/// Present only for runs tracked from admission. Missing legacy run entries do
/// not establish an empty or zero-cost run. Entries survive turn/run replacement.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunCostWork {
    pub work: BTreeMap<String, CostWork>,
    /// Separate bases for each source/bill identity. Never a sum of all bases.
    #[serde(default)]
    pub charges:
        BTreeMap<String, bitrouter_sdk::language_model::native_accounting::NativeCostClaim>,
    /// Latest read uncertainty for a request, independent of retained claims.
    #[serde(default)]
    pub charge_unknown: BTreeMap<String, String>,
}

impl RunCostWork {
    fn record(&mut self, id: String, next: CostWork) -> Result<(), CoreError> {
        if let Some(prior) = self.work.get(&id) {
            let same_identity = prior.agent_id == next.agent_id
                && prior.agent_turn_id == next.agent_turn_id
                && prior.step_id == next.step_id
                && prior.kind == next.kind
                && (prior.request_id.is_none() || prior.request_id == next.request_id);
            let same_outcome = prior.state != CostWorkState::OutcomeRecorded || prior == &next;
            if !same_identity || !same_outcome {
                return Err(CoreError::rejected(
                    ErrorCode::CheckpointConflict,
                    "cost work identity or committed outcome changed",
                ));
            }
        }
        self.work.insert(id, next);
        Ok(())
    }
}

fn work(agent_id: &str, turn: &AgentTurn, step: &ModelStep, kind: CostWorkKind) -> CostWork {
    CostWork {
        agent_id: agent_id.into(),
        agent_turn_id: turn.agent_turn_id.clone(),
        step_id: Some(step.step_id.clone()),
        request_id: step
            .plan
            .as_ref()
            .or(step.count_plan.as_ref())
            .map(|plan| plan.request_id.clone()),
        kind,
        state: CostWorkState::IntentRecorded,
        elapsed_ms: None,
        token_estimate: None,
        unknown_cost_reason: "cost_not_reported".into(),
    }
}

/// Extend the cost inventory inside the same proposed checkpoint as its source
/// intent/outcome. Failed ACKs cannot expose new evidence or erase old exposure.
pub(crate) fn synchronize(state: &mut SessionSnapshot) -> Result<(), CoreError> {
    for (agent_id, agent) in &state.agents {
        let Some(turn) = agent.turn.as_ref() else {
            continue;
        };
        let Some(ledger) = state.cost_work.get_mut(&turn.run_id) else {
            continue;
        };
        for step in &turn.steps {
            if step.reconstructed_from.is_none() {
                let mut entry = work(agent_id, turn, step, CostWorkKind::Preparation);
                if step.plan.is_some()
                    || step.count_plan.is_some()
                    || (step.settled && !step.interrupted)
                    || turn.status == AgentStatus::Failed
                {
                    entry.state = CostWorkState::OutcomeRecorded;
                }
                ledger.record(format!("{}/preparation", step.step_id), entry)?;
            }
            for record in &step.preparation_work {
                let mut entry = work(agent_id, turn, step, CostWorkKind::PreparationCallback);
                entry.request_id = Some(record.work.request_id.clone());
                if let Some(report) = &record.report {
                    entry.state = CostWorkState::OutcomeRecorded;
                    entry.elapsed_ms = Some(report.elapsed_ms);
                }
                let scope = if record.work.kind == bitrouter_sdk::language_model::native_preparation::NativePreparationWorkKind::PromptTransform {
                    "app"
                } else {
                    "pipeline"
                };
                ledger.record(
                    format!(
                        "{}/preparation/{scope}/{}",
                        step.step_id, record.work.work_index
                    ),
                    entry,
                )?;
            }
            for count in &step.input_counts {
                let mut entry = work(agent_id, turn, step, CostWorkKind::InputCount);
                if let Some(report) = &count.report {
                    entry.state = CostWorkState::OutcomeRecorded;
                    entry.elapsed_ms = Some(report.elapsed_ms);
                }
                ledger.record(
                    format!("{}/count/{}", step.step_id, count.route_index),
                    entry,
                )?;
            }
            if let Some(validation) = &step.context_validation {
                let mut entry = work(agent_id, turn, step, CostWorkKind::ContextValidation);
                entry.request_id = Some(validation.request_id.clone());
                if let Some(report) = &validation.report {
                    entry.state = CostWorkState::OutcomeRecorded;
                    entry.elapsed_ms = report.work_elapsed_ms;
                }
                ledger.record(format!("{}/validation", step.step_id), entry)?;
            }
            if let Some(rebuild) = &step.rebuild {
                let mut entry = work(agent_id, turn, step, CostWorkKind::ContextRebuild);
                entry.state = CostWorkState::OutcomeRecorded;
                entry.elapsed_ms = Some(rebuild.elapsed_ms);
                ledger.record(format!("{}/rebuild", step.step_id), entry)?;
            }
            for attempt in &step.attempts {
                let mut entry = work(agent_id, turn, step, CostWorkKind::ProviderAttempt);
                if let Some(receipt) = &attempt.receipt {
                    entry.state = CostWorkState::OutcomeRecorded;
                    entry.token_estimate = Some(receipt.report.token_cost.clone());
                    entry.elapsed_ms = Some(receipt.report.elapsed_ms);
                }
                ledger.record(attempt.attempt_id.clone(), entry)?;
                for record in &attempt.provider_work {
                    use bitrouter_sdk::language_model::native_work::NativeProviderWorkKind;
                    let kind = match record.work.kind {
                        NativeProviderWorkKind::AuthenticationPreparation => {
                            CostWorkKind::AuthenticationPreparation
                        }
                        NativeProviderWorkKind::Authentication => CostWorkKind::Authentication,
                        NativeProviderWorkKind::AuthenticationRefresh => {
                            CostWorkKind::AuthenticationRefresh
                        }
                        NativeProviderWorkKind::HttpDispatch => CostWorkKind::HttpDispatch,
                    };
                    let mut entry = work(agent_id, turn, step, kind);
                    if let Some(report) = &record.report {
                        entry.state = CostWorkState::OutcomeRecorded;
                        entry.elapsed_ms = Some(report.elapsed_ms);
                    }
                    ledger.record(
                        format!(
                            "{}/provider-work/{}",
                            attempt.attempt_id, record.work.work_index
                        ),
                        entry,
                    )?;
                }
            }
        }
        for invocation in &turn.invocations {
            let dispatch = &invocation.dispatch;
            ledger.record(
                dispatch.attempt_id.clone(),
                CostWork {
                    agent_id: dispatch.agent_id.clone(),
                    agent_turn_id: dispatch.agent_turn_id.clone(),
                    step_id: Some(dispatch.step_id.clone()),
                    request_id: None,
                    kind: CostWorkKind::WorkspaceTool,
                    state: if invocation.result.is_some() {
                        CostWorkState::OutcomeRecorded
                    } else {
                        CostWorkState::IntentRecorded
                    },
                    elapsed_ms: None,
                    token_estimate: None,
                    unknown_cost_reason: "harness_cost_not_reported".into(),
                },
            )?;
        }
    }
    // Requests outlive their initiating turns. Reusing a pending fetch never
    // transfers its expenditure to a newer run or duplicates it for consumers.
    for request in state.signals.requests.values() {
        let Some(origin) = &request.origin else {
            continue;
        };
        let Some(ledger) = state.cost_work.get_mut(&origin.run_id) else {
            continue;
        };
        ledger.record(
            request.request_id.clone(),
            CostWork {
                agent_id: origin.agent_id.clone(),
                agent_turn_id: origin.agent_turn_id.clone(),
                step_id: None,
                request_id: None,
                kind: CostWorkKind::MaterialFetch,
                state: if request.resolved {
                    CostWorkState::OutcomeRecorded
                } else {
                    CostWorkState::IntentRecorded
                },
                elapsed_ms: None,
                token_estimate: None,
                unknown_cost_reason: "harness_cost_not_reported".into(),
            },
        )?;
    }
    Ok(())
}

/// Pure context reconstruction still needs a durable intent before local work.
pub(crate) fn begin_rebuild(
    state: &mut SessionSnapshot,
    agent_id: &str,
    step_id: &str,
) -> Result<(), CoreError> {
    let turn = state
        .agents
        .get(agent_id)
        .and_then(|agent| agent.turn.as_ref())
        .ok_or_else(|| CoreError::rejected(ErrorCode::Busy, "cost work turn missing"))?;
    let step = turn
        .steps
        .last()
        .filter(|step| step.step_id == step_id)
        .ok_or_else(|| CoreError::rejected(ErrorCode::StaleRevision, "cost work step changed"))?;
    let entry = work(agent_id, turn, step, CostWorkKind::ContextRebuild);
    if let Some(ledger) = state.cost_work.get_mut(&turn.run_id) {
        ledger.record(format!("{step_id}/rebuild"), entry)?;
    }
    Ok(())
}
