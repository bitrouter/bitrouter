//! Configuration-state and guarded-reload wire contracts.
//!
//! Admission, timestamps, operation ownership and execution remain service-owned.

use serde::{Deserialize, Serialize};

/// Where the daemon's primary configuration comes from.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSourceKind {
    File,
    Default,
}

/// Whether the currently saved primary configuration can be inspected.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedConfigState {
    Available,
    Generated,
    Missing,
    Invalid,
    Unavailable,
}

/// Relationship between inspected configuration inputs and the running daemon.
/// Startup-owned settings and separate policy sources participate; external
/// OAuth, registry, and discovery services are not probed by this comparison.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunningConfigState {
    InSync,
    ReloadRequired,
    RestartRequired,
    Mixed,
    Unknown,
}

/// State of a configuration source loaded separately from `bitrouter.yaml`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuxiliaryConfigState {
    InSync,
    ReloadRequired,
    NotConfigured,
    Missing,
    Invalid,
    Unavailable,
    Unknown,
}

/// Redaction-safe saved/running configuration contract.
///
/// Field names are fixed schema categories. Configuration values, source
/// bytes, credentials, and value-derived hashes never leave the process.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigurationState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_instance_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    pub source: ConfigSourceKind,
    pub saved: SavedConfigState,
    pub running: RunningConfigState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reload_required_fields: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restart_required_fields: Vec<String>,
    pub named_policy: AuxiliaryConfigState,
    pub access_policies: AuxiliaryConfigState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reload: Option<ReloadReport>,
    /// Bounded mixed/unknown outcomes retained for this daemon boot, oldest
    /// first. A later preparation failure cannot erase the report that made
    /// the running configuration mixed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mixed_state_history: Vec<ReloadReport>,
}

/// Consistency of the runtime configuration after the last completed reload.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReloadConsistency {
    /// Every participant in the last completed reload reached its intended
    /// state, or a failed candidate made no live change.
    Consistent,
    /// A reload may have changed some live participants before it failed or
    /// was interrupted. Operators must inspect the participant report.
    Mixed,
}

/// A terminal reload outcome. `Unknown` is reserved for an interrupted
/// in-process attempt after mutation began; it is never evidence of failure.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReloadOutcome {
    Succeeded,
    Failed,
    PartiallyApplied,
    Unknown,
}

/// A live subsystem participating in a daemon reload.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReloadParticipant {
    RoutingTable,
    UpstreamTimeoutClients,
    NamedPolicyRuntime,
    AccessPolicyStore,
}

/// What one reload participant actually did.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReloadParticipantOutcome {
    Applied,
    Unchanged,
    Failed,
    NotAttempted,
}

/// A safe, structured participant failure. Raw configuration and upstream
/// errors may carry credentials, so the externally visible message is fixed
/// and the detailed error remains in process logs only.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReloadFailure {
    pub code: String,
    pub message: String,
}

/// The actual result for one reload participant.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReloadParticipantReport {
    pub participant: ReloadParticipant,
    pub outcome: ReloadParticipantOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ReloadFailure>,
}

/// A completed reload attempt. The report deliberately does not claim a
/// cross-subsystem transaction: a partial outcome identifies every participant
/// that changed before a later failure.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReloadReport {
    pub server_instance_id: String,
    pub generation: u64,
    pub outcome: ReloadOutcome,
    pub participants: Vec<ReloadParticipantReport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restart_required_fields: Vec<String>,
    /// RFC 3339 UTC timestamp captured when execution started.
    pub started_at: String,
    /// RFC 3339 UTC timestamp captured when this terminal report was recorded.
    pub completed_at: String,
}

/// State exposed by the coordinator. A generation marks a terminal attempt
/// boundary; it is not an atomic snapshot of every request path.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReloadState {
    pub server_instance_id: String,
    pub generation: u64,
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<ReloadReport>,
    pub consistency: ReloadConsistency,
    /// Bounded reports for prior mixed/unknown states in this process boot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mixed_state_history: Vec<ReloadReport>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReloadInput {
    pub request_id: String,
    pub expected_server_instance_id: String,
    pub expected_generation: u64,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Running,
    Succeeded,
    Failed,
    PartiallyApplied,
    Unknown,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationReport {
    pub request_id: String,
    pub server_instance_id: String,
    pub generation_before: u64,
    pub status: OperationStatus,
    pub accepted_at_unix_ms: i64,
    pub completed_at_unix_ms: Option<i64>,
    pub retain_until_unix_ms: Option<i64>,
    pub lookup_url: String,
    pub result: Option<ReloadReport>,
}

impl OperationReport {
    pub fn is_running(&self) -> bool {
        self.status == OperationStatus::Running
    }
    pub fn succeeded(&self) -> bool {
        self.status == OperationStatus::Succeeded
    }
}
