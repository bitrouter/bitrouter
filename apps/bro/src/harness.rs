//! BRO-owned permissions, checkpoint states and live observations.
//! Provider requests and responses use the public AI protocol types.

use serde::{Deserialize, Serialize};

/// The tools authorized by the local workspace grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolMode {
    Coding,
    ReadOnly,
}

/// Only confirmed effects authorize advancing a tool checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EffectStatus {
    Completed,
    NotExecuted,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TurnStatus {
    Running,
    WaitingForInput,
    Completed,
    Failed,
    Cancelled,
    RecoveryRequired,
}

impl TurnStatus {
    pub(crate) fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// Approval binds an exact invocation and its original argument bytes.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct InputRequest {
    pub request_id: String,
    pub tool_id: String,
    pub tool_name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VerificationStatus {
    Passed,
    Failed,
    Unavailable,
    NotRequested,
    Denied,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct VerificationEvidence {
    pub command: String,
    #[serde(default)]
    pub interpreter: Option<serde_json::Value>,
    pub exit_status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub timed_out: bool,
    pub error: Option<String>,
}

/// Display deltas are independent of the durable model and tool results.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum RunEvent {
    AssistantDelta {
        item_id: String,
        text: String,
    },
    ToolOutputDelta {
        id: String,
        source: String,
        text: String,
    },
}
