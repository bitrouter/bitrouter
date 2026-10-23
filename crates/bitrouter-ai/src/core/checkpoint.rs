//! Exact-byte checkpoint envelopes shared by the service and its harness client.
//! Encoding, hash verification, durable append admission and commit barriers are
//! application responsibilities. Constructing an ACK does not prove persistence.

use super::protocol::{ArtifactRef, CoreError, ErrorCode, validate_id};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableHead {
    pub execution_epoch: u64,
    pub state_revision: u64,
    pub event_seq: u64,
    pub batch_id: Option<String>,
    pub payload_sha256: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointBatch {
    pub identity: BatchIdentity,
    pub payload_encoding: String,
    pub payload_bytes: String,
    pub payload_sha256: String,
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

impl CheckpointAck {
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

fn conflict(message: impl Into<String>) -> CoreError {
    CoreError::rejected(ErrorCode::CheckpointConflict, message)
}
