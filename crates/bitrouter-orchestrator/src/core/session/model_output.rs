//! Canonical result contributions reserved across concurrent provider attempts.
//! Other report metadata and future history/application growth still pass their
//! own admission checks; this is not an allocation lease for SDK internals.

use super::*;
use crate::core::accounting::work::CostWorkState;
use crate::core::checkpoint::serialized_bytes;

pub(super) fn allowance(limits: &Limits) -> Result<u64, CoreError> {
    // Each active attempt can contribute its result to a receipt and to the
    // outcome event. Leave one additional pair of shares for existing state.
    // This allocation policy is separate from total checkpoint admission.
    let shares = (u64::from(limits.active_models) + 1) * 2;
    let bytes = limits.checkpoint_bytes / shares;
    if bytes == 0 {
        return Err(reject(
            ErrorCode::LimitExceeded,
            "checkpoint has no canonical model output capacity",
        ));
    }
    Ok(bytes)
}

pub(super) fn reserved(state: &SessionSnapshot) -> Result<u64, CoreError> {
    let mut pending = BTreeMap::new();
    let mut retained = BTreeMap::new();
    for agent in state.agents.values() {
        let Some(turn) = &agent.turn else { continue };
        for attempt in turn.steps.iter().flat_map(|step| &step.attempts) {
            retained.insert(&attempt.attempt_id, attempt.canonical_output_bytes);
            let Some(limit) = attempt.canonical_output_bytes else {
                continue;
            };
            validate_limit(state, &turn.run_id, limit)?;
            if let Some(receipt) = &attempt.receipt {
                validate_report(Some(limit), &receipt.report)?;
            } else {
                pending.insert(&attempt.attempt_id, limit);
            }
        }
    }
    // The cost inventory outlives root runs and child turns. Retiring an
    // interrupted turn cannot release capacity still owed to late evidence.
    for (run_id, ledger) in &state.cost_work {
        for (id, work) in &ledger.work {
            let Some(source) = &work.provider_source else {
                continue;
            };
            if let Some(retained) = retained.get(id)
                && *retained != source.canonical_output_bytes
            {
                return Err(reject(
                    ErrorCode::CheckpointConflict,
                    "retained attempt and cost inventory disagree on output allowance",
                ));
            }
            let Some(limit) = source.canonical_output_bytes else {
                continue;
            };
            validate_limit(state, run_id, limit)?;
            if let Some(evidence) = state.provider_evidence.get(id) {
                validate_report(Some(limit), &evidence.report)?;
            }
            if work.state == CostWorkState::IntentRecorded {
                pending.insert(id, limit);
            }
        }
    }
    pending.values().try_fold(0u64, |total, limit| {
        limit
            .checked_mul(2)
            .and_then(|bytes| total.checked_add(bytes))
            .ok_or_else(|| {
                reject(
                    ErrorCode::LimitExceeded,
                    "model output reservation exhausted",
                )
            })
    })
}

fn validate_limit(state: &SessionSnapshot, run_id: &str, limit: u64) -> Result<(), CoreError> {
    if limit == 0
        || state
            .run
            .as_ref()
            .filter(|run| run.run_id == run_id)
            .is_some_and(|run| allowance(&run.limits) != Ok(limit))
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
