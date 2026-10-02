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
    ToolResultLimits::for_input(input_bytes, output_bytes)
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
    match call.dispatch.result_limits {
        Some(frozen)
            if frozen.output_bytes == call.result_limit_bytes
                && frozen.output_bytes > 0
                && frozen.payload_bytes >= minimum
                && frozen.payload_bytes <= ceiling.payload_bytes =>
        {
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
    }
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
