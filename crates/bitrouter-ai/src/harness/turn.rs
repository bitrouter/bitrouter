//! Public harness turn contracts. Execution and storage remain application-owned.

use crate::harness::agent::ToolMode;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Queued,
    Accepted,
    Running,
    WaitingForInput,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
    RecoveryRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Passed,
    Failed,
    Unavailable,
    NotRequested,
    Denied,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct VerificationEvidence {
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

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnEventPayload {
    SteeringUpdated {
        receipt: crate::harness::turn::SteeringReceipt,
        text: Option<String>,
    },
    TurnQueued {
        user_item_id: String,
        prompt: String,
        queue_order: u64,
    },
    Accepted {
        #[serde(default)]
        user_item_id: String,
        prompt: String,
        workspace: PathBuf,
        model: String,
        #[serde(default)]
        tool_mode: ToolMode,
        #[serde(default)]
        idempotency_key: Option<String>,
        #[serde(default)]
        request_fingerprint: Option<String>,
    },
    Started,
    AssistantDelta {
        #[serde(default)]
        item_id: String,
        text: String,
    },
    ToolOutputDelta {
        id: String,
        source: String,
        text: String,
    },
    InputRequested {
        request_id: String,
        tool_id: String,
        tool_name: String,
        arguments: String,
    },
    InputResolved {
        request_id: String,
        approved: bool,
    },
    CancelRequested,
    Finished {
        status: TurnStatus,
        detail: String,
        final_answer: Option<String>,
        verification: VerificationStatus,
        verification_evidence: Option<VerificationEvidence>,
        unknown_effect: bool,
    },
}

/// Complete approval metadata is available even when its event was evicted.
#[derive(Clone, Serialize, Deserialize)]
pub struct InputRequest {
    pub request_id: String,
    pub tool_id: String,
    pub tool_name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteeringStatus {
    Received,
    Applied,
    NotApplied,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SteeringReceipt {
    pub input_id: String,
    pub turn_id: String,
    pub order: u64,
    pub status: SteeringStatus,
    pub context_version: Option<u64>,
    pub next_step_id: Option<String>,
    pub reason: Option<String>,
}

impl TurnStatus {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

redacted_contract_debug!(
    VerificationEvidence,
    TurnEventPayload,
    InputRequest,
    SteeringReceipt
);
