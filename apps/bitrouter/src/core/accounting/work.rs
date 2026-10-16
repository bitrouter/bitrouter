//! Durable cost exposure from acknowledged execution records. This inventory
//! retains retired work; it neither charges callers nor invents provider bills.

use crate::storage::checkpoint::snapshot::accounting::work::{
    CostWork, CostWorkKind, CostWorkState, ProviderAttemptSource, RunCostWork,
};

use crate::storage::checkpoint::snapshot::AgentTurn;
use crate::storage::checkpoint::snapshot::ModelStep;
use crate::storage::checkpoint::snapshot::SessionSnapshot;
use bitrouter_ai::core::protocol::{CoreError, ErrorCode};
use bitrouter_ai::harness::inspection::AgentStatus;

impl RunCostWork {
    fn record(&mut self, id: String, next: CostWork) -> Result<(), CoreError> {
        if let Some(prior) = self.work.get(&id) {
            let same_identity = prior.agent_id == next.agent_id
                && prior.agent_turn_id == next.agent_turn_id
                && prior.step_id == next.step_id
                && prior.kind == next.kind
                && (prior.provider_source.is_none()
                    || prior.provider_source == next.provider_source)
                && (prior.request_id.is_none() || prior.request_id == next.request_id);
            // Old snapshots can acquire commitments from still-retained frozen
            // steps, but existing commitments and monetary evidence cannot change.
            let mut comparable = prior.clone();
            if comparable.provider_source.is_none() {
                comparable.provider_source = next.provider_source.clone();
            }
            if comparable.outcome_sha256.is_none() {
                comparable.outcome_sha256 = next.outcome_sha256.clone();
            }
            if comparable.generation_usage.is_none() {
                comparable.generation_usage = next.generation_usage.clone();
            }
            let same_outcome = prior.state != CostWorkState::OutcomeRecorded || comparable == next;
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
        generation_usage: None,
        decision_usage: None,
        decision_estimate_micro_usd: None,
        unknown_cost_reason: "cost_not_reported".into(),
        provider_source: None,
        outcome_sha256: None,
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
                let scope = if record.work.kind == crate::gateway::pipeline::native_preparation::NativePreparationWorkKind::PromptTransform {
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
                entry.provider_source = step.plan.as_ref().and_then(|plan| {
                    plan.routes
                        .get(attempt.index as usize)
                        .map(|route| ProviderAttemptSource {
                            attempt_index: attempt.index,
                            route: route.clone(),
                            canonical_output_bytes: attempt.canonical_output_bytes,
                            canonical_output_version: attempt.canonical_output_version,
                            attempt_report_bytes: attempt.attempt_report_bytes,
                        })
                });
                if let Some(receipt) = &attempt.receipt {
                    entry.state = CostWorkState::OutcomeRecorded;
                    entry.token_estimate = Some(receipt.report.token_cost.clone());
                    entry.generation_usage = receipt
                        .report
                        .result
                        .as_ref()
                        .and_then(|result| result.usage.clone());
                    entry.elapsed_ms = Some(receipt.report.elapsed_ms);
                    entry.outcome_sha256 = Some(report_digest(&receipt.report)?);
                }
                ledger.record(attempt.attempt_id.clone(), entry)?;
                for record in &attempt.provider_work {
                    use crate::gateway::pipeline::native_work::NativeProviderWorkKind;
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
                    generation_usage: None,
                    decision_usage: None,
                    decision_estimate_micro_usd: None,
                    unknown_cost_reason: "harness_cost_not_reported".into(),
                    provider_source: None,
                    outcome_sha256: None,
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
                generation_usage: None,
                decision_usage: None,
                decision_estimate_micro_usd: None,
                unknown_cost_reason: "harness_cost_not_reported".into(),
                provider_source: None,
                outcome_sha256: None,
            },
        )?;
    }
    for receipt in state.context_store.decisions.values() {
        let Some(ledger) = state.cost_work.get_mut(&receipt.run_id) else {
            continue;
        };
        let usage = receipt.outcome.as_ref().and_then(|outcome| match outcome {
            Ok(response) => Some(response.usage),
            Err(error) => error.usage,
        });
        let estimate = usage
            .zip(receipt.pricing.as_ref())
            .and_then(|(usage, pricing)| {
                let amount = pricing.estimate(usage) * 1_000_000.0;
                (amount.is_finite() && amount >= 0.0 && amount < u64::MAX as f64)
                    .then(|| amount.ceil() as u64)
            });
        let outcome_sha256 = receipt
            .outcome
            .as_ref()
            .map(|outcome| {
                serde_json::to_vec(outcome)
                    .map(|bytes| crate::storage::checkpoint::sha256(&bytes))
                    .map_err(|_| {
                        CoreError::rejected(
                            ErrorCode::CheckpointConflict,
                            "decision outcome cannot be encoded",
                        )
                    })
            })
            .transpose()?;
        ledger.record(
            receipt.decision_id.clone(),
            CostWork {
                agent_id: receipt.agent_id.clone(),
                agent_turn_id: receipt.task_id.clone(),
                step_id: None,
                request_id: None,
                kind: CostWorkKind::DecisionModel,
                state: if receipt.outcome.is_some() {
                    CostWorkState::OutcomeRecorded
                } else {
                    CostWorkState::IntentRecorded
                },
                elapsed_ms: receipt.elapsed_ms,
                token_estimate: None,
                generation_usage: None,
                decision_usage: usage,
                decision_estimate_micro_usd: estimate,
                unknown_cost_reason: if usage.is_some() {
                    "decision_bill_not_reported"
                } else {
                    "decision_usage_and_bill_unknown"
                }
                .into(),
                provider_source: None,
                outcome_sha256,
            },
        )?;
    }
    Ok(())
}

pub(crate) fn report_digest(
    report: &bitrouter_ai::core::evidence::NativeAttemptReport,
) -> Result<String, CoreError> {
    serde_json::to_vec(report)
        .map(|bytes| crate::storage::checkpoint::sha256(&bytes))
        .map_err(|_| {
            CoreError::rejected(
                ErrorCode::CheckpointConflict,
                "attempt report cannot be encoded",
            )
        })
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
