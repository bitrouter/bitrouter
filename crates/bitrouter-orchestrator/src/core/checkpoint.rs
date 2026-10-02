//! Exact-byte checkpoint batches and the core's acknowledgement barrier.
//! Persistence is supplied by the harness. A proposal is never an execution
//! authorization; only a matching durable acknowledgement advances the head.

use std::collections::{BTreeMap, BTreeSet};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::protocol::{
    ArtifactRef, CoreError, ErrorCode, Limits, OwnershipGrant, VERSION, validate_id,
};

pub fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn validate_digest(value: &str) -> Result<(), CoreError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(CoreError::rejected(
            ErrorCode::CheckpointConflict,
            "expected lowercase SHA-256 digest",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableHead {
    pub execution_epoch: u64,
    pub state_revision: u64,
    pub event_seq: u64,
    pub batch_id: Option<String>,
    pub payload_sha256: Option<String>,
}

impl DurableHead {
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.state_revision == 0 {
            if self != &Self::default() {
                return Err(conflict(
                    "an empty durable head must have all-zero versions and no batch",
                ));
            }
        } else {
            if self.execution_epoch == 0 || self.event_seq < self.state_revision {
                return Err(conflict("durable head has inconsistent versions"));
            }
            validate_id(
                self.batch_id
                    .as_deref()
                    .ok_or_else(|| conflict("durable head has no batch"))?,
            )?;
            validate_digest(
                self.payload_sha256
                    .as_deref()
                    .ok_or_else(|| conflict("durable head has no digest"))?,
            )?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchIdentity {
    pub batch_id: String,
    pub session_id: String,
    pub execution_epoch: u64,
    pub core_instance_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableEvent {
    pub event_seq: u64,
    #[serde(rename = "type")]
    pub kind: String,
    pub run_id: Option<String>,
    pub agent_id: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub schema_version: u32,
    pub state_revision: u64,
    pub artifact_refs: Vec<ArtifactRef>,
    /// Versioned core-owned snapshot. Harnesses store it without editing it;
    /// the scheduler decodes and validates its concrete state during restore.
    pub state: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointPayload {
    pub identity: BatchIdentity,
    pub base_event_seq: u64,
    pub base_state_revision: u64,
    pub events: Vec<DurableEvent>,
    /// Persist with the append, serialized against the harness's actual tool
    /// start. Block late execute/approval for these exact identities, including
    /// identities absent from the local ledger. Already started work continues;
    /// this fence is not a tool outcome. An ACK covers both records atomically.
    #[serde(default)]
    pub tool_start_fences: Vec<ToolStartFence>,
    pub checkpoint: Checkpoint,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolStartFence {
    pub invocation_id: String,
    pub attempt_id: String,
}

pub(crate) fn validate_tool_start_fences(fences: &[ToolStartFence]) -> Result<(), CoreError> {
    let mut ids = BTreeSet::new();
    for fence in fences {
        validate_id(&fence.invocation_id)?;
        validate_id(&fence.attempt_id)?;
        if !ids.insert(&fence.invocation_id) {
            return Err(conflict("duplicate tool start fence"));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointBatch {
    pub identity: BatchIdentity,
    pub payload_encoding: String,
    pub payload_bytes: String,
    pub payload_sha256: String,
}

impl CheckpointBatch {
    /// Serialize exactly once. Retain this object for retransmission; neither
    /// endpoint computes a digest from a reserialized/reordered JSON object.
    pub fn encode(payload: &CheckpointPayload, limits: &Limits) -> Result<Self, CoreError> {
        validate_payload(payload)?;
        let bytes = serde_json::to_vec(payload).map_err(|error| conflict(error.to_string()))?;
        if bytes.len() as u64 > limits.checkpoint_bytes {
            return Err(CoreError::rejected(
                ErrorCode::LimitExceeded,
                "serialized checkpoint exceeds negotiated bound",
            ));
        }
        let batch = Self {
            identity: payload.identity.clone(),
            payload_encoding: "base64-json".into(),
            payload_bytes: STANDARD.encode(&bytes),
            payload_sha256: sha256(&bytes),
        };
        batch.check_wire_limit(limits)?;
        Ok(batch)
    }

    pub fn decode(&self, limits: &Limits) -> Result<CheckpointPayload, CoreError> {
        if self.payload_encoding != "base64-json" {
            return Err(CoreError::rejected(
                ErrorCode::UnsupportedVersion,
                "unsupported checkpoint encoding",
            ));
        }
        let max_encoded = limits
            .checkpoint_bytes
            .saturating_add(2)
            .saturating_div(3)
            .saturating_mul(4);
        if self.payload_bytes.len() as u64 > max_encoded {
            return Err(CoreError::rejected(
                ErrorCode::LimitExceeded,
                "encoded checkpoint exceeds negotiated bound",
            ));
        }
        self.check_wire_limit(limits)?;
        let bytes = STANDARD
            .decode(&self.payload_bytes)
            .map_err(|error| conflict(error.to_string()))?;
        if bytes.len() as u64 > limits.checkpoint_bytes {
            return Err(CoreError::rejected(
                ErrorCode::LimitExceeded,
                "decoded checkpoint exceeds negotiated bound",
            ));
        }
        if sha256(&bytes) != self.payload_sha256 {
            return Err(conflict("checkpoint digest mismatch"));
        }
        let payload: CheckpointPayload =
            serde_json::from_slice(&bytes).map_err(|error| conflict(error.to_string()))?;
        if payload.identity != self.identity {
            return Err(conflict(
                "checkpoint envelope does not match hashed identity",
            ));
        }
        validate_payload(&payload)?;
        Ok(payload)
    }

    /// Count the complete `checkpoint.proposed` wire message, including base64
    /// expansion and the transport envelope, without allocating another copy.
    pub fn wire_bytes(&self) -> Result<u64, CoreError> {
        #[derive(Serialize)]
        struct Envelope<'a> {
            #[serde(rename = "type")]
            kind: &'static str,
            payload: &'a CheckpointBatch,
        }
        let mut counter = ByteCounter(0);
        serde_json::to_writer(
            &mut counter,
            &Envelope {
                kind: "checkpoint.proposed",
                payload: self,
            },
        )
        .map_err(|error| conflict(error.to_string()))?;
        Ok(counter.0)
    }

    fn check_wire_limit(&self, limits: &Limits) -> Result<(), CoreError> {
        if self.wire_bytes()? > limits.unacknowledged_bytes {
            return Err(CoreError::rejected(
                ErrorCode::LimitExceeded,
                "checkpoint wire envelope exceeds unacknowledged output bound",
            ));
        }
        Ok(())
    }

    /// Harness-side admission checks. The caller must perform these checks and
    /// the append atomically under its durable ownership grant. Artifact
    /// availability means verified durable bytes, not a staging upload.
    pub fn validate_append(
        &self,
        grant: &OwnershipGrant,
        head: &DurableHead,
        limits: &Limits,
        available: &BTreeMap<String, ArtifactRef>,
        retained_ack: Option<&CheckpointAck>,
    ) -> Result<CheckpointAck, CoreError> {
        grant.validate()?;
        head.validate()?;
        let payload = self.decode(limits)?;
        if self.identity.session_id != grant.session_id
            || self.identity.core_instance_id != grant.core_instance_id
        {
            return Err(CoreError::rejected(
                ErrorCode::UnauthorizedScope,
                "batch belongs to another session or core instance",
            ));
        }
        if self.identity.execution_epoch != grant.execution_epoch
            || head.execution_epoch > grant.execution_epoch
        {
            return Err(CoreError::rejected(
                ErrorCode::StaleEpoch,
                "batch is fenced by current ownership grant",
            ));
        }
        if let Some(previous) = retained_ack {
            let expected = CheckpointAck::for_batch(self, &payload);
            if previous != &expected
                || previous.state_revision > head.state_revision
                || previous.through_event_seq > head.event_seq
            {
                return Err(conflict(
                    "retained batch identity has different content or is ahead of durable head",
                ));
            }
            return Ok(previous.clone());
        }
        if head.batch_id.as_deref() == Some(&self.identity.batch_id) {
            let ack = CheckpointAck::for_batch(self, &payload);
            if ack.head() == *head {
                return Ok(ack);
            }
            return Err(conflict("same batch identity has different content"));
        }
        if payload.base_state_revision != head.state_revision
            || payload.base_event_seq != head.event_seq
        {
            return Err(conflict("checkpoint append does not match durable base"));
        }
        for reference in &payload.checkpoint.artifact_refs {
            if available.get(&reference.artifact_id) != Some(reference) {
                return Err(CoreError::rejected(
                    ErrorCode::ArtifactUnavailable,
                    format!("artifact {} is not durable", reference.artifact_id),
                ));
            }
        }
        Ok(CheckpointAck::for_batch(self, &payload))
    }
}

fn validate_payload(payload: &CheckpointPayload) -> Result<(), CoreError> {
    validate_tool_start_fences(&payload.tool_start_fences)?;
    validate_id(&payload.identity.batch_id)?;
    validate_id(&payload.identity.session_id)?;
    validate_id(&payload.identity.core_instance_id)?;
    if payload.identity.execution_epoch == 0 {
        return Err(CoreError::rejected(
            ErrorCode::StaleEpoch,
            "checkpoint epoch starts at one",
        ));
    }
    if payload.checkpoint.schema_version != VERSION {
        return Err(CoreError::rejected(
            ErrorCode::UnsupportedVersion,
            "unsupported checkpoint schema",
        ));
    }
    if payload.base_state_revision.checked_add(1) != Some(payload.checkpoint.state_revision)
        || payload.events.is_empty()
    {
        return Err(conflict(
            "checkpoint must advance one revision and contain durable events",
        ));
    }
    let mut sequence = payload.base_event_seq;
    for event in &payload.events {
        sequence = sequence
            .checked_add(1)
            .ok_or_else(|| conflict("event sequence exhausted"))?;
        if event.event_seq != sequence || event.kind.is_empty() {
            return Err(conflict(
                "event sequence is not contiguous or event type is empty",
            ));
        }
    }
    let mut ids = BTreeSet::new();
    for reference in &payload.checkpoint.artifact_refs {
        validate_id(&reference.artifact_id)?;
        validate_digest(&reference.sha256)?;
        if !ids.insert(&reference.artifact_id) {
            return Err(conflict("duplicate artifact identity"));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointAck {
    pub batch_id: String,
    pub payload_sha256: String,
    pub through_event_seq: u64,
    pub state_revision: u64,
    pub execution_epoch: u64,
}

impl CheckpointAck {
    pub(crate) fn for_batch(batch: &CheckpointBatch, payload: &CheckpointPayload) -> Self {
        Self {
            batch_id: batch.identity.batch_id.clone(),
            payload_sha256: batch.payload_sha256.clone(),
            through_event_seq: payload
                .events
                .last()
                .map_or(payload.base_event_seq, |event| event.event_seq),
            state_revision: payload.checkpoint.state_revision,
            execution_epoch: batch.identity.execution_epoch,
        }
    }

    pub fn head(&self) -> DurableHead {
        DurableHead {
            execution_epoch: self.execution_epoch,
            state_revision: self.state_revision,
            event_seq: self.through_event_seq,
            batch_id: Some(self.batch_id.clone()),
            payload_sha256: Some(self.payload_sha256.clone()),
        }
    }
}

/// Pure core-side barrier. Provider/tool I/O must run outside scheduler locks
/// and consult this barrier before every new dispatch. It is not a store.
pub struct CommitGate {
    grant: OwnershipGrant,
    head: DurableHead,
    limits: Limits,
    pending: Option<CheckpointBatch>,
    connected: bool,
    provisionally_blocked: bool,
    released: bool,
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
        self.pending.as_ref()
    }

    pub fn can_dispatch(&self) -> bool {
        self.connected
            && !self.provisionally_blocked
            && !self.released
            && self.pending.is_none()
            && self.head.state_revision > 0
            && self.head.execution_epoch == self.grant.execution_epoch
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
        batch.validate_append(&self.grant, &self.head, &self.limits, &artifacts, None)?;
        self.pending = Some(batch);
        self.pending
            .as_ref()
            .ok_or_else(|| conflict("checkpoint proposal was not retained"))
    }

    /// Returns the newly committed snapshot only once. Exact duplicate ACKs
    /// return None, so an adapter cannot consume events or dispatch twice.
    pub fn acknowledge(
        &mut self,
        ack: &CheckpointAck,
    ) -> Result<Option<CheckpointPayload>, CoreError> {
        if ack.execution_epoch != self.grant.execution_epoch {
            return Err(CoreError::rejected(
                ErrorCode::StaleEpoch,
                "acknowledgement belongs to a different epoch",
            ));
        }
        if ack.head() == self.head {
            return Ok(None);
        }
        let batch = self
            .pending
            .as_ref()
            .ok_or_else(|| conflict("no pending batch matches acknowledgement"))?;
        let payload = batch.decode(&self.limits)?;
        if *ack != CheckpointAck::for_batch(batch, &payload) {
            return Err(conflict(
                "acknowledgement does not match the complete pending batch",
            ));
        }
        self.head = ack.head();
        self.pending = None;
        Ok(Some(payload))
    }

    /// Reconcile an ACK lost after atomic harness commit. A different head is
    /// never guessed: full authenticated restore is required in that case.
    pub fn reconnect(
        &mut self,
        durable: &DurableHead,
    ) -> Result<Option<CheckpointPayload>, CoreError> {
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
        let committed = self.acknowledge(&ack)?;
        self.connected = true;
        Ok(committed)
    }
}

fn conflict(message: impl Into<String>) -> CoreError {
    CoreError::rejected(ErrorCode::CheckpointConflict, message)
}

struct ByteCounter(u64);

impl std::io::Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| std::io::Error::other("serialized size exhausted"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
