//! Service-side checks for the managed harness protocol. Wire data lives in AI.

use bitrouter_ai::core::protocol::{
    ArtifactRef, COLLABORATION_TOOLS, Capabilities, ClientMessage, Command, CoreError, ErrorCode,
    HarnessManifest, HarnessTool, Limits, OwnershipGrant, ToolObservation, ToolOutcome, ToolResult,
    ToolResultLimits, VERSION, validate_id,
};
use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::checkpoint::{serialized_bytes, sha256};

/// Count immutable objects once even when a payload references one repeatedly.
pub(crate) fn artifact_bytes(references: &[ArtifactRef]) -> Result<u64, CoreError> {
    let mut seen = BTreeMap::new();
    let mut bytes = 0u64;
    for reference in references {
        validate_id(&reference.artifact_id)?;
        bitrouter_ai::core::checkpoint::validate_digest(&reference.sha256)?;
        if let Some(prior) = seen.insert(&reference.artifact_id, reference) {
            if prior != reference {
                return Err(CoreError::rejected(
                    ErrorCode::CheckpointConflict,
                    "artifact identity has conflicting content references",
                ));
            }
        } else {
            bytes = bytes.checked_add(reference.bytes).ok_or_else(|| {
                CoreError::rejected(ErrorCode::LimitExceeded, "artifact byte count overflow")
            })?;
        }
    }
    Ok(bytes)
}

/// Application-owned manifest hashing and host capability admission.
pub trait HarnessManifestExt {
    fn digest(tools: &[HarnessTool]) -> Result<String, CoreError>;
    fn validate(&self, capabilities: &Capabilities, limits: &Limits) -> Result<(), CoreError>;
}

/// Application checks against the connection's authenticated grant and limits.
/// Passing these checks alone does not authorize an operation or commit state.
pub trait ClientMessageExt {
    fn validate(&self, grant: &OwnershipGrant, limits: &Limits) -> Result<(), CoreError>;
}

/// Service/harness enforcement of a tool invocation's frozen result limits.
pub trait ToolResultLimitsExt {
    fn validate_result(&self, result: &ToolResult) -> Result<(), CoreError>;
    fn validate_observation(&self, observation: &ToolObservation) -> Result<(), CoreError>;
}

impl HarnessManifestExt for HarnessManifest {
    fn digest(tools: &[HarnessTool]) -> Result<String, CoreError> {
        serde_json::to_vec(tools)
            .map(|bytes| sha256(&bytes))
            .map_err(|error| {
                CoreError::rejected(ErrorCode::UnsupportedCapability, error.to_string())
            })
    }

    fn validate(&self, capabilities: &Capabilities, limits: &Limits) -> Result<(), CoreError> {
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
            if !names.insert(&tool.name)
                || COLLABORATION_TOOLS.contains(&tool.name.as_str())
                || super::context_router::tools::NAMES.contains(&tool.name.as_str())
            {
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

impl ClientMessageExt for ClientMessage {
    fn validate(&self, grant: &OwnershipGrant, limits: &Limits) -> Result<(), CoreError> {
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
        let bound = match self.command {
            Command::Bind(_) | Command::Restore(_) | Command::ProviderEvidence(_) => {
                limits.unacknowledged_bytes
            }
            _ => limits.input_bytes,
        };
        if serialized_bytes(self)? > bound {
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

impl ToolResultLimitsExt for ToolResultLimits {
    fn validate_result(&self, result: &ToolResult) -> Result<(), CoreError> {
        if result.output.len() as u64 > self.output_bytes {
            return Err(CoreError::rejected(
                ErrorCode::LimitExceeded,
                "tool result exceeds its admitted output bound",
            ));
        }
        validate_tool_payload(self, result)?;
        validate_tool_artifacts(self, &result.evidence)
    }

    fn validate_observation(&self, observation: &ToolObservation) -> Result<(), CoreError> {
        validate_tool_payload(self, observation)?;
        validate_tool_artifacts(self, &observation.evidence)
    }
}

pub(crate) fn tool_result_limits(
    input_bytes: u64,
    output_bytes: u64,
) -> Result<ToolResultLimits, CoreError> {
    let result = ToolResult {
        invocation_id: "x".repeat(128),
        attempt_id: "x".repeat(128),
        status: ToolOutcome::EffectUnknown,
        output: String::new(),
        evidence: Vec::new(),
        workspace_revision: None,
    };
    let result_bytes = serialized_bytes(&result)?;
    let envelope = ClientMessage {
        version: VERSION,
        session_id: "x".repeat(128),
        execution_epoch: u64::MAX,
        operation_id: "x".repeat(128),
        expected_state_revision: Some(u64::MAX),
        command: Command::ToolResult(result),
    };
    // tool.result and tool.status have equal-length tags and identical
    // envelopes. Reserve the largest envelope even for in-process callers.
    let overhead = serialized_bytes(&envelope)?.saturating_sub(result_bytes);
    let payload_bytes = input_bytes.saturating_sub(overhead);
    if output_bytes == 0 || payload_bytes < result_bytes {
        return Err(CoreError::rejected(
            ErrorCode::LimitExceeded,
            "tool control input cannot fit a result envelope",
        ));
    }
    Ok(ToolResultLimits {
        output_bytes,
        payload_bytes,
        artifact_bytes: None,
    })
}

fn validate_tool_artifacts(
    limits: &ToolResultLimits,
    references: &[ArtifactRef],
) -> Result<(), CoreError> {
    let bytes = artifact_bytes(references)?;
    if limits.artifact_bytes.is_some_and(|bound| bytes > bound) {
        return Err(CoreError::rejected(
            ErrorCode::LimitExceeded,
            "tool evidence exceeds its admitted artifact bound",
        ));
    }
    Ok(())
}

fn validate_tool_payload(
    limits: &ToolResultLimits,
    value: &impl Serialize,
) -> Result<(), CoreError> {
    if serialized_bytes(value)? > limits.payload_bytes {
        return Err(CoreError::rejected(
            ErrorCode::LimitExceeded,
            "tool payload exceeds its admitted serialized bound",
        ));
    }
    Ok(())
}
