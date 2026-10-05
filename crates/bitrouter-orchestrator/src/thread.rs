//! Durable conversation and input contracts. Transports supply authenticated
//! callers and never select a workspace or permission grant on their behalf.

use std::path::PathBuf;

use bitrouter_sdk::caller::CallerContext;
use serde::{Deserialize, Serialize};

use crate::agent::AgentConfig;
use crate::item::CallRecord;
use crate::store::EffectStatus;
use crate::turn::{
    SteeringReceipt, TurnEvent, TurnLifecycle, TurnReceipt, TurnSnapshot, TurnStatus,
};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadTarget {
    pub thread_id: String,
    pub server_instance_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// A cold directory projection. Listing does not load context or subscribe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadDirectoryEntry {
    pub thread: ThreadSnapshot,
    pub turn_status: Option<TurnStatus>,
    pub turn_id: Option<String>,
    pub needs_input: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadDirectoryPage {
    /// Fixes directory membership, not the mutable status of each Thread.
    pub cutoff: u64,
    pub next_after: Option<u64>,
    pub entries: Vec<ThreadDirectoryEntry>,
}

/// Presentation comes from committed facts. SDK messages are not reconstructed
/// from this view or from its evictable live-output cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<RecoveryState>,
    pub thread: ThreadSnapshot,
    pub config: AgentConfig,
    pub verification_command: Option<String>,
    /// Active Turn, or the last activated Turn after it settles.
    pub latest_turn: Option<TurnSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryState {
    pub source_server_instance_id: String,
    #[serde(default)]
    pub source_execution_owner: Option<crate::store::ExecutionOwner>,
    pub source_cursor: u64,
    pub stored_status: ThreadStatus,
    pub stored_pause_reason: Option<String>,
    pub context_valid: bool,
    pub terminal_checkpoint: bool,
    pub turn: Option<RecoveryTurn>,
    pub blockers: Vec<RecoveryBlocker>,
}

/// Explicitly accept one inspected durable checkpoint. A load alone never
/// grants continuation; retries retain the inspected source and operation key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadRecoveryRequest {
    pub source_server_instance_id: String,
    pub source_cursor: u64,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryTurn {
    #[serde(default)]
    pub resources: Option<crate::harness::HarnessInventory>,
    pub turn_id: String,
    pub user_item_id: String,
    pub cancel_requested: bool,
    pub budget: RecoveryBudget,
    pub steering: Vec<RecoveredSteering>,
    pub unresolved_calls: Vec<CallRecord>,
    #[serde(default)]
    pub continuation_checkpoint: bool,
    #[serde(default)]
    pub outcome: Option<crate::store::SettlementOutcome>,
    #[serde(default)]
    pub confirmed_verification: Option<(
        crate::turn::VerificationStatus,
        crate::turn::VerificationEvidence,
    )>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveredSteering {
    pub receipt: SteeringReceipt,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecoveryBlocker {
    OwnershipUnconfirmed,
    EffectUnconfirmed { item_id: String, tool_name: String },
    UnsettledCall { item_id: String, tool_name: String },
    ModelRequestIncomplete { step_id: String },
    InvalidRecord { detail: String },
    BudgetUncertain { detail: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ThreadChange {
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
        usage: Option<bitrouter_sdk::language_model::Usage>,
        message: bitrouter_sdk::language_model::Message,
        calls: Vec<CallRecord>,
    },
    AssistantInterrupted {
        turn_id: String,
        step_id: String,
        item_id: String,
        partial: bitrouter_sdk::language_model::Message,
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
        message: bitrouter_sdk::language_model::Message,
        effect: EffectStatus,
    },
    VerificationResult {
        turn_id: String,
        call: CallRecord,
        evidence: crate::turn::VerificationEvidence,
        effect: EffectStatus,
        active_duration_ms: u64,
        tool_calls: u32,
    },
    ContextAdvanced {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resources: Option<Box<crate::harness::HarnessInventory>>,
        turn_id: String,
        context_version: u64,
    },
}

/// One acknowledged Thread transaction; sequences are durable root cursors and
/// can have gaps because internal execution facts share the same stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadEvent {
    pub server_instance_id: String,
    pub thread_id: String,
    pub seq: u64,
    pub timestamp_ms: u64,
    pub changes: Vec<ThreadChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

pub struct ThreadHistoryRequest {
    pub after: u64,
    pub cutoff: Option<u64>,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadHistoryPage {
    pub server_instance_id: String,
    pub thread_id: String,
    pub cutoff: u64,
    pub events: Vec<ThreadEvent>,
    pub next_after: Option<u64>,
}

pub struct ThreadRequest {
    pub caller: CallerContext,
    pub workspace: PathBuf,
    pub config: AgentConfig,
    pub permission_profile: PermissionProfile,
    pub verification_command: Option<String>,
    pub idempotency_key: String,
}

/// Trusted host configuration; this is not accepted from an input RPC.
pub struct WorkspaceGrant {
    pub workspace: PathBuf,
    pub permission_profiles: Vec<PermissionProfile>,
}
