//! Canonical result and delivery contributions reserved before dispatch.
//! Tool intents, later prompts and physical allocations retain
//! separate admission obligations; no placeholder here is execution evidence.

use super::*;
use crate::core::accounting::work::CostWorkState;
use crate::core::checkpoint::serialized_bytes;

pub(super) const DELIVERY_VERSION: u32 = 2;
pub(super) const CURRENT_VERSION: u32 = 3;

pub(super) fn allowance(limits: &Limits) -> Result<u64, CoreError> {
    policy_allowance(limits, Some(CURRENT_VERSION))
}

fn policy_allowance(limits: &Limits, version: Option<u32>) -> Result<u64, CoreError> {
    let copies = match version {
        None => 2,
        // Receipt/event, history, turn/run answers, response output/answer and
        // a terminal event. Additional wait targets are admitted dynamically.
        Some(DELIVERY_VERSION) => 8,
        // Complete reports also enter receipts, events and the cost inventory.
        Some(CURRENT_VERSION) => 16,
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
                    attempt.attempt_report_bytes,
                );
                retained.insert(&attempt.attempt_id, contract);
                validate_limit(state, &turn.run_id, (contract.0, contract.1))?;
                if let Some(bytes) = contract.2 {
                    let plan = step.plan.as_ref().ok_or_else(|| {
                        reject(
                            ErrorCode::CheckpointConflict,
                            "report contract has no frozen plan",
                        )
                    })?;
                    let route = plan.routes.get(attempt.index as usize).ok_or_else(|| {
                        reject(
                            ErrorCode::CheckpointConflict,
                            "report contract has no frozen route",
                        )
                    })?;
                    validate_report_allowance(contract, bytes, &plan.request_id, route)?;
                } else if contract.1 == Some(CURRENT_VERSION) {
                    return Err(reject(
                        ErrorCode::CheckpointConflict,
                        "report contract is missing",
                    ));
                }
                let Some(limit) = contract.0 else { continue };
                let mut bytes = if let Some(receipt) = &attempt.receipt {
                    validate_report(Some(limit), contract.2, &receipt.report)?;
                    0
                } else {
                    pending_report_bytes(limit, contract.2)?
                };
                if matches!(contract.1, Some(DELIVERY_VERSION | CURRENT_VERSION))
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
                    if contract.2.is_some()
                        && let Some(error) = attempt
                            .receipt
                            .as_ref()
                            .and_then(|receipt| receipt.report.error.as_ref())
                    {
                        // A reported failure may become the turn reason, child
                        // conclusion and failure event before the step settles.
                        bytes = sum(bytes, times(sum(serialized_bytes(error)?, 128)?, 3)?)?;
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
                source.attempt_report_bytes,
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
            validate_limit(state, run_id, (contract.0, contract.1))?;
            if let Some(bytes) = contract.2 {
                let request_id = work.request_id.as_deref().ok_or_else(|| {
                    reject(
                        ErrorCode::CheckpointConflict,
                        "report inventory has no request",
                    )
                })?;
                validate_report_allowance(contract, bytes, request_id, &source.route)?;
            } else if contract.1 == Some(CURRENT_VERSION) {
                return Err(reject(
                    ErrorCode::CheckpointConflict,
                    "report inventory contract is missing",
                ));
            }
            let Some(limit) = contract.0 else { continue };
            if let Some(evidence) = state.provider_evidence.get(id) {
                validate_report(Some(limit), contract.2, &evidence.report)?;
            }
            if work.state == CostWorkState::IntentRecorded {
                pending
                    .entry(id)
                    .or_insert(pending_report_bytes(limit, contract.2)?);
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
    if version.is_some_and(|version| !matches!(version, DELIVERY_VERSION | CURRENT_VERSION))
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

pub(super) fn report_allowance(
    canonical: u64,
    request_id: &str,
    route: &bitrouter_sdk::language_model::native::NativeRoute,
) -> Result<u64, CoreError> {
    let envelope = NativeAttemptReport::rejection_byte_reserve(request_id, route)
        .map_err(|error| reject(ErrorCode::LimitExceeded, &error.to_string()))?;
    sum(canonical, envelope)
}

fn validate_report_allowance(
    contract: (Option<u64>, Option<u32>, Option<u64>),
    bytes: u64,
    request_id: &str,
    route: &bitrouter_sdk::language_model::native::NativeRoute,
) -> Result<(), CoreError> {
    if contract.1 != Some(CURRENT_VERSION)
        || contract
            .0
            .is_none_or(|canonical| report_allowance(canonical, request_id, route) != Ok(bytes))
    {
        return Err(reject(
            ErrorCode::CheckpointConflict,
            "attempt report allowance differs from frozen policy",
        ));
    }
    Ok(())
}

fn pending_report_bytes(canonical: u64, report: Option<u64>) -> Result<u64, CoreError> {
    match report {
        // Receipt/event, repeated cache source, ledger estimate and late evidence
        // together fit four report contributions. Three more cover a failed
        // report's terminal reason, conclusion and event before settlement.
        Some(bytes) => sum(times(bytes, 7)?, 4096),
        None => times(canonical, 2),
    }
}

pub(super) fn validate_report(
    limit: Option<u64>,
    report_limit: Option<u64>,
    report: &NativeAttemptReport,
) -> Result<(), CoreError> {
    validate_complete_report(report_limit, report)?;
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

fn validate_complete_report(
    limit: Option<u64>,
    report: &NativeAttemptReport,
) -> Result<(), CoreError> {
    use bitrouter_sdk::language_model::native::{
        NativeEvidenceCommitment, NativeReportRejectionReason,
    };
    use bitrouter_sdk::language_model::native_accounting::NativeTokenCost;
    let invalid = || {
        reject(
            ErrorCode::CheckpointConflict,
            "attempt report violates its frozen contract",
        )
    };
    let digest = |value: &NativeEvidenceCommitment| {
        value.bytes > 0
            && value.sha256.len() == 64
            && value
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    if let Some(limit) = limit
        && serialized_bytes(report)? > limit
    {
        return Err(invalid());
    }
    let Some(rejected) = &report.report_rejection else {
        return Ok(());
    };
    let limit = limit.ok_or_else(invalid)?;
    if rejected.version != 1
        || rejected.byte_limit != limit
        || !digest(&rejected.original)
        || (rejected.reason == NativeReportRejectionReason::ByteLimit
            && rejected.original.bytes <= limit)
        || rejected
            .actual_provider
            .as_ref()
            .is_some_and(|value| !digest(value))
        || rejected
            .actual_model
            .as_ref()
            .is_some_and(|value| !digest(value))
        || rejected.had_result != rejected.actual_provider.is_some()
        || rejected.had_result != rejected.actual_model.is_some()
        || (!rejected.had_result && rejected.usage.is_some())
        || report.result.is_some()
        || report.actual_provider.is_some()
        || report.actual_model.is_some()
        || report.error.as_deref() != report.rejection_reason()
        || report
            .output_rejection
            .as_ref()
            .is_some_and(|value| !rejected.had_result || value.usage != rejected.usage)
    {
        return Err(invalid());
    }
    match &report.token_cost {
        NativeTokenCost::UnknownCommitment { reason }
            if digest(reason)
                && rejected.reason != NativeReportRejectionReason::NonFinitePricing => {}
        NativeTokenCost::ConfiguredEstimateCommitment {
            rates,
            pricing_metadata,
            ..
        } if digest(pricing_metadata) => {
            let non_finite = [
                rates.uncached_input,
                rates.cache_read,
                rates.cache_write,
                rates.output,
            ]
            .into_iter()
            .flatten()
            .any(|rate| !f64::from_bits(rate).is_finite());
            if non_finite != (rejected.reason == NativeReportRejectionReason::NonFinitePricing) {
                return Err(invalid());
            }
        }
        _ => return Err(invalid()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_sdk::language_model::native::NativeRouteConstraints;

    #[test]
    fn report_rejection_contract_rejects_inconsistent_evidence()
    -> Result<(), Box<dyn std::error::Error>> {
        let digest = json!({"bytes":100,"sha256":"a".repeat(64)});
        let value = json!({
            "request_id":"request","attempt_index":0,"elapsed_ms":0,
            "route":{"provider":"provider","model":"model","protocol":"chat_completions","constraints":NativeRouteConstraints::default()},
            "error":"attempt report exceeds its admitted byte limit",
            "token_cost":{"status":"unknown_commitment","reason":digest},
            "report_rejection":{"version":1,"byte_limit":8192,"reason":"byte_limit",
                "original":{"bytes":8193,"sha256":"b".repeat(64)},"had_result":false}
        });
        let valid: NativeAttemptReport = serde_json::from_value(value.clone())?;
        validate_report(None, Some(8192), &valid)?;
        assert_eq!(
            validate_report(None, None, &valid)
                .err()
                .map(|error| error.code),
            Some(ErrorCode::CheckpointConflict)
        );
        for mode in [
            "version",
            "limit",
            "original_size",
            "digest",
            "empty_commitment",
            "partial_identity",
            "missing_identity",
            "deliverable_result",
            "original_error",
            "non_finite_without_rates",
            "uncommitted_cost",
            "report_overflow",
        ] {
            let mut forged = value.clone();
            match mode {
                "version" => forged["report_rejection"]["version"] = json!(2),
                "limit" => forged["report_rejection"]["byte_limit"] = json!(8193),
                "original_size" => forged["report_rejection"]["original"]["bytes"] = json!(8192),
                "digest" => {
                    forged["report_rejection"]["original"]["sha256"] = json!("z".repeat(64))
                }
                "empty_commitment" => forged["token_cost"]["reason"]["bytes"] = json!(0),
                "partial_identity" => {
                    forged["report_rejection"]["actual_provider"] = digest.clone()
                }
                "missing_identity" => forged["report_rejection"]["had_result"] = json!(true),
                "deliverable_result" => {
                    forged["result"] =
                        json!({"content":[],"finish_reason":"stop","provider_metadata":{}})
                }
                "original_error" => forged["error"] = json!("original provider error"),
                "non_finite_without_rates" => {
                    forged["report_rejection"]["reason"] = json!("non_finite_pricing")
                }
                "uncommitted_cost" => {
                    forged["token_cost"] = json!({"status":"unknown","reason":"missing"})
                }
                "report_overflow" => forged["cache"]["source"] = json!("x".repeat(8192)),
                _ => return Err("unknown corruption case".into()),
            }
            if mode == "report_overflow" {
                forged["cache"]["read_tokens"] = Value::Null;
                forged["cache"]["write_tokens"] = Value::Null;
                forged["cache"]["unknown_reason"] = Value::Null;
            }
            let forged: NativeAttemptReport = serde_json::from_value(forged)?;
            assert_eq!(
                validate_report(None, Some(8192), &forged)
                    .err()
                    .map(|error| error.code),
                Some(ErrorCode::CheckpointConflict),
                "{mode}"
            );
        }
        Ok(())
    }
}
