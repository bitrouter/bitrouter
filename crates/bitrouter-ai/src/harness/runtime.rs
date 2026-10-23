//! Public harness runtime contracts. Execution and storage remain application-owned.

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct RuntimeCapabilities {
    #[serde(default = "legacy_native_execution")]
    pub native_execution: bool,
    #[serde(default)]
    pub startup_discovery: Option<StartupDiscovery>,
    pub server_instance_id: String,
    pub limits: RuntimeLimits,
    #[serde(default)]
    pub execution_ownership: Option<crate::harness::runtime::OwnerClaim>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeLimits {
    pub startup_roots: usize,
    pub startup_records: u64,
    pub startup_metadata_bytes: usize,
    pub recovery_readers: usize,
    pub recovery_page_records: usize,
    pub recovery_page_bytes: usize,
    pub recovery_records_per_thread: u64,
    pub history_page_bytes: usize,
    pub events_per_thread: usize,
    pub event_bytes_per_thread: usize,
    pub subscribers_per_thread: usize,
    pub subscriber_bytes_per_thread: usize,
    pub steering_inputs_per_turn: usize,
    pub steering_bytes_per_turn: usize,
    pub hot_threads: usize,
    pub queued_turns_per_thread: usize,
    pub context_bytes_per_thread: usize,
    pub hot_context_bytes: usize,
    pub tools_per_turn: usize,
    pub global_tools: usize,
    pub active_turns: usize,
    pub retained_turns: usize,
    pub retained_bytes: usize,
    pub retention_seconds: u64,
    pub subscriber_queue: usize,
    pub request_bytes: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    UnknownThread,
    Unauthorized,
    RecoveryRequired,
    InvalidRequest,
    UnknownTurn,
    Conflict,
    Overloaded,
    ShuttingDown,
    InstanceChanged,
    ResyncRequired,
    StorageUnavailable,
    UnsupportedCapability,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StartupDiscovery {
    pub complete: bool,
    pub writer_fenced: bool,
    pub index_cutoff: u64,
    pub inspected_roots: usize,
    pub blocked_workspaces: usize,
    pub scanned_records: u64,
    pub error: Option<ErrorCode>,
}

/// A stopped owner is durable proof that its service sealed admission, joined
/// owned execution and confirmed cleanup before releasing write authority.
/// Abrupt process loss leaves this record active; a new epoch cannot retire it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionOwner {
    pub server_instance_id: String,
    pub generation: u64,
    pub stopped_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum OwnerClaim {
    Acquired { owner: ExecutionOwner },
    Blocked { owner: ExecutionOwner },
    Unfenced,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SettlementOutcome {
    pub status: crate::harness::agent::RunStatus,
    pub final_answer: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EffectStatus {
    Completed,
    NotExecuted,
    Unknown,
}

fn legacy_native_execution() -> bool {
    true
}

redacted_contract_debug!(RuntimeCapabilities, SettlementOutcome);
