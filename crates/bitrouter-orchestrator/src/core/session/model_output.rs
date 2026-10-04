//! Canonical result and delivery contributions reserved before dispatch.
//! Tool intents, report metadata, later prompts and physical allocations retain
//! separate admission obligations; no placeholder here is execution evidence.

use super::*;
use crate::core::accounting::work::CostWorkState;
use crate::core::checkpoint::serialized_bytes;

pub(super) const DELIVERY_VERSION: u32 = 2;

pub(super) fn allowance(limits: &Limits) -> Result<u64, CoreError> {
    policy_allowance(limits, Some(DELIVERY_VERSION))
}

fn policy_allowance(limits: &Limits, version: Option<u32>) -> Result<u64, CoreError> {
    let copies = match version {
        None => 2,
        // Receipt/event, history, turn/run answers, response output/answer and
        // a terminal event. Additional wait targets are admitted dynamically.
        Some(DELIVERY_VERSION) => 8,
        _ => {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "unknown canonical output policy",
            ));
        }
    };
    let shares = (u64::from(limits.active_models) + 1) * copies;
    let bytes = limits.checkpoint_bytes / shares;
    if bytes == 0 {
        return Err(reject(
            ErrorCode::LimitExceeded,
            "checkpoint has no canonical model output capacity",
        ));
    }
    Ok(bytes)
}

fn sum(left: u64, right: u64) -> Result<u64, CoreError> {
    left.checked_add(right).ok_or_else(|| {
        reject(
            ErrorCode::LimitExceeded,
            "model output reservation exhausted",
        )
    })
}

fn times(bytes: u64, copies: u64) -> Result<u64, CoreError> {
    bytes.checked_mul(copies).ok_or_else(|| {
        reject(
            ErrorCode::LimitExceeded,
            "model output reservation exhausted",
        )
    })
}

pub(super) fn reserved(state: &SessionSnapshot) -> Result<u64, CoreError> {
    let mut pending = BTreeMap::new();
    let mut retained = BTreeMap::new();
    for agent in state.agents.values() {
        let Some(turn) = &agent.turn else { continue };
        for step in &turn.steps {
            for attempt in &step.attempts {
                let contract = (
                    attempt.canonical_output_bytes,
                    attempt.canonical_output_version,
                );
                retained.insert(&attempt.attempt_id, contract);
                validate_limit(state, &turn.run_id, contract)?;
                let Some(limit) = contract.0 else { continue };
                let mut bytes = if let Some(receipt) = &attempt.receipt {
                    validate_report(Some(limit), &receipt.report)?;
                    0
                } else {
                    times(limit, 2)?
                };
                if contract.1 == Some(DELIVERY_VERSION)
                    && !step.settled
                    && !turn.status.terminal()
                    && turn.status != AgentStatus::Cancelling
                {
                    let result_bytes = match &attempt.receipt {
                        Some(receipt) => receipt
                            .report
                            .result
                            .as_ref()
                            .map(serialized_bytes)
                            .transpose()?,
                        None => Some(limit),
                    };
                    if let Some(result_bytes) = result_bytes {
                        bytes = sum(bytes, delivery(state, agent, turn, step, result_bytes)?)?;
                    }
                }
                pending.insert(&attempt.attempt_id, bytes);
            }
        }
    }
    // Retired attempts owe their receipt/event, but cannot apply new history to
    // a replacement turn. Never replace a live delivery reservation with this
    // smaller ledger-only contribution.
    for (run_id, ledger) in &state.cost_work {
        for (id, work) in &ledger.work {
            let Some(source) = &work.provider_source else {
                continue;
            };
            let contract = (
                source.canonical_output_bytes,
                source.canonical_output_version,
            );
            if retained
                .get(id)
                .is_some_and(|retained| *retained != contract)
            {
                return Err(reject(
                    ErrorCode::CheckpointConflict,
                    "retained attempt and cost inventory disagree on output allowance",
                ));
            }
            validate_limit(state, run_id, contract)?;
            let Some(limit) = contract.0 else { continue };
            if let Some(evidence) = state.provider_evidence.get(id) {
                validate_report(Some(limit), &evidence.report)?;
            }
            if work.state == CostWorkState::IntentRecorded {
                pending.entry(id).or_insert(times(limit, 2)?);
            }
        }
    }
    pending
        .values()
        .try_fold(0, |total, bytes| sum(total, *bytes))
}

fn delivery(
    state: &SessionSnapshot,
    agent: &AgentState,
    turn: &AgentTurn,
    step: &ModelStep,
    bytes: u64,
) -> Result<u64, CoreError> {
    let root = agent.agent_id == state.agent_id;
    let response = responses::active_id(state).is_some();
    // Future history, provisional answer and one terminal event. Existing
    // cleanup projections already count the surrounding mailbox/wait objects;
    // add their as-yet unknown answer bytes while the attempt is unresolved.
    let wait_copies = times(
        state
            .waits
            .values()
            .filter(|wait| {
                wait.result.is_none() && wait.state.targets.contains_key(&agent.agent_id)
            })
            .count() as u64,
        2,
    )?;
    let copies = 3
        + u64::from(root)
        + u64::from(agent.parent_id.is_some() && !turn.notified)
        + u64::from(response)
        + u64::from(response && root)
        + wait_copies;
    let mut reserved = times(bytes, copies)?;
    // Both cleanup views begin exposing all retained sources once an answer
    // exists, including inherited sources not introduced by this model step.
    reserved = sum(
        reserved,
        times(serialized_bytes(&agent.context_sources)?, wait_copies)?,
    )?;
    let message = Message {
        role: Role::Assistant,
        content: Vec::new(),
    };
    reserved = sum(reserved, serialized_bytes(&message)?)?;
    let source = ContextSource::capture(&step.context);
    if !agent.context_sources.contains(&source) {
        // Source also enters the parent conclusion and pending wait results.
        reserved = sum(reserved, times(serialized_bytes(&source)?, copies)?)?;
    }
    let mut sources = serialized_bytes(&agent.context_sources)?;
    if !agent.context_sources.contains(&source) {
        sources = sum(sources, sum(serialized_bytes(&source)?, 1)?)?;
    }
    reserved = sum(
        reserved,
        wait_output::future(state, &agent.agent_id, bytes, sources)?,
    )?;
    if response {
        reserved = sum(
            reserved,
            serialized_bytes(&responses::ResponseOutput {
                event_seq: u64::MAX,
                agent_id: agent.agent_id.clone(),
                agent_name: agent.display_path.clone(),
                agent_turn_id: turn.agent_turn_id.clone(),
                step_id: step.step_id.clone(),
                message,
                call_ids: BTreeMap::new(),
            })?,
        )?;
    }
    Ok(reserved)
}

fn validate_limit(
    state: &SessionSnapshot,
    run_id: &str,
    contract: (Option<u64>, Option<u32>),
) -> Result<(), CoreError> {
    let (limit, version) = contract;
    if version.is_some_and(|version| version != DELIVERY_VERSION)
        || (version.is_some() && limit.is_none())
    {
        return Err(reject(
            ErrorCode::CheckpointConflict,
            "invalid canonical output policy",
        ));
    }
    let Some(limit) = limit else { return Ok(()) };
    if limit == 0
        || state
            .run
            .as_ref()
            .filter(|run| run.run_id == run_id)
            .is_some_and(|run| policy_allowance(&run.limits, version) != Ok(limit))
    {
        return Err(reject(
            ErrorCode::CheckpointConflict,
            "canonical output reservation differs from frozen run policy",
        ));
    }
    Ok(())
}

pub(super) fn validate_report(
    limit: Option<u64>,
    report: &NativeAttemptReport,
) -> Result<(), CoreError> {
    let Some(limit) = limit else { return Ok(()) };
    if report
        .result
        .as_ref()
        .map(serialized_bytes)
        .transpose()?
        .is_some_and(|bytes| bytes > limit)
        || report
            .output_rejection
            .as_ref()
            .is_some_and(|rejection| rejection.byte_limit != limit)
    {
        return Err(reject(
            ErrorCode::CheckpointConflict,
            "canonical model outcome exceeds its frozen output contract",
        ));
    }
    Ok(())
}
