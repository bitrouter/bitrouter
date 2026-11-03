//! Core acknowledgement barrier. Checkpoint codecs and append validation live
//! in application storage; only this scheduler gate authorizes new dispatch.

use crate::storage::checkpoint::CheckpointBatchExt;
use bitrouter_ai::core::checkpoint::{
    CheckpointAck, CheckpointBatch, CheckpointPayload, DurableHead,
};
use bitrouter_ai::core::protocol::{CoreError, ErrorCode, Limits, OwnershipGrant};

/// Pure core-side barrier. Provider/tool I/O must run outside scheduler locks
/// and consult this barrier before every new dispatch. It is not a store.
pub struct CommitGate {
    grant: OwnershipGrant,
    head: DurableHead,
    limits: Limits,
    pending: Option<PendingBatch>,
    connected: bool,
    provisionally_blocked: bool,
    released: bool,
}

struct PendingBatch {
    batch: CheckpointBatch,
    // Established by full append validation before retention. The encoded
    // batch stays immutable, so ACK reconciliation need not decode it again
    // when the session already retains the corresponding typed candidate.
    state_revision: u64,
    through_event_seq: u64,
}

impl CommitGate {
    /// The embedding host must authenticate the harness/owner and validate the
    /// restore snapshot before creating a gate from a nonempty durable head.
    pub fn new(
        grant: OwnershipGrant,
        head: DurableHead,
        limits: Limits,
    ) -> Result<Self, CoreError> {
        grant.validate()?;
        head.validate()?;
        limits.validate()?;
        if head.execution_epoch > grant.execution_epoch {
            return Err(CoreError::rejected(
                ErrorCode::StaleEpoch,
                "durable head is newer than grant",
            ));
        }
        Ok(Self {
            grant,
            head,
            limits,
            pending: None,
            connected: true,
            provisionally_blocked: false,
            released: false,
        })
    }

    pub fn head(&self) -> &DurableHead {
        &self.head
    }
    pub fn grant(&self) -> &OwnershipGrant {
        &self.grant
    }
    pub fn pending(&self) -> Option<&CheckpointBatch> {
        self.pending.as_ref().map(|pending| &pending.batch)
    }

    pub fn can_dispatch(&self) -> bool {
        self.connected
            && !self.provisionally_blocked
            && !self.released
            && self.pending.is_none()
            && self.head.state_revision > 0
            && self.head.execution_epoch == self.grant.execution_epoch
    }

    #[cfg(test)]
    pub(super) fn dispatch_fence_for_test(&self) -> &'static str {
        if !self.connected {
            "Disconnected"
        } else if self.provisionally_blocked {
            "Provisional"
        } else if self.released {
            "Released"
        } else if self.pending.is_some() {
            "Pending"
        } else if self.head.state_revision == 0 {
            "NoRevision"
        } else if self.head.execution_epoch != self.grant.execution_epoch {
            "Epoch"
        } else {
            "Ready"
        }
    }

    pub fn disconnect(&mut self) {
        self.connected = false;
    }
    pub fn block_dispatch(&mut self) {
        self.provisionally_blocked = true;
    }
    pub fn clear_dispatch_block(&mut self) {
        self.provisionally_blocked = false;
    }
    pub fn release(&mut self) {
        self.released = true;
    }

    pub fn propose(&mut self, payload: CheckpointPayload) -> Result<&CheckpointBatch, CoreError> {
        if !self.connected || self.released {
            return Err(CoreError::rejected(
                ErrorCode::CheckpointUnavailable,
                "durable authority is disconnected or released",
            ));
        }
        if self.pending.is_some() {
            return Err(CoreError::rejected(
                ErrorCode::Busy,
                "one checkpoint batch is already outstanding",
            ));
        }
        if payload.base_state_revision != self.head.state_revision
            || payload.base_event_seq != self.head.event_seq
            || self.head.batch_id.as_deref() == Some(&payload.identity.batch_id)
        {
            return Err(conflict(
                "new proposal must extend the current head with a new batch identity",
            ));
        }
        let batch = CheckpointBatch::encode(&payload, &self.limits)?;
        let artifacts = payload
            .checkpoint
            .artifact_refs
            .iter()
            .map(|reference| (reference.artifact_id.clone(), reference.clone()))
            .collect();
        let ack = batch.validate_append(&self.grant, &self.head, &self.limits, &artifacts, None)?;
        self.pending = Some(PendingBatch {
            batch,
            state_revision: ack.state_revision,
            through_event_seq: ack.through_event_seq,
        });
        self.pending()
            .ok_or_else(|| conflict("checkpoint proposal was not retained"))
    }

    /// Returns the newly committed snapshot only once. Exact duplicate ACKs
    /// return None, so an adapter cannot consume events or dispatch twice.
    pub fn acknowledge(
        &mut self,
        ack: &CheckpointAck,
    ) -> Result<Option<CheckpointPayload>, CoreError> {
        self.acknowledge_with(ack, CheckpointBatch::decode)
    }

    /// Publish only the ACK barrier; the session adopts its retained candidate.
    pub(crate) fn acknowledge_transition(
        &mut self,
        ack: &CheckpointAck,
    ) -> Result<Option<()>, CoreError> {
        self.acknowledge_with(ack, |_, _| Ok(()))
    }

    fn acknowledge_with<T>(
        &mut self,
        ack: &CheckpointAck,
        extract: impl FnOnce(&CheckpointBatch, &Limits) -> Result<T, CoreError>,
    ) -> Result<Option<T>, CoreError> {
        if ack.execution_epoch != self.grant.execution_epoch {
            return Err(CoreError::rejected(
                ErrorCode::StaleEpoch,
                "acknowledgement belongs to a different epoch",
            ));
        }
        if ack.head() == self.head {
            return Ok(None);
        }
        let pending = self
            .pending
            .as_ref()
            .ok_or_else(|| conflict("no pending batch matches acknowledgement"))?;
        let committed = extract(&pending.batch, &self.limits)?;
        if ack.batch_id != pending.batch.identity.batch_id
            || ack.payload_sha256 != pending.batch.payload_sha256
            || ack.execution_epoch != pending.batch.identity.execution_epoch
            || ack.state_revision != pending.state_revision
            || ack.through_event_seq != pending.through_event_seq
        {
            return Err(conflict(
                "acknowledgement does not match the complete pending batch",
            ));
        }
        self.head = ack.head();
        self.pending = None;
        Ok(Some(committed))
    }

    /// Reconcile an ACK lost after atomic harness commit. A different head is
    /// never guessed: full authenticated restore is required in that case.
    pub fn reconnect(
        &mut self,
        durable: &DurableHead,
    ) -> Result<Option<CheckpointPayload>, CoreError> {
        self.reconnect_with(durable, CheckpointBatch::decode)
    }

    /// Reconcile a lost ACK without reconstructing the retained candidate.
    pub(crate) fn reconnect_transition(
        &mut self,
        durable: &DurableHead,
    ) -> Result<Option<()>, CoreError> {
        self.reconnect_with(durable, |_, _| Ok(()))
    }

    fn reconnect_with<T>(
        &mut self,
        durable: &DurableHead,
        extract: impl FnOnce(&CheckpointBatch, &Limits) -> Result<T, CoreError>,
    ) -> Result<Option<T>, CoreError> {
        self.connected = false;
        durable.validate()?;
        if durable == &self.head {
            self.connected = true;
            return Ok(None);
        }
        let ack = CheckpointAck {
            batch_id: durable
                .batch_id
                .clone()
                .ok_or_else(|| conflict("reconnected head has no batch"))?,
            payload_sha256: durable
                .payload_sha256
                .clone()
                .ok_or_else(|| conflict("reconnected head has no digest"))?,
            through_event_seq: durable.event_seq,
            state_revision: durable.state_revision,
            execution_epoch: durable.execution_epoch,
        };
        let committed = self.acknowledge_with(&ack, extract)?;
        self.connected = true;
        Ok(committed)
    }
}

fn conflict(message: impl Into<String>) -> CoreError {
    CoreError::rejected(ErrorCode::CheckpointConflict, message)
}

#[cfg(test)]
mod tests;
