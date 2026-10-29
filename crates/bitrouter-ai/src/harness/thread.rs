//! Public harness thread contracts. Execution and storage remain application-owned.

use crate::harness::agent::AgentConfig;
use crate::harness::item::CallRecord;
use crate::harness::runtime::EffectStatus;
use crate::harness::turn::{SteeringReceipt, TurnEvent, TurnLifecycle, TurnReceipt, TurnSnapshot};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionProfile {
    ReadOnly,
    #[default]
    Ask,
    AllowEffects,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    Idle,
    Busy,
    Paused,
    RecoveryRequired,
    Closing,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ThreadSnapshot {
    pub server_instance_id: String,
    pub thread_id: String,
    pub status: ThreadStatus,
    pub workspace: PathBuf,
    pub model: String,
    pub permission_profile: PermissionProfile,
    pub context_version: u64,
    pub cursor: u64,
    pub active_turn_id: Option<String>,
    pub queued: Vec<TurnReceipt>,
    pub pause_reason: Option<String>,
    pub waiting_for_capacity: bool,
}

/// Presentation comes from committed facts. SDK messages are not reconstructed
/// from this view or from its evictable live-output cache.
#[derive(Clone, Serialize, Deserialize)]
pub struct ThreadView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<RecoveryState>,
    pub thread: ThreadSnapshot,
    pub config: AgentConfig,
    pub verification_command: Option<String>,
    /// Active Turn, or the last activated Turn after it settles.
    pub latest_turn: Option<TurnSnapshot>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RecoveryState {
    pub source_server_instance_id: String,
    #[serde(default)]
    pub source_execution_owner: Option<crate::harness::runtime::ExecutionOwner>,
    pub source_cursor: u64,
    pub stored_status: ThreadStatus,
    pub stored_pause_reason: Option<String>,
    pub context_valid: bool,
    pub terminal_checkpoint: bool,
    pub turn: Option<RecoveryTurn>,
    pub blockers: Vec<RecoveryBlocker>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RecoveryTurn {
    #[serde(default)]
    pub resources: Option<crate::harness::resources::HarnessInventory>,
    pub turn_id: String,
    pub user_item_id: String,
    pub cancel_requested: bool,
    pub budget: RecoveryBudget,
    pub steering: Vec<RecoveredSteering>,
    pub unresolved_calls: Vec<CallRecord>,
    #[serde(default)]
    pub continuation_checkpoint: bool,
    #[serde(default)]
    pub outcome: Option<crate::harness::runtime::SettlementOutcome>,
    #[serde(default)]
    pub confirmed_verification: Option<(
        crate::harness::turn::VerificationStatus,
        crate::harness::turn::VerificationEvidence,
    )>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct RecoveryBudget {
    pub model_steps: u32,
    pub tool_calls_known: u32,
    pub tool_calls_unknown: bool,
    pub estimated_spend_microusd: u64,
    pub estimated_spend_available: bool,
    pub usage_unknown_steps: Vec<String>,
    pub active_duration_ms: u64,
    pub active_duration_unknown: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RecoveredSteering {
    pub receipt: SteeringReceipt,
    pub text: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecoveryBlocker {
    OwnershipUnconfirmed,
    EffectUnconfirmed { item_id: String, tool_name: String },
    UnsettledCall { item_id: String, tool_name: String },
    ModelRequestIncomplete { step_id: String },
    InvalidRecord { detail: String },
    BudgetUncertain { detail: String },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ThreadChange {
    ContextRouting {
        turn_id: String,
        inspection: Box<crate::harness::inspection::Inspection>,
    },
    Created {
        view: Box<ThreadView>,
    },
    TurnQueued {
        receipt: TurnReceipt,
        user_item_id: String,
        prompt: String,
    },
    TurnActivated {
        turn_id: String,
        context_version: u64,
    },
    QueuedTurnCancelled {
        turn_id: String,
    },
    QueueResumed,
    Recovered {
        source_server_instance_id: String,
        source_cursor: u64,
    },
    Checkpoint {
        snapshot: ThreadSnapshot,
    },
    TurnLifecycle {
        turn_id: String,
        lifecycle: TurnLifecycle,
    },
    ModelStep {
        turn_id: String,
        step_id: String,
        item_id: String,
        context_version: u64,
    },
    AssistantResponse {
        turn_id: String,
        step_id: String,
        item_id: String,
        request_id: String,
        requested_model: String,
        usage: Option<crate::types::Usage>,
        message: crate::types::Message,
        calls: Vec<CallRecord>,
    },
    AssistantInterrupted {
        turn_id: String,
        step_id: String,
        item_id: String,
        partial: crate::types::Message,
        detail: String,
    },
    ToolIntent {
        turn_id: String,
        step_id: String,
        call: CallRecord,
    },
    ToolResult {
        turn_id: String,
        step_id: String,
        item_id: String,
        message: crate::types::Message,
        effect: EffectStatus,
    },
    VerificationResult {
        turn_id: String,
        call: CallRecord,
        evidence: crate::harness::turn::VerificationEvidence,
        effect: EffectStatus,
        active_duration_ms: u64,
        tool_calls: u32,
    },
    ContextAdvanced {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resources: Option<Box<crate::harness::resources::HarnessInventory>>,
        turn_id: String,
        context_version: u64,
    },
}

/// One acknowledged Thread transaction; sequences are durable root cursors and
/// can have gaps because internal execution facts share the same stream.
#[derive(Clone, Serialize, Deserialize)]
pub struct ThreadEvent {
    pub server_instance_id: String,
    pub thread_id: String,
    pub seq: u64,
    pub timestamp_ms: u64,
    pub changes: Vec<ThreadChange>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadObservation {
    Snapshot {
        view: Box<ThreadView>,
        resynchronized: bool,
        catchup: Vec<ThreadEvent>,
    },
    Event {
        event: Box<ThreadEvent>,
    },
    /// Volatile output anchored to a durable cutoff. It never advances the
    /// Thread cursor and is not replayed by history pagination.
    Live {
        after_cursor: u64,
        event: Box<TurnEvent>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ThreadHistoryPage {
    pub server_instance_id: String,
    pub thread_id: String,
    pub cutoff: u64,
    pub events: Vec<ThreadEvent>,
    pub next_after: Option<u64>,
}

redacted_contract_debug!(
    ThreadSnapshot,
    ThreadView,
    RecoveryState,
    RecoveryTurn,
    RecoveryBudget,
    RecoveredSteering,
    RecoveryBlocker,
    ThreadChange,
    ThreadEvent,
    ThreadObservation,
    ThreadHistoryPage
);

/// Explicitly accept one inspected durable checkpoint. A load alone never
/// grants continuation; retries retain the inspected source and operation key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadRecoveryRequest {
    pub source_server_instance_id: String,
    pub source_cursor: u64,
    pub idempotency_key: String,
}
