//! Recover native execution from Core facts, retaining unselected evidence.

use super::{Active, RecoveredCall, Step};
use crate::core::protocol::ToolOutcome;
use crate::core::session::SessionSnapshot;
use crate::item::{CallOrigin, CallRecord};
use crate::store::{EffectStatus, ExecutionRecord};

pub(super) fn is_projection(fact: &ExecutionRecord, calls: &[RecoveredCall]) -> bool {
    match fact {
        ExecutionRecord::ModelRequest { .. }
        | ExecutionRecord::ModelResponse { .. }
        | ExecutionRecord::ModelInterrupted { .. } => true,
        ExecutionRecord::ToolIntent { call, .. } => call.origin == CallOrigin::Model,
        ExecutionRecord::ToolResult { item_id, .. } => !calls.iter().any(|call| {
            call.call.item_id == *item_id && call.call.origin == CallOrigin::Verification
        }),
        _ => false,
    }
}

pub(super) fn observe(
    active: &mut Active,
    snapshot: &SessionSnapshot,
    fact: &ExecutionRecord,
) -> Result<(), String> {
    active.native = true;
    let Some(run) = &snapshot.run else {
        return Ok(());
    };
    let ExecutionRecord::CoreCheckpoint { batch, limits } = fact else {
        return Err("native state has no checkpoint".into());
    };
    let payload = batch.decode(limits).map_err(|error| error.message)?;
    if payload
        .events
        .iter()
        .any(|event| event.kind == "input.accepted")
    {
        active.native_runs.insert(run.run_id.clone(), (0, 0));
    }
    // A restore/seed checkpoint can still describe the previous native Turn.
    let Some(counters) = active.native_runs.get_mut(&run.run_id) else {
        return Ok(());
    };
    // A later Core transition can start new paid work or clock intervals. Only
    // an explicit native RunCheckpoint/Settled fact seals the aggregate budget.
    active.exact_budget = false;
    *counters = (run.model_attempts, run.active_ms);
    let root = snapshot
        .agents
        .get(&snapshot.agent_id)
        .ok_or("native root agent missing")?;
    active.messages = root.history.clone();
    active.version = root.context_revision;
    active.budget.model_steps = active
        .native_runs
        .values()
        .fold(0_u32, |total, (steps, _)| total.saturating_add(*steps));
    active.budget.active_duration_ms = active.budget.active_duration_ms.max(
        active
            .native_runs
            .values()
            .fold(0_u64, |total, (_, ms)| total.saturating_add(*ms)),
    );
    for receipt in snapshot
        .context_store
        .decisions
        .values()
        .filter(|receipt| active.native_runs.contains_key(&receipt.run_id))
    {
        let unpriced = receipt.pricing.is_none()
            && receipt
                .outcome
                .as_ref()
                .is_none_or(|outcome| match outcome {
                    Ok(_) => true,
                    Err(error) => error.may_have_run,
                });
        if unpriced {
            active
                .native_unpriced_decisions
                .insert(receipt.decision_id.clone());
        } else {
            active
                .native_unpriced_decisions
                .remove(&receipt.decision_id);
        }
        let unknown = receipt
            .outcome
            .as_ref()
            .is_none_or(|outcome| match outcome {
                Ok(_) => false,
                Err(error) => error.may_have_run && error.usage.is_none(),
            });
        if unknown {
            active
                .native_unknown_usage
                .insert(receipt.decision_id.clone());
        } else {
            active.native_unknown_usage.remove(&receipt.decision_id);
        }
    }
    for agent in snapshot.agents.values() {
        let Some(turn) = agent.turn.as_ref().filter(|turn| turn.run_id == run.run_id) else {
            continue;
        };
        for step in &turn.steps {
            if step.plan.is_none() {
                continue;
            }
            active.steps.insert(
                step.step_id.clone(),
                Step {
                    item_id: step.step_id.clone(),
                    context_version: step.context_revision,
                    complete: step.settled,
                    interrupted: step.interrupted,
                    usage_known: !step.attempts.is_empty()
                        && step.attempts.iter().all(|attempt| {
                            attempt.receipt.as_ref().is_some_and(|receipt| {
                                receipt
                                    .report
                                    .result
                                    .as_ref()
                                    .is_some_and(|result| result.usage.is_some())
                                    && receipt.report.error.is_none()
                            })
                        }),
                },
            );
        }
        for invocation in &turn.invocations {
            let call = RecoveredCall {
                step_id: invocation.dispatch.step_id.clone(),
                call: CallRecord {
                    origin: CallOrigin::Model,
                    item_id: invocation.public_call_id.clone(),
                    provider_call_id: invocation.provider_call_id.clone(),
                    name: invocation.dispatch.tool.clone(),
                    arguments: invocation.dispatch.arguments.to_string(),
                },
                // Native execution crosses Running before its local start fence.
                intent: invocation
                    .tool_observations
                    .values()
                    .chain(invocation.recovery_observation.iter())
                    .chain(&invocation.prior_recovery_observations)
                    .any(|observation| {
                        matches!(
                            observation.status,
                            crate::core::protocol::ToolStatus::Running
                                | crate::core::protocol::ToolStatus::Stopped
                        )
                    }),
                result: invocation.result.as_ref().map(|result| {
                    (
                        None,
                        match result.status {
                            ToolOutcome::EffectUnknown => EffectStatus::Unknown,
                            ToolOutcome::NotExecuted | ToolOutcome::Denied => {
                                EffectStatus::NotExecuted
                            }
                            _ => EffectStatus::Completed,
                        },
                    )
                }),
            };
            if let Some(prior) = active
                .calls
                .iter_mut()
                .find(|prior| prior.call.item_id == call.call.item_id)
            {
                *prior = call;
            } else {
                active.calls.push(call);
            }
        }
    }
    Ok(())
}
