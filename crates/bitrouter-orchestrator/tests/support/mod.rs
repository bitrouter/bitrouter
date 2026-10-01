use std::collections::BTreeMap;

use bitrouter_orchestrator::core::checkpoint::{
    CheckpointAck, CheckpointBatch, DurableHead, sha256,
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
    pub limits: Limits,
    pub fail_next_commit: bool,
}

impl DurableHarness {
    pub fn new(grant: OwnershipGrant) -> Self {
        Self {
            grant,
            head: DurableHead::default(),
            batches: Vec::new(),
            acknowledgements: BTreeMap::new(),
            artifacts: BTreeMap::new(),
            limits: Limits::default(),
            fail_next_commit: false,
        }
    }

    pub fn commit(&mut self, batch: &CheckpointBatch) -> Result<CheckpointAck, CoreError> {
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
            self.batches.push(batch.clone());
            self.acknowledgements
                .insert(batch.identity.batch_id.clone(), ack.clone());
            self.head = ack.head();
        }
        Ok(ack)
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
            .insert(reference.artifact_id.clone(), reference);
        Ok(())
    }
}
