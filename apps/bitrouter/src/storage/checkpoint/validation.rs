//! Shared host-policy, manifest, artifact and frozen tool-result checks for durable
//! harness records. Wire data lives in AI; live connection admission stays core-owned.

use bitrouter_ai::core::protocol::{
    ArtifactRef, COLLABORATION_TOOLS, Capabilities, CoreError, ErrorCode, HarnessManifest,
    HarnessTool, Limits, ToolObservation, ToolResult, ToolResultLimits, VERSION, validate_id,
};
use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::{serialized_bytes, sha256};

pub(crate) const CONTEXT_TOOLS: &[&str] = &[
    "context_search",
    "context_recall",
    "context_publish",
    "context_extract",
];

/// Check host policy without a live scheduler, for both admission and restoration.
pub(crate) fn limits_within(limits: &Limits, host: &Limits) -> Result<(), CoreError> {
    limits.validate()?;
    host.validate()?;
    let requested = [
        limits.active_models as u64,
        limits.agents as u64,
        limits.child_depth as u64,
        limits.model_attempts as u64,
        limits.outstanding_tools as u64,
        limits.total_tools as u64,
        limits.queued_runs as u64,
        limits.mailbox_messages as u64,
        limits.active_seconds,
        limits.checkpoint_bytes,
        limits.unacknowledged_bytes,
        limits.ephemeral_bytes,
        limits.input_bytes,
    ];
    let allowed = [
        host.active_models as u64,
        host.agents as u64,
        host.child_depth as u64,
        host.model_attempts as u64,
        host.outstanding_tools as u64,
        host.total_tools as u64,
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

fn negotiate_capabilities(
    capabilities: &Capabilities,
    version: u32,
    required: &[String],
    limits: &Limits,
) -> Result<(), CoreError> {
    if version != capabilities.version || version != VERSION {
        return Err(CoreError::rejected(
            ErrorCode::UnsupportedVersion,
            "unsupported harness protocol version",
        ));
    }
    if let Some(feature) = required
        .iter()
        .find(|feature| !capabilities.operations.contains(feature))
    {
        return Err(CoreError::rejected(
            ErrorCode::UnsupportedCapability,
            format!("unsupported required feature: {feature}"),
        ));
    }
    limits_within(limits, &capabilities.limits)
}

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
        negotiate_capabilities(capabilities, VERSION, &self.required_features, limits)?;
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
                || CONTEXT_TOOLS.contains(&tool.name.as_str())
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

#[cfg(feature = "core")]
pub(crate) fn tool_result_limits(
    input_bytes: u64,
    output_bytes: u64,
) -> Result<ToolResultLimits, CoreError> {
    use bitrouter_ai::core::protocol::{ClientMessage, Command, ToolOutcome};
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

#[cfg(test)]
mod host_policy_tests;
