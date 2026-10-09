//! Admission for completed preparation, counter, validation and integration
//! work. Projections reserve bytes; they never become execution evidence.

use super::*;
use crate::core::checkpoint::serialized_bytes;
use bitrouter_sdk::language_model::native::{
    NativeEvidenceCommitment, NativeInputCount, NativeInputCountRejection,
    auxiliary_report_allowance,
};

pub(super) const VERSION: u32 = 1;
const FAILURE_BYTES: u64 = 1024;
const FAILURE_OMITTED: &str = "model step failed: oversized diagnostic retained as a commitment";

fn invalid() -> CoreError {
    reject(
        ErrorCode::CheckpointConflict,
        "invalid auxiliary outcome contract",
    )
}

fn add(left: u64, right: u64) -> Result<u64, CoreError> {
    left.checked_add(right).ok_or_else(|| {
        reject(
            ErrorCode::LimitExceeded,
            "auxiliary outcome reservation exhausted",
        )
    })
}

fn commitment_valid(value: &NativeEvidenceCommitment) -> bool {
    value.bytes > 0
        && value.sha256.len() == 64
        && value
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn report_reservation(request_id: &str, report: Option<&impl Serialize>) -> Result<u64, CoreError> {
    let limit = auxiliary_report_allowance(request_id).map_err(|_| invalid())?;
    if let Some(report) = report {
        if serialized_bytes(report)? > limit {
            return Err(invalid());
        }
        return Ok(0);
    }
    // State and outcome event, plus wider elapsed/count accounting fields.
    add(limit.checked_mul(2).ok_or_else(invalid)?, 256)
}

pub(super) fn reserved(state: &SessionSnapshot) -> Result<u64, CoreError> {
    let mut bytes = 0;
    for agent in state.agents.values() {
        let Some(turn) = &agent.turn else { continue };
        for step in &turn.steps {
            if step
                .auxiliary_output_version
                .is_some_and(|version| version != VERSION)
            {
                return Err(invalid());
            }
            let bounded = step.auxiliary_output_version == Some(VERSION);
            if let Some(diagnostic) = &step.failure_diagnostic
                && (!bounded || diagnostic.bytes <= FAILURE_BYTES || !commitment_valid(diagnostic))
            {
                return Err(invalid());
            }
            if !bounded {
                if step.input_counts.iter().any(|record| {
                    record
                        .report
                        .as_ref()
                        .is_some_and(|report| report.report_rejection.is_some())
                }) {
                    return Err(invalid());
                }
                continue;
            }
            // A callback can fail before a provider attempt exists. Reserve its
            // terminal reason/conclusion/event and the diagnostic commitment.
            if !step.settled && !turn.status.terminal() {
                bytes = add(bytes, FAILURE_BYTES * 3 + 512)?;
            }
            for record in &step.preparation_work {
                if record
                    .report
                    .as_ref()
                    .is_some_and(|report| report.work != record.work)
                {
                    return Err(invalid());
                }
                bytes = add(
                    bytes,
                    report_reservation(&record.work.request_id, record.report.as_ref())?,
                )?;
            }
            for record in &step.input_counts {
                let plan = step.count_plan.as_ref().ok_or_else(invalid)?;
                if let Some(report) = &record.report {
                    if report.request_id != plan.request_id
                        || report.route_index != record.route_index
                    {
                        return Err(invalid());
                    }
                    if let Some(rejection) = &report.report_rejection {
                        let limit =
                            auxiliary_report_allowance(&plan.request_id).map_err(|_| invalid())?;
                        if rejection.version != 1
                            || rejection.byte_limit != limit
                            || rejection.original.bytes <= limit
                            || !commitment_valid(&rejection.original)
                            || !matches!(&report.outcome, NativeInputCount::Unavailable { reason } if reason == NativeInputCountRejection::REASON)
                        {
                            return Err(invalid());
                        }
                    }
                }
                bytes = add(
                    bytes,
                    report_reservation(&plan.request_id, record.report.as_ref())?,
                )?;
            }
            if let Some(record) = &step.context_validation {
                if record.report.as_ref().is_some_and(|report| {
                    report.request_id != record.request_id
                        || report.allowed == report.error_code.is_some()
                }) {
                    return Err(invalid());
                }
                bytes = add(
                    bytes,
                    report_reservation(&record.request_id, record.report.as_ref())?,
                )?;
                if record.report.is_none() {
                    // Validation can atomically activate the known candidate
                    // history. Do not rely on earlier history shrinking.
                    bytes = add(bytes, serialized_bytes(&step.input_history)?)?;
                }
            }
            for record in step
                .attempts
                .iter()
                .flat_map(|attempt| &attempt.provider_work)
            {
                if record
                    .report
                    .as_ref()
                    .is_some_and(|report| report.work != record.work)
                {
                    return Err(invalid());
                }
                bytes = add(
                    bytes,
                    report_reservation(&record.work.request_id, record.report.as_ref())?,
                )?;
            }
        }
    }
    Ok(bytes)
}

pub(super) fn failure(step: Option<&mut ModelStep>, reason: &str) -> Result<String, CoreError> {
    let Some(step) = step.filter(|step| step.auxiliary_output_version == Some(VERSION)) else {
        return Ok(reason.into());
    };
    if serialized_bytes(&reason)? <= FAILURE_BYTES {
        return Ok(reason.into());
    }
    step.failure_diagnostic =
        Some(NativeEvidenceCommitment::capture(&reason).map_err(|_| invalid())?);
    Ok(FAILURE_OMITTED.into())
}

pub(super) fn terminal_reason(step: &ModelStep) -> Option<&'static str> {
    if step.auxiliary_output_version != Some(VERSION) {
        return None;
    }
    if step.preparation_work.iter().any(|record| {
        record
            .report
            .as_ref()
            .is_some_and(|report| report.error_code.is_some())
    }) {
        return Some("preparation callback failed");
    }
    if step
        .context_validation
        .as_ref()
        .and_then(|record| record.report.as_ref())
        .is_some_and(|report| !report.allowed)
    {
        return Some("rebuilt context validation failed");
    }
    if step.input_counts.iter().any(|record| {
        record
            .report
            .as_ref()
            .is_some_and(|report| report.report_rejection.is_some())
    }) {
        return Some(NativeInputCountRejection::REASON);
    }
    None
}

pub(super) fn validate_history(
    payload: &CheckpointPayload,
    history: &mut BTreeMap<String, Option<u32>>,
) -> Result<(), CoreError> {
    let state: SessionSnapshot =
        serde_json::from_value(payload.checkpoint.state.clone()).map_err(json_error)?;
    reserved(&state)?;
    for step in state
        .agents
        .values()
        .filter_map(|agent| agent.turn.as_ref())
        .flat_map(|turn| &turn.steps)
    {
        if history
            .insert(step.step_id.clone(), step.auxiliary_output_version)
            .is_some_and(|prior| prior != step.auxiliary_output_version)
        {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "auxiliary outcome policy changed",
            ));
        }
    }
    Ok(())
}
