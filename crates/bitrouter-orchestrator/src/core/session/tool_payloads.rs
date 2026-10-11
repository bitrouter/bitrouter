//! Transport-independent bounds for outcomes and lifecycle evidence. Frozen
//! limits prevent later signals or host policy from enlarging an admitted reply.

use super::*;
use crate::core::protocol::ToolResultLimits;

pub(super) fn admit(
    state: &SessionSnapshot,
    host: &Limits,
    output_bytes: u64,
) -> Result<ToolResultLimits, CoreError> {
    let input_bytes = state.run.as_ref().map_or(host.input_bytes, |run| {
        run.limits.input_bytes.min(host.input_bytes)
    });
    let mut limits = ToolResultLimits::for_input(input_bytes, output_bytes)?;
    if state
        .manifest
        .required_features
        .iter()
        .any(|feature| feature == super::super::context_router::NATIVE_TOOLS)
    {
        // Native values offload large bodies; reserve a small, explicitly frozen
        // metadata envelope instead of the entire command input allowance.
        limits.payload_bytes = limits.payload_bytes.min(output_bytes.saturating_add(4096));
    }
    let tools = state
        .run
        .as_ref()
        .map_or(host.outstanding_tools, |run| run.limits.outstanding_tools);
    limits.artifact_bytes = Some(artifact_allowance(
        state.manifest.artifact_quota_bytes,
        tools,
    )?);
    Ok(limits)
}

pub(super) fn artifact_allowance(quota: u64, tools: u32) -> Result<u64, CoreError> {
    // Five first outcome/status slots per outstanding invocation, plus two
    // shares left for other objects. This is an allocation policy; admission
    // separately checks the total retained footprint and future obligations.
    let shares = u64::from(tools)
        .checked_mul(5)
        .and_then(|value| value.checked_add(2))
        .ok_or_else(|| {
            reject(
                ErrorCode::LimitExceeded,
                "artifact reservation count exhausted",
            )
        })?;
    Ok(quota / shares)
}

pub(super) fn limits(
    call: &Invocation,
    host: &Limits,
    run: Option<&RootRun>,
    input_limits: Option<&Limits>,
) -> Result<ToolResultLimits, CoreError> {
    let retained = run
        .filter(|run| run.run_id == call.dispatch.run_id)
        .map(|run| &run.limits)
        .or(input_limits);
    let input_bytes = retained.map_or(host.input_bytes, |limits| {
        limits.input_bytes.min(host.input_bytes)
    });
    let ceiling = ToolResultLimits::for_input(input_bytes, call.result_limit_bytes)?;
    let minimum = super::super::checkpoint::serialized_bytes(&ToolResult {
        invocation_id: call.dispatch.invocation_id.clone(),
        attempt_id: call.dispatch.attempt_id.clone(),
        status: ToolOutcome::EffectUnknown,
        output: String::new(),
        evidence: Vec::new(),
        workspace_revision: None,
    })?;
    let limits = match call.dispatch.result_limits {
        Some(frozen)
            if frozen.output_bytes == call.result_limit_bytes
                && frozen.output_bytes > 0
                && frozen.payload_bytes >= minimum
                && frozen.payload_bytes <= ceiling.payload_bytes =>
        {
            // Legacy commands retain an absent body bound: an execution that
            // was already authorized cannot acquire a smaller reply contract.
            // Their real references still count toward the retained quota,
            // but they have no retroactive artifact-body reservation.
            Ok(frozen)
        }
        Some(_) => Err(reject(
            ErrorCode::CheckpointConflict,
            "tool reply limits exceed admitted policy or differ from the intent",
        )),
        None if retained.is_some() => Ok(ceiling),
        None => Err(reject(
            ErrorCode::RecoveryRequired,
            "legacy tool intent has no retained run input policy",
        )),
    }?;
    if let Some(bytes) = call.recovery_archive_allowance
        && bytes != archive::observation_allowance(call, limits)?
    {
        return Err(reject(
            ErrorCode::CheckpointConflict,
            "recovery archive reservation differs from the frozen reply contract",
        ));
    }
    Ok(limits)
}

pub(super) fn validate_result(
    call: &Invocation,
    result: &ToolResult,
    limits: ToolResultLimits,
) -> Result<(), CoreError> {
    if call.dispatch.invocation_id != result.invocation_id
        || call.dispatch.attempt_id != result.attempt_id
    {
        return Err(reject(
            ErrorCode::InvalidToolResult,
            "tool result does not match invocation and attempt",
        ));
    }
    limits.validate_result(result)
}

/// Each first essential status and each uncertain/definite result has an
/// independent evidence allowance. Repeated optional reports need fresh room.
pub(super) fn remaining_artifact_slots(call: &Invocation) -> u64 {
    use super::super::protocol::ToolStatus;
    if call
        .result
        .as_ref()
        .is_some_and(|result| result.status != ToolOutcome::EffectUnknown)
    {
        return 0;
    }
    let stopped = tool_status::observed(call, ToolStatus::Stopped);
    let running = tool_status::observed(call, ToolStatus::Running);
    // An uncertain outcome does not consume the independent first uncertain
    // observation allowance; either message can arrive first.
    let uncertain = tool_status::observed(call, ToolStatus::EffectUnknown);
    let outcomes = if call.result.is_none() { 2 } else { 1 };
    outcomes
        + u64::from(!stopped)
        + u64::from(!stopped && !running && call.result.is_none())
        + u64::from(!uncertain)
}
