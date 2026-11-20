//! Application-owned turn observations and lifecycle records.

use super::item::LiveActivity;
use bitrouter_ai::harness::{
    agent::ToolMode,
    turn::{
        InputRequest, SteeringReceipt, TurnEventPayload, TurnStatus, VerificationEvidence,
        VerificationStatus,
    },
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
    pub context_routing: Option<crate::managed_contract::harness::inspection::Inspection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<crate::managed_contract::harness::resources::HarnessInventory>,
    #[serde(default)]
    pub steering: Vec<bitrouter_ai::harness::turn::SteeringReceipt>,
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
        verification: bitrouter_ai::harness::turn::VerificationStatus,
        verification_evidence: Option<bitrouter_ai::harness::turn::VerificationEvidence>,
        unknown_effect: bool,
    },
}

redacted_contract_debug!(TurnEvent, TurnSnapshot, TurnReceipt, TurnLifecycle);
