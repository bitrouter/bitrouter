//! Shared checkpoint codecs and durable append validation.
//! These checks do not persist a batch or authorize dispatch. The harness owns
//! atomic append; core owns the acknowledgement barrier.

pub mod snapshot;
pub mod validation;

use bitrouter_ai::core::checkpoint::{
    BatchIdentity, CheckpointAck, CheckpointBatch, CheckpointPayload, DurableHead, ToolStartFence,
    validate_digest,
};
use std::collections::{BTreeMap, BTreeSet};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Serialize;
use sha2::{Digest, Sha256};

use bitrouter_ai::core::protocol::{
    ArtifactRef, CoreError, ErrorCode, Limits, OwnershipGrant, VERSION, validate_id,
};

pub fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
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

/// A verified decode tied to immutable bytes and the limits used to admit them.
/// Internal replay consumers can share it without accepting an arbitrary payload.
pub(crate) struct VerifiedBatch<'a> {
    batch: &'a CheckpointBatch,
    limits: &'a Limits,
    payload: CheckpointPayload,
}

impl VerifiedBatch<'_> {
    pub(crate) fn batch(&self) -> &CheckpointBatch {
        self.batch
    }

    pub(crate) fn payload(&self) -> &CheckpointPayload {
        &self.payload
    }

    pub(crate) fn validate_append(
        &self,
        grant: &OwnershipGrant,
        head: &DurableHead,
        limits: &Limits,
        available: &BTreeMap<String, ArtifactRef>,
        retained_ack: Option<&CheckpointAck>,
    ) -> Result<CheckpointAck, CoreError> {
        if self.limits != limits {
            return Err(conflict("verified checkpoint limits changed"));
        }
        grant.validate()?;
        head.validate()?;
        validate_decoded_append(
            self.batch,
            &self.payload,
            grant,
            head,
            available,
            retained_ack,
        )
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

fn conflict(message: impl Into<String>) -> CoreError {
    CoreError::rejected(ErrorCode::CheckpointConflict, message)
}

pub(crate) fn serialized_bytes(value: &impl Serialize) -> Result<u64, CoreError> {
    let mut counter = ByteCounter(0);
    serde_json::to_writer(&mut counter, value).map_err(|error| conflict(error.to_string()))?;
    Ok(counter.0)
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

/// Application checkpoint codecs and durable append checks over public envelopes.
/// A returned ACK is only a proposal: persistence and ownership checks must be
/// serialized by the harness before it sends that ACK to the core.
pub trait CheckpointBatchExt: Sized {
    fn encode(payload: &CheckpointPayload, limits: &Limits) -> Result<Self, CoreError>;
    fn decode(&self, limits: &Limits) -> Result<CheckpointPayload, CoreError>;
    fn wire_bytes(&self) -> Result<u64, CoreError>;
    fn validate_append(
        &self,
        grant: &OwnershipGrant,
        head: &DurableHead,
        limits: &Limits,
        available: &BTreeMap<String, ArtifactRef>,
        retained_ack: Option<&CheckpointAck>,
    ) -> Result<CheckpointAck, CoreError>;
    fn validate_append_with_payload(
        &self,
        grant: &OwnershipGrant,
        head: &DurableHead,
        limits: &Limits,
        available: &BTreeMap<String, ArtifactRef>,
        retained_ack: Option<&CheckpointAck>,
    ) -> Result<(CheckpointAck, CheckpointPayload), CoreError>;
}

impl CheckpointBatchExt for CheckpointBatch {
    /// Serialize exactly once. Retain this object for retransmission; neither
    /// endpoint computes a digest from a reserialized/reordered JSON object.
    fn encode(payload: &CheckpointPayload, limits: &Limits) -> Result<Self, CoreError> {
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
        check_base64_wire_limit(&batch, limits)?;
        Ok(batch)
    }

    fn decode(&self, limits: &Limits) -> Result<CheckpointPayload, CoreError> {
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
        check_base64_wire_limit(self, limits)?;
        let bytes = match STANDARD.decode(&self.payload_bytes) {
            Ok(bytes) => bytes,
            Err(error) => {
                // Invalid base64 may contain JSON escapes. Preserve the exact
                // wire-limit error before reporting malformed payload bytes.
                check_wire_limit(self, limits)?;
                return Err(conflict(error.to_string()));
            }
        };
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
    fn wire_bytes(&self) -> Result<u64, CoreError> {
        #[derive(Serialize)]
        struct Envelope<'a> {
            #[serde(rename = "type")]
            kind: &'static str,
            payload: &'a CheckpointBatch,
        }
        serialized_bytes(&Envelope {
            kind: "checkpoint.proposed",
            payload: self,
        })
    }

    /// Harness-side admission checks. The caller must perform these checks and
    /// the append atomically under its durable ownership grant. Artifact
    /// availability means verified durable bytes, not a staging upload.
    fn validate_append(
        &self,
        grant: &OwnershipGrant,
        head: &DurableHead,
        limits: &Limits,
        available: &BTreeMap<String, ArtifactRef>,
        retained_ack: Option<&CheckpointAck>,
    ) -> Result<CheckpointAck, CoreError> {
        self.validate_append_with_payload(grant, head, limits, available, retained_ack)
            .map(|(ack, _)| ack)
    }

    /// Apply the same admission checks as [`Self::validate_append`] and return
    /// the verified payload for artifact checks, tool fences and event storage.
    /// Keep validation and persistence in the same durable transaction; the
    /// returned ACK does not establish that the append has been persisted.
    fn validate_append_with_payload(
        &self,
        grant: &OwnershipGrant,
        head: &DurableHead,
        limits: &Limits,
        available: &BTreeMap<String, ArtifactRef>,
        retained_ack: Option<&CheckpointAck>,
    ) -> Result<(CheckpointAck, CheckpointPayload), CoreError> {
        grant.validate()?;
        head.validate()?;
        let payload = self.decode(limits)?;
        let ack = validate_decoded_append(self, &payload, grant, head, available, retained_ack)?;
        Ok((ack, payload))
    }
}

pub(crate) fn verified_batch<'a>(
    batch: &'a CheckpointBatch,
    limits: &'a Limits,
) -> Result<VerifiedBatch<'a>, CoreError> {
    Ok(VerifiedBatch {
        batch,
        limits,
        payload: batch.decode(limits)?,
    })
}

/// Check a projected payload length without allocating its base64 bytes.
/// Base64 contains no JSON-escaped characters, so the empty wire envelope
/// plus its encoded length gives the exact transport size.
pub(crate) fn check_projected_size(
    identity: &BatchIdentity,
    payload_bytes: u64,
    limits: &Limits,
) -> Result<(), CoreError> {
    if payload_bytes > limits.checkpoint_bytes {
        return Err(CoreError::rejected(
            ErrorCode::LimitExceeded,
            "checkpoint cleanup capacity exhausted",
        ));
    }
    let encoded = payload_bytes
        .checked_add(2)
        .and_then(|bytes| (bytes / 3).checked_mul(4))
        .ok_or_else(|| {
            CoreError::rejected(ErrorCode::LimitExceeded, "checkpoint size exhausted")
        })?;
    let envelope = CheckpointBatch {
        identity: identity.clone(),
        payload_encoding: "base64-json".into(),
        payload_bytes: String::new(),
        payload_sha256: "0".repeat(64),
    };
    if envelope
        .wire_bytes()?
        .checked_add(encoded)
        .is_none_or(|bytes| bytes > limits.unacknowledged_bytes)
    {
        return Err(CoreError::rejected(
            ErrorCode::LimitExceeded,
            "checkpoint cleanup wire capacity exhausted",
        ));
    }
    Ok(())
}

fn check_wire_limit(batch: &CheckpointBatch, limits: &Limits) -> Result<(), CoreError> {
    if batch.wire_bytes()? > limits.unacknowledged_bytes {
        return Err(CoreError::rejected(
            ErrorCode::LimitExceeded,
            "checkpoint wire envelope exceeds unacknowledged output bound",
        ));
    }
    Ok(())
}

/// Valid base64 needs no JSON escaping. Count only the small envelope and
/// add the encoded length instead of rescanning a potentially multi-MiB
/// string at every admission. For untrusted input this is a lower bound;
/// decode must still strictly validate base64 before accepting the batch.
fn check_base64_wire_limit(batch: &CheckpointBatch, limits: &Limits) -> Result<(), CoreError> {
    #[derive(Serialize)]
    struct EmptyBatch<'a> {
        identity: &'a BatchIdentity,
        payload_encoding: &'a str,
        payload_bytes: &'static str,
        payload_sha256: &'a str,
    }
    #[derive(Serialize)]
    struct Envelope<'a> {
        #[serde(rename = "type")]
        kind: &'static str,
        payload: EmptyBatch<'a>,
    }
    let overhead = serialized_bytes(&Envelope {
        kind: "checkpoint.proposed",
        payload: EmptyBatch {
            identity: &batch.identity,
            payload_encoding: &batch.payload_encoding,
            payload_bytes: "",
            payload_sha256: &batch.payload_sha256,
        },
    })?;
    if overhead
        .checked_add(batch.payload_bytes.len() as u64)
        .is_none_or(|bytes| bytes > limits.unacknowledged_bytes)
    {
        return Err(CoreError::rejected(
            ErrorCode::LimitExceeded,
            "checkpoint wire envelope exceeds unacknowledged output bound",
        ));
    }
    Ok(())
}

// Only call with a validated grant/head and a payload decoded from this
// batch. External callers must validate the hashed bytes.
fn validate_decoded_append(
    batch: &CheckpointBatch,
    payload: &CheckpointPayload,
    grant: &OwnershipGrant,
    head: &DurableHead,
    available: &BTreeMap<String, ArtifactRef>,
    retained_ack: Option<&CheckpointAck>,
) -> Result<CheckpointAck, CoreError> {
    if batch.identity.session_id != grant.session_id
        || batch.identity.core_instance_id != grant.core_instance_id
    {
        return Err(CoreError::rejected(
            ErrorCode::UnauthorizedScope,
            "batch belongs to another session or core instance",
        ));
    }
    if batch.identity.execution_epoch != grant.execution_epoch
        || head.execution_epoch > grant.execution_epoch
    {
        return Err(CoreError::rejected(
            ErrorCode::StaleEpoch,
            "batch is fenced by current ownership grant",
        ));
    }
    if let Some(previous) = retained_ack {
        let expected = ack_for_batch(batch, payload);
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
    if head.batch_id.as_deref() == Some(&batch.identity.batch_id) {
        let ack = ack_for_batch(batch, payload);
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
    Ok(ack_for_batch(batch, payload))
}

pub(crate) fn ack_for_batch(batch: &CheckpointBatch, payload: &CheckpointPayload) -> CheckpointAck {
    CheckpointAck {
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
