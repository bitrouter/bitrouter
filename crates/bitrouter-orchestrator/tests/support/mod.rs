use std::collections::{BTreeMap, BTreeSet};

use bitrouter_orchestrator::core::checkpoint::{
    CheckpointAck, CheckpointBatch, DurableHead, ToolStartFence, sha256,
};
use bitrouter_orchestrator::core::protocol::{
    ArtifactRef, CoreError, ErrorCode, Limits, OwnershipGrant,
};

/// Deterministic fake durable authority. Cloning models a process restart with
/// the same committed store. The local tool ledger is separate from core events.
#[derive(Clone)]
pub struct DurableHarness {
    pub grant: OwnershipGrant,
    pub head: DurableHead,
    pub batches: Vec<CheckpointBatch>,
    pub acknowledgements: BTreeMap<String, CheckpointAck>,
    pub artifacts: BTreeMap<String, ArtifactRef>,
    pub artifact_bytes: BTreeMap<String, Vec<u8>>,
    pub staged_artifacts: BTreeMap<String, (ArtifactRef, Vec<u8>)>,
    pub limits: Limits,
    pub fail_next_commit: bool,
    pub tool_start_fences: BTreeSet<ToolStartFence>,
    pub started_tools: BTreeSet<ToolStartFence>,
}

impl DurableHarness {
    pub fn new(grant: OwnershipGrant) -> Self {
        Self {
            grant,
            head: DurableHead::default(),
            batches: Vec::new(),
            acknowledgements: BTreeMap::new(),
            artifacts: BTreeMap::new(),
            artifact_bytes: BTreeMap::new(),
            staged_artifacts: BTreeMap::new(),
            limits: Limits::default(),
            fail_next_commit: false,
            tool_start_fences: BTreeSet::new(),
            started_tools: BTreeSet::new(),
        }
    }

    pub fn commit(&mut self, batch: &CheckpointBatch) -> Result<CheckpointAck, CoreError> {
        for reference in &batch.decode(&self.limits)?.checkpoint.artifact_refs {
            self.require_artifact(reference, &mut BTreeSet::new())?;
        }
        let retained = self.acknowledgements.get(&batch.identity.batch_id);
        let ack = batch.validate_append(
            &self.grant,
            &self.head,
            &self.limits,
            &self.artifacts,
            retained,
        )?;
        if let Some(previous) = self
            .batches
            .iter()
            .find(|previous| previous.identity.batch_id == batch.identity.batch_id)
            && previous != batch
        {
            return Err(CoreError::rejected(
                ErrorCode::CheckpointConflict,
                "batch ID was previously used",
            ));
        }
        if retained.is_some() {
            return Ok(ack);
        }
        if self.fail_next_commit {
            self.fail_next_commit = false;
            return Err(CoreError::rejected(
                ErrorCode::CheckpointUnavailable,
                "injected atomic commit failure",
            ));
        }
        if self.head != ack.head() {
            self.tool_start_fences
                .extend(batch.decode(&self.limits)?.tool_start_fences);
            self.batches.push(batch.clone());
            self.acknowledgements
                .insert(batch.identity.batch_id.clone(), ack.clone());
            self.head = ack.head();
        }
        Ok(ack)
    }

    /// Called under the same store lock as commit, at the actual start boundary
    /// after approval. False means no new effect, including duplicate commands.
    pub fn try_start_tool(&mut self, identity: ToolStartFence) -> bool {
        !self.tool_start_fences.contains(&identity) && self.started_tools.insert(identity)
    }

    pub fn put_artifact(&mut self, reference: ArtifactRef, bytes: &[u8]) -> Result<(), CoreError> {
        if sha256(bytes) != reference.sha256 || bytes.len() as u64 != reference.bytes {
            return Err(CoreError::rejected(
                ErrorCode::ArtifactUnavailable,
                "incomplete or corrupt artifact",
            ));
        }
        if let Some(previous) = self.artifacts.get(&reference.artifact_id)
            && previous != &reference
        {
            return Err(CoreError::rejected(
                ErrorCode::OperationConflict,
                "immutable artifact identity already exists",
            ));
        }
        self.artifacts
            .insert(reference.artifact_id.clone(), reference.clone());
        self.artifact_bytes
            .insert(reference.artifact_id.clone(), bytes.to_vec());
        Ok(())
    }

    pub fn put_chunk(
        &mut self,
        reference: ArtifactRef,
        offset: u64,
        chunk: &[u8],
    ) -> Result<(), CoreError> {
        let invalid =
            || CoreError::rejected(ErrorCode::ArtifactUnavailable, "invalid artifact chunk");
        let offset = usize::try_from(offset).map_err(|_| invalid())?;
        let end = offset.checked_add(chunk.len()).ok_or_else(invalid)?;
        if end as u64 > reference.bytes || reference.bytes > self.limits.unacknowledged_bytes {
            return Err(invalid());
        }
        if let Some(existing) = self.artifacts.get(&reference.artifact_id) {
            return if existing == &reference
                && self
                    .artifact_bytes
                    .get(&reference.artifact_id)
                    .and_then(|bytes| bytes.get(offset..end))
                    == Some(chunk)
            {
                self.require_artifact(&reference, &mut BTreeSet::new())
            } else {
                Err(invalid())
            };
        }
        let (expected, bytes) = self
            .staged_artifacts
            .entry(reference.artifact_id.clone())
            .or_insert_with(|| (reference.clone(), Vec::new()));
        if expected != &reference || offset > bytes.len() {
            return Err(invalid());
        }
        let shared = bytes.len().min(end);
        if bytes[offset..shared] != chunk[..shared - offset] {
            return Err(invalid());
        }
        bytes.extend_from_slice(&chunk[shared - offset..]);
        if bytes.len() as u64 == reference.bytes {
            let (_, bytes) = self
                .staged_artifacts
                .remove(&reference.artifact_id)
                .ok_or_else(invalid)?;
            self.put_artifact(reference.clone(), &bytes)?;
            self.require_artifact(&reference, &mut BTreeSet::new())?;
        }
        Ok(())
    }

    pub fn read_artifact(
        &self,
        reference: &ArtifactRef,
        offset: u64,
        max_bytes: u64,
    ) -> Result<Vec<u8>, CoreError> {
        self.require_artifact(reference, &mut BTreeSet::new())?;
        let bytes = self
            .artifact_bytes
            .get(&reference.artifact_id)
            .ok_or_else(|| {
                CoreError::rejected(ErrorCode::ArtifactUnavailable, "artifact body missing")
            })?;
        let start = usize::try_from(offset).map_err(|_| {
            CoreError::rejected(ErrorCode::ArtifactUnavailable, "artifact offset invalid")
        })?;
        let end = offset.saturating_add(max_bytes).min(bytes.len() as u64) as usize;
        bytes.get(start..end).map(<[u8]>::to_vec).ok_or_else(|| {
            CoreError::rejected(ErrorCode::ArtifactUnavailable, "artifact range invalid")
        })
    }

    fn require_artifact(
        &self,
        reference: &ArtifactRef,
        visited: &mut BTreeSet<String>,
    ) -> Result<(), CoreError> {
        let missing = || {
            CoreError::rejected(
                ErrorCode::ArtifactUnavailable,
                "artifact dependency missing or corrupt",
            )
        };
        if self.artifacts.get(&reference.artifact_id) != Some(reference) {
            return Err(missing());
        }
        let bytes = self
            .artifact_bytes
            .get(&reference.artifact_id)
            .ok_or_else(missing)?;
        if bytes.len() as u64 != reference.bytes || sha256(bytes) != reference.sha256 {
            return Err(missing());
        }
        if !visited.insert(reference.artifact_id.clone()) {
            return Ok(());
        }
        if reference.media_type == "application/vnd.bitrouter.recovery+json" {
            let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| missing())?;
            let dependencies: Vec<ArtifactRef> =
                serde_json::from_value(value.get("dependencies").cloned().ok_or_else(missing)?)
                    .map_err(|_| missing())?;
            for dependency in &dependencies {
                self.require_artifact(dependency, visited)?;
            }
        }
        Ok(())
    }
}
