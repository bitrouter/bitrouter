//! Public harness turn contracts. Execution and storage remain application-owned.

use crate::harness::agent::ToolMode;
use crate::harness::item::LiveActivity;
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

#[derive(Clone, Serialize, Deserialize)]
pub struct TurnEvent {
    pub thread_id: String,
    pub server_instance_id: String,
    pub turn_id: String,
    pub seq: u64,
    pub timestamp_ms: u64,
    pub payload: TurnEventPayload,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TurnSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_routing: Option<crate::harness::inspection::Inspection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<crate::harness::resources::HarnessInventory>,
    #[serde(default)]
    pub steering: Vec<crate::harness::turn::SteeringReceipt>,
    pub thread_id: String,
    pub server_instance_id: String,
    pub model: String,
    pub turn_id: String,
    pub status: TurnStatus,
    pub cursor: u64,
    pub workspace: PathBuf,
    #[serde(default)]
    pub tool_mode: ToolMode,
    pub final_answer: Option<String>,
    pub detail: Option<String>,
    pub unknown_effect: bool,
    pub verification: VerificationStatus,
    pub verification_evidence: Option<VerificationEvidence>,
    pub pending_input_id: Option<String>,
    pub pending_input: Option<InputRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<LiveActivity>,
}

/// Complete approval metadata is available even when its event was evicted.
#[derive(Clone, Serialize, Deserialize)]
pub struct InputRequest {
    pub request_id: String,
    pub tool_id: String,
    pub tool_name: String,
    pub arguments: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TurnReceipt {
    #[serde(default)]
    pub user_item_id: String,
    pub thread_id: String,
    pub turn_id: String,
    pub queue_order: u64,
    pub status: TurnStatus,
}

/// Small control facts; model and tool content lives only in canonical facts.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnLifecycle {
    Started,
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
    SteeringUpdated {
        receipt: SteeringReceipt,
        text: Option<String>,
    },
    Finished {
        status: TurnStatus,
        detail: String,
        final_answer: Option<String>,
        verification: crate::harness::turn::VerificationStatus,
        verification_evidence: Option<crate::harness::turn::VerificationEvidence>,
        unknown_effect: bool,
    },
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
    TurnEvent,
    TurnSnapshot,
    InputRequest,
    TurnReceipt,
    TurnLifecycle,
    SteeringReceipt
);
