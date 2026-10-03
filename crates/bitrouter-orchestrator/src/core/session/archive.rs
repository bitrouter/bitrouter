//! Lossless recovery evidence outside the bounded online checkpoint. The core
//! hydrates and validates the complete evidence before using any lifecycle fact.

use super::*;
use crate::core::protocol::{ArtifactRef, RunActivityReconciliation, ToolObservation};
use base64::{Engine, engine::general_purpose::STANDARD};

const MEDIA_TYPE: &str = "application/vnd.bitrouter.recovery+json";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryArchive {
    schema_version: u32,
    session_id: String,
    run_id: Option<String>,
    activity: Vec<RunActivityReconciliation>,
    tools: BTreeMap<String, ArchivedTool>,
    dependencies: Vec<ArtifactRef>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchivedTool {
    attempt_id: String,
    revision: u64,
    current: Option<ToolObservation>,
    prior: Vec<ToolObservation>,
}

pub(super) struct Prepared {
    pub state: SessionSnapshot,
    pub blob: Option<Blob>,
}

pub(super) struct Blob {
    reference: ArtifactRef,
    bytes: Vec<u8>,
}

pub(super) fn prepare(
    state: &SessionSnapshot,
    compact: bool,
    limits: &Limits,
) -> Result<Prepared, CoreError> {
    let mut state = state.clone();
    if !compact && state.recovery_archive.is_none() {
        return Ok(Prepared { state, blob: None });
    }
    let mut record = RecoveryArchive {
        schema_version: VERSION,
        session_id: state.session_id.clone(),
        run_id: state.run.as_ref().map(|run| run.run_id.clone()),
        activity: state
            .run
            .as_mut()
            .map(|run| std::mem::take(&mut run.activity_reconciliations))
            .unwrap_or_default(),
        tools: BTreeMap::new(),
        dependencies: Vec::new(),
    };
    for call in state
        .agents
        .values_mut()
        .filter_map(|agent| agent.turn.as_mut())
        .flat_map(|turn| &mut turn.invocations)
    {
        if call.recovery_observation.is_some() || !call.prior_recovery_observations.is_empty() {
            record.tools.insert(
                call.dispatch.invocation_id.clone(),
                ArchivedTool {
                    attempt_id: call.dispatch.attempt_id.clone(),
                    revision: call.recovery_observation_revision,
                    current: call.recovery_observation.take(),
                    prior: std::mem::take(&mut call.prior_recovery_observations),
                },
            );
        }
    }
    record.dependencies = dependencies(&record)?;
    let bytes = serde_json::to_vec(&record).map_err(json_error)?;
    if bytes.len() as u64
        > state
            .manifest
            .artifact_quota_bytes
            .min(limits.unacknowledged_bytes)
    {
        return Err(reject(
            ErrorCode::LimitExceeded,
            "recovery archive exceeds artifact or restoration bound",
        ));
    }
    let hash = sha256(&bytes);
    let reference = ArtifactRef {
        artifact_id: format!("recovery-{hash}"),
        sha256: hash,
        bytes: bytes.len() as u64,
        media_type: MEDIA_TYPE.into(),
    };
    state.recovery_archive = Some(reference.clone());
    Ok(Prepared {
        state,
        blob: Some(Blob { reference, bytes }),
    })
}

fn dependencies(record: &RecoveryArchive) -> Result<Vec<ArtifactRef>, CoreError> {
    Ok(recovery::artifact_map(
        record
            .tools
            .values()
            .flat_map(|tool| tool.current.iter().chain(&tool.prior))
            .flat_map(|observation| observation.evidence.iter().cloned()),
    )?
    .into_values()
    .collect())
}

/// Sending chunks only stages an object. The subsequent checkpoint ACK must
/// certify its exact bytes and all transitive dependencies as durable.
pub(super) async fn persist(
    blob: Option<&Blob>,
    manifest: &HarnessManifest,
    harness: &dyn HarnessPort,
) -> Result<(), CoreError> {
    let Some(blob) = blob else { return Ok(()) };
    let chunk = usize::try_from(manifest.max_artifact_chunk_bytes)
        .map_err(|_| reject(ErrorCode::LimitExceeded, "artifact chunk bound is invalid"))?;
    if chunk == 0 {
        return Err(reject(
            ErrorCode::LimitExceeded,
            "artifact chunk bound is zero",
        ));
    }
    let mut offset = 0;
    for content in blob.bytes.chunks(chunk) {
        harness
            .send(ServerMessage::ArtifactPut {
                reference: blob.reference.clone(),
                offset,
                content_base64: STANDARD.encode(content),
            })
            .await?;
        offset += content.len() as u64;
    }
    Ok(())
}

pub(super) async fn hydrate(
    payload: &mut CheckpointPayload,
    harness: &dyn HarnessPort,
    limits: &Limits,
) -> Result<(), CoreError> {
    let mut state: SessionSnapshot =
        serde_json::from_value(payload.checkpoint.state.clone()).map_err(json_error)?;
    if state.session_id != payload.identity.session_id {
        return Err(reject(
            ErrorCode::UnauthorizedScope,
            "snapshot and archive session differ",
        ));
    }
    let Some(reference) = state.recovery_archive.clone() else {
        return Ok(());
    };
    if reference.media_type != MEDIA_TYPE
        || reference.artifact_id != format!("recovery-{}", reference.sha256)
        || !payload.checkpoint.artifact_refs.contains(&reference)
        || reference.bytes > state.manifest.artifact_quota_bytes
        || reference.bytes > limits.unacknowledged_bytes
    {
        return Err(reject(
            ErrorCode::ArtifactUnavailable,
            "invalid or oversized recovery archive",
        ));
    }
    // The host's read contract verifies the root's transitive availability;
    // flattening its dependencies back into Restore would recreate the limit.
    let mut bytes = Vec::new();
    while (bytes.len() as u64) < reference.bytes {
        let remaining = reference.bytes - bytes.len() as u64;
        let count = remaining
            .min(state.manifest.max_artifact_chunk_bytes)
            .min(limits.input_bytes / 2);
        if count == 0 {
            return Err(reject(
                ErrorCode::ArtifactUnavailable,
                "invalid archive read bound",
            ));
        }
        let chunk = harness
            .read_artifact(&reference, bytes.len() as u64, count)
            .await?;
        if chunk.is_empty() || chunk.len() as u64 > count {
            return Err(reject(
                ErrorCode::ArtifactUnavailable,
                "incomplete or oversized artifact chunk",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    if sha256(&bytes) != reference.sha256 {
        return Err(reject(
            ErrorCode::ArtifactUnavailable,
            "recovery archive digest differs",
        ));
    }
    let mut record: RecoveryArchive = serde_json::from_slice(&bytes).map_err(json_error)?;
    if record.schema_version != VERSION
        || record.session_id != state.session_id
        || record.run_id != state.run.as_ref().map(|run| run.run_id.clone())
        || record.dependencies != dependencies(&record)?
    {
        return Err(reject(
            ErrorCode::CheckpointConflict,
            "recovery archive identity or dependencies differ",
        ));
    }
    if let Some(run) = &mut state.run {
        if !run.activity_reconciliations.is_empty() {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "inline and archived activity overlap",
            ));
        }
        run.activity_reconciliations = record.activity;
    } else if !record.activity.is_empty() {
        return Err(reject(
            ErrorCode::CheckpointConflict,
            "archived activity has no run",
        ));
    }
    for call in state
        .agents
        .values_mut()
        .filter_map(|agent| agent.turn.as_mut())
        .flat_map(|turn| &mut turn.invocations)
    {
        if call.recovery_observation.is_some() || !call.prior_recovery_observations.is_empty() {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "inline and archived tool evidence overlap",
            ));
        }
        if let Some(tool) = record.tools.remove(&call.dispatch.invocation_id) {
            if tool.attempt_id != call.dispatch.attempt_id
                || tool.revision != call.recovery_observation_revision
            {
                return Err(reject(
                    ErrorCode::CheckpointConflict,
                    "archived tool identity or revision differs",
                ));
            }
            call.recovery_observation = tool.current;
            call.prior_recovery_observations = tool.prior;
        } else if call.recovery_observation_revision != 0 {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "archived tool evidence is missing",
            ));
        }
    }
    if !record.tools.is_empty() {
        return Err(reject(
            ErrorCode::CheckpointConflict,
            "archive contains an unknown invocation",
        ));
    }
    payload.checkpoint.state = encode(&state)?;
    Ok(())
}

pub(super) fn pad_reference(state: &mut SessionSnapshot) {
    if let Some(reference) = &mut state.recovery_archive {
        reference.bytes = u64::MAX;
    }
}
