//! Version 1 of the BitRouter harness protocol, shared by native and remote
//! clients. This is a BitRouter contract, not an OpenAI collaboration schema.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::checkpoint::{CheckpointAck, CheckpointBatch, DurableHead, sha256};

pub const VERSION: u32 = 1;
pub const BETA: &str = "orchestrator_core=v1";

pub const COLLABORATION_TOOLS: &[&str] = &[
    "spawn_agent",
    "delegate_task",
    "send_message",
    "followup_task",
    "wait_agent",
    "interrupt_agent",
    "list_agents",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    pub version: u32,
    pub core_instance_id: String,
    pub operations: Vec<String>,
    pub transports: Vec<String>,
    pub unsupported_features: Vec<String>,
    pub limits: Limits,
    pub max_sessions: u32,
    pub max_host_model_attempts: u32,
}

impl Capabilities {
    pub fn negotiate(
        &self,
        version: u32,
        required: &[String],
        limits: &Limits,
    ) -> Result<(), CoreError> {
        if version != self.version || version != VERSION {
            return Err(CoreError::rejected(
                ErrorCode::UnsupportedVersion,
                "unsupported harness protocol version",
            ));
        }
        if let Some(feature) = required
            .iter()
            .find(|feature| !self.operations.contains(feature))
        {
            return Err(CoreError::rejected(
                ErrorCode::UnsupportedCapability,
                format!("unsupported required feature: {feature}"),
            ));
        }
        limits.within(&self.limits)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    UnsupportedCapability,
    UnsupportedVersion,
    UnauthorizedScope,
    StaleEpoch,
    StaleRevision,
    OperationConflict,
    Busy,
    LimitExceeded,
    CheckpointConflict,
    CheckpointUnavailable,
    ArtifactUnavailable,
    NoFeasibleRoute,
    InvalidToolResult,
    RecoveryRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitStatus {
    NotCommitted,
    Committed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code:?}: {message}")]
#[serde(deny_unknown_fields)]
pub struct CoreError {
    pub code: ErrorCode,
    pub message: String,
    pub commit_status: CommitStatus,
}

impl CoreError {
    pub fn rejected(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            commit_status: CommitStatus::NotCommitted,
        }
    }
}

/// Limits are frozen with each run. Host limits are upper bounds; callers can
/// only lower them. Checkpoint/output limits are byte counts, not token counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub active_models: u32,
    pub agents: u32,
    pub child_depth: u32,
    pub model_attempts: u32,
    pub outstanding_tools: u32,
    pub queued_runs: u32,
    pub mailbox_messages: u32,
    pub active_seconds: u64,
    pub checkpoint_bytes: u64,
    pub unacknowledged_bytes: u64,
    pub ephemeral_bytes: u64,
    pub input_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            active_models: 4,
            agents: 32,
            child_depth: 4,
            model_attempts: 128,
            outstanding_tools: 8,
            queued_runs: 32,
            mailbox_messages: 128,
            active_seconds: 600,
            checkpoint_bytes: 8 * 1024 * 1024,
            unacknowledged_bytes: 16 * 1024 * 1024,
            ephemeral_bytes: 4 * 1024 * 1024,
            input_bytes: 64 * 1024,
        }
    }
}

impl Limits {
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.active_models == 0
            || self.agents == 0
            || self.model_attempts == 0
            || self.outstanding_tools == 0
            || self.mailbox_messages == 0
            || self.active_seconds == 0
            || self.checkpoint_bytes < 4096
            || self.unacknowledged_bytes < self.checkpoint_bytes
            || self.input_bytes == 0
            || self.input_bytes > self.checkpoint_bytes / 4
        {
            return Err(CoreError::rejected(
                ErrorCode::LimitExceeded,
                "invalid execution or byte limits",
            ));
        }
        Ok(())
    }

    pub fn within(&self, host: &Self) -> Result<(), CoreError> {
        self.validate()?;
        host.validate()?;
        let requested = [
            self.active_models as u64,
            self.agents as u64,
            self.child_depth as u64,
            self.model_attempts as u64,
            self.outstanding_tools as u64,
            self.queued_runs as u64,
            self.mailbox_messages as u64,
            self.active_seconds,
            self.checkpoint_bytes,
            self.unacknowledged_bytes,
            self.ephemeral_bytes,
            self.input_bytes,
        ];
        let allowed = [
            host.active_models as u64,
            host.agents as u64,
            host.child_depth as u64,
            host.model_attempts as u64,
            host.outstanding_tools as u64,
            host.queued_runs as u64,
            host.mailbox_messages as u64,
            host.active_seconds,
            host.checkpoint_bytes,
            host.unacknowledged_bytes,
            host.ephemeral_bytes,
            host.input_bytes,
        ];
        if requested
            .iter()
            .zip(allowed)
            .any(|(request, cap)| *request > cap)
        {
            return Err(CoreError::rejected(
                ErrorCode::LimitExceeded,
                "requested limits exceed host policy",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelMode {
    #[default]
    Fixed,
    Policy,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextMode {
    #[default]
    Fixed,
    Auto,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingSettings {
    #[serde(default)]
    pub model: ModelMode,
    #[serde(default)]
    pub context: ContextMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnershipGrant {
    pub session_id: String,
    pub harness_id: String,
    pub core_instance_id: String,
    pub execution_epoch: u64,
}

impl OwnershipGrant {
    pub fn validate(&self) -> Result<(), CoreError> {
        validate_id(&self.session_id)?;
        validate_id(&self.harness_id)?;
        validate_id(&self.core_instance_id)?;
        if self.execution_epoch == 0 {
            return Err(CoreError::rejected(
                ErrorCode::StaleEpoch,
                "ownership epochs start at one",
            ));
        }
        Ok(())
    }
}

pub(crate) fn validate_id(value: &str) -> Result<(), CoreError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-.:".contains(&byte))
    {
        return Err(CoreError::rejected(
            ErrorCode::UnauthorizedScope,
            "invalid or oversized identity",
        ));
    }
    Ok(())
}

/// Unknown effects require exclusive harness enforcement. A declaration cannot
/// grant authority; the host intersects this manifest with its own policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffect {
    Read,
    Write,
    Shell,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub effect: ToolEffect,
    pub approval_required: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessManifest {
    pub tools: Vec<HarnessTool>,
    pub tool_manifest_digest: String,
    pub workspace_id: String,
    pub workspace_revision: Option<String>,
    pub permission_revision: u64,
    pub max_tool_output_bytes: u64,
    pub artifact_quota_bytes: u64,
    pub max_artifact_chunk_bytes: u64,
    pub required_features: Vec<String>,
}

impl HarnessManifest {
    pub fn digest(tools: &[HarnessTool]) -> Result<String, CoreError> {
        serde_json::to_vec(tools)
            .map(|bytes| sha256(&bytes))
            .map_err(|error| {
                CoreError::rejected(ErrorCode::UnsupportedCapability, error.to_string())
            })
    }

    pub fn validate(&self, capabilities: &Capabilities, limits: &Limits) -> Result<(), CoreError> {
        capabilities.negotiate(VERSION, &self.required_features, limits)?;
        validate_id(&self.workspace_id)?;
        if self.max_tool_output_bytes == 0
            || self.max_tool_output_bytes > limits.input_bytes / 2
            || self.max_artifact_chunk_bytes == 0
            || self.max_artifact_chunk_bytes > limits.input_bytes / 2
            || self.artifact_quota_bytes < self.max_artifact_chunk_bytes
        {
            return Err(CoreError::rejected(
                ErrorCode::LimitExceeded,
                "tool output/chunk bounds or artifact quota are invalid",
            ));
        }
        if Self::digest(&self.tools)? != self.tool_manifest_digest {
            return Err(CoreError::rejected(
                ErrorCode::CheckpointConflict,
                "tool manifest digest mismatch",
            ));
        }
        let mut names = BTreeSet::new();
        for tool in &self.tools {
            validate_id(&tool.name)?;
            if !names.insert(&tool.name) || COLLABORATION_TOOLS.contains(&tool.name.as_str()) {
                return Err(CoreError::rejected(
                    ErrorCode::UnsupportedCapability,
                    "duplicate tool or reserved core collaboration name",
                ));
            }
            if !tool.parameters.is_object() {
                return Err(CoreError::rejected(
                    ErrorCode::UnsupportedCapability,
                    "tool parameters must be a JSON schema object",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    pub artifact_id: String,
    pub sha256: String,
    pub bytes: u64,
    pub media_type: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterialRef {
    pub material_id: String,
    pub version: String,
    pub sha256: String,
    pub media_type: String,
    pub provenance: String,
    pub required: bool,
    pub artifact: Option<ArtifactRef>,
    pub content: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalUpdate {
    pub signal_revision: u64,
    pub observed_at: String,
    pub scope: String,
    pub source: String,
    pub workspace_revision: Option<String>,
    pub manifest: HarnessManifest,
    pub materials: Vec<MaterialRef>,
    pub facts: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskInput {
    pub text: String,
    pub model: String,
    pub effort: Option<String>,
    #[serde(default)]
    pub routing: RoutingSettings,
    pub acceptance_criteria: Vec<String>,
    pub required_materials: Vec<String>,
    pub verification: Option<Verification>,
    pub limits: Option<Limits>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verification {
    pub tool: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    NotStarted,
    WaitingApproval,
    Running,
    Stopped,
    EffectUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    Succeeded,
    Failed,
    Denied,
    NotExecuted,
    EffectUnknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    pub invocation_id: String,
    pub attempt_id: String,
    pub status: ToolOutcome,
    pub output: String,
    pub evidence: Vec<ArtifactRef>,
    pub workspace_revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolObservation {
    pub invocation_id: String,
    pub attempt_id: String,
    pub status: ToolStatus,
    pub evidence: Vec<ArtifactRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bind {
    pub grant: OwnershipGrant,
    pub durable_head: DurableHead,
    pub checkpoint: Option<CheckpointBatch>,
    pub manifest: HarnessManifest,
    pub limits: Limits,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Restore {
    pub binding: Bind,
    pub journal_tail: Vec<CheckpointBatch>,
    pub tools: Vec<ToolObservation>,
    pub results: Vec<ToolResult>,
    pub available_artifacts: Vec<ArtifactRef>,
    /// Takeover is never inferred from an epoch alone. The authenticated
    /// harness must have revoked the previous scheduler and reconciled its I/O.
    pub previous_owner_stopped: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientMessage {
    pub version: u32,
    pub session_id: String,
    pub execution_epoch: u64,
    pub operation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_state_revision: Option<u64>,
    #[serde(flatten)]
    pub command: Command,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", deny_unknown_fields)]
pub enum Command {
    #[serde(rename = "session.bind")]
    Bind(Box<Bind>),
    #[serde(rename = "session.restore")]
    Restore(Box<Restore>),
    #[serde(rename = "input.enqueue")]
    Enqueue(TaskInput),
    #[serde(rename = "input.steer")]
    Steer {
        run_id: String,
        agent_turn_id: String,
        text: String,
    },
    #[serde(rename = "run.cancel")]
    CancelRun { run_id: String },
    #[serde(rename = "agent.cancel")]
    CancelAgent { agent_id: String },
    #[serde(rename = "queue.resume")]
    ResumeQueue,
    #[serde(rename = "signals.update")]
    Signals(Box<SignalUpdate>),
    #[serde(rename = "tool.result")]
    ToolResult(ToolResult),
    #[serde(rename = "tool.status")]
    ToolStatus(ToolObservation),
    #[serde(rename = "material.result")]
    Material {
        request_id: String,
        material: Option<MaterialRef>,
        unavailable_reason: Option<String>,
    },
    #[serde(rename = "checkpoint.ack")]
    Ack(CheckpointAck),
    #[serde(rename = "session.head")]
    Head { durable_head: DurableHead },
    #[serde(rename = "operation.get")]
    Operation { target_operation_id: String },
    #[serde(rename = "session.release")]
    Release,
}

impl ClientMessage {
    pub fn validate(&self, grant: &OwnershipGrant, limits: &Limits) -> Result<(), CoreError> {
        if self.version != VERSION {
            return Err(CoreError::rejected(
                ErrorCode::UnsupportedVersion,
                "supported harness version is 1",
            ));
        }
        validate_id(&self.operation_id)?;
        if self.session_id != grant.session_id {
            return Err(CoreError::rejected(
                ErrorCode::UnauthorizedScope,
                "connection is bound to a different session",
            ));
        }
        if self.execution_epoch != grant.execution_epoch {
            return Err(CoreError::rejected(
                ErrorCode::StaleEpoch,
                "message does not belong to the active grant",
            ));
        }
        let encoded = serde_json::to_vec(self).map_err(|error| {
            CoreError::rejected(ErrorCode::UnsupportedCapability, error.to_string())
        })?;
        let bound = match self.command {
            Command::Bind(_) | Command::Restore(_) => limits.unacknowledged_bytes,
            _ => limits.input_bytes,
        };
        if encoded.len() as u64 > bound {
            return Err(CoreError::rejected(
                ErrorCode::LimitExceeded,
                "control input exceeds negotiated byte bound",
            ));
        }
        if matches!(
            self.command,
            Command::Enqueue(_)
                | Command::Steer { .. }
                | Command::CancelRun { .. }
                | Command::CancelAgent { .. }
                | Command::ResumeQueue
                | Command::Release
        ) && self.expected_state_revision.is_none()
        {
            return Err(CoreError::rejected(
                ErrorCode::StaleRevision,
                "intent mutations require expected_state_revision",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationDisposition {
    Accepted,
    Applied,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationReceipt {
    pub operation_id: String,
    pub request_sha256: String,
    pub disposition: OperationDisposition,
    pub assigned_ids: BTreeMap<String, String>,
    pub state_revision: u64,
    pub error: Option<CoreError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolExecute {
    pub invocation_id: String,
    pub attempt_id: String,
    pub run_id: String,
    pub agent_id: String,
    pub agent_turn_id: String,
    pub step_id: String,
    pub context_revision: u64,
    pub tool: String,
    pub arguments: Value,
    pub tool_manifest_digest: String,
    pub permission_revision: u64,
    pub workspace_id: String,
    pub execution_epoch: u64,
    pub authorizing_event_seq: u64,
    pub verification: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", deny_unknown_fields)]
pub enum ServerMessage {
    #[serde(rename = "checkpoint.proposed")]
    Checkpoint(CheckpointBatch),
    #[serde(rename = "tool.execute")]
    ToolExecute(ToolExecute),
    #[serde(rename = "tool.cancel")]
    ToolCancel {
        invocation_id: String,
        attempt_id: String,
        execution_epoch: u64,
    },
    #[serde(rename = "material.request")]
    MaterialRequest {
        request_id: String,
        material_id: String,
        version: String,
    },
    #[serde(rename = "artifact.put")]
    ArtifactPut {
        reference: ArtifactRef,
        offset: u64,
        content_base64: String,
    },
    #[serde(rename = "operation.receipt")]
    Receipt(OperationReceipt),
    #[serde(rename = "operation.unknown")]
    UnknownOperation { operation_id: String },
    #[serde(rename = "session.head")]
    Head(DurableHead),
    #[serde(rename = "error")]
    Error(CoreError),
}
