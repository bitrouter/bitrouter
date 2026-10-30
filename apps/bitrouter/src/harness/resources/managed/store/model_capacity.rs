//! A replacement projection, never current checkpoint plus the same future growth.
use super::local::Journal;
use crate::{core::session::native_capacity::Projection, storage::checkpoint::serialized_bytes};
use bitrouter_ai::core::protocol::{CoreError, ErrorCode};

fn exhausted() -> CoreError {
    CoreError::rejected(
        ErrorCode::LimitExceeded,
        "native experiment ACK capacity exhausted",
    )
}
fn add(a: u64, b: u64) -> Result<u64, CoreError> {
    a.checked_add(b).ok_or_else(exhausted)
}
fn base64(bytes: u64) -> Result<u64, CoreError> {
    bytes
        .checked_add(2)
        .and_then(|n| n.checked_div(3))
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(exhausted)
}

pub(super) fn check(journal: &Journal, projection: &Projection) -> Result<(), CoreError> {
    // Replace the checkpoint, tool-start map and artifact bodies in the sizing
    // view. Re-adding the existing checkpoint here would spend its reservation
    // twice when a real result consumes the previously reserved space.
    let checkpoint_bytes = serialized_bytes(&journal.checkpoint)?;
    let starts_bytes = serialized_bytes(&journal.starts)?;
    let artifact_bytes = serialized_bytes(&journal.artifacts)?;
    let fixed = serialized_bytes(journal)?
        .checked_sub(checkpoint_bytes)
        .and_then(|n| n.checked_sub(starts_bytes))
        .and_then(|n| n.checked_sub(artifact_bytes))
        .ok_or_else(exhausted)?;
    let checkpoint = checkpoint_bytes.max(add(base64(projection.checkpoint)?, 4096)?);
    let mut starts = serialized_bytes(&journal.starts)?;
    for (id, reserved) in &projection.tools {
        match journal.starts.get(id) {
            Some(started)
                if started.result.as_ref().is_some_and(|result| {
                    result.status != bitrouter_ai::core::protocol::ToolOutcome::EffectUnknown
                }) => {}
            Some(started) => {
                let current = serialized_bytes(&started.result)?;
                starts = starts.checked_sub(current).ok_or_else(exhausted)?;
                starts = add(starts, current.max(reserved.result))?;
            }
            None => {
                starts = add(
                    starts,
                    add(
                        add(reserved.command, reserved.result)?,
                        add(serialized_bytes(id)?, 256)?,
                    )?,
                )?
            }
        }
    }
    for (id, started) in &journal.starts {
        if started.result.as_ref().is_none_or(|result| {
            result.status == bitrouter_ai::core::protocol::ToolOutcome::EffectUnknown
        }) && !projection.tools.contains_key(id)
        {
            return Err(CoreError::rejected(
                ErrorCode::CheckpointConflict,
                "unresolved native effect has no Core reservation",
            ));
        }
    }
    let metadata = journal.artifacts.values().try_fold(0, |bytes, artifact| {
        add(
            bytes,
            add(
                serialized_bytes(&artifact.reference)?,
                add(serialized_bytes(&artifact.reference.artifact_id)?, 128)?,
            )?,
        )
    })?;
    // Every newly committed reference is also in the projected checkpoint/tool
    // evidence. A second checkpoint allowance covers its journal-map metadata,
    // including escaped media types; existing unreferenced uploads count above.
    let artifacts = add(
        add(base64(projection.artifacts)?, metadata)?,
        projection.checkpoint,
    )?;
    // Fixed-width head/ACK numbers, fences and owned Choice outcome digests are
    // also represented in Core's projection. Retain the larger existing scalars.
    let bytes = add(
        add(add(fixed, checkpoint)?, starts)?,
        add(artifacts, projection.checkpoint)?,
    )?;
    crate::tenancy::experiment::check_native_allowance(bytes).map_err(|_| exhausted())
}
