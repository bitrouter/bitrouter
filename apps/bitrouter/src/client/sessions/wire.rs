//! Application-local contract for the daemon's owner-scoped session endpoint.
//!
//! Clients receive snapshots and sequenced events, never process handles.
//! The nested ACP, capability and initial-settings types remain application
//! dependencies while the session wire contract is separated from execution.

use std::collections::BTreeSet;
use std::fmt;
use std::path::PathBuf;

use agent_client_protocol::schema::v1::{
    ListSessionsResponse, PermissionOption, SessionConfigOption, SessionConfigOptionValue,
    SessionUpdate, ToolCallUpdate,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::acp_cli::LaunchOptions;
use crate::commands::agent_options::{CapabilitySnapshot, RoutingOptions, SessionSelection};
use crate::harness::acp::client::SessionInitialSettings;

pub const SESSION_PROTOCOL_VERSION: u16 = 3;

/// Versioned request carried by `DaemonCommand::Sessions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRequest {
    pub version: u16,
    pub authorization: Option<SessionAuthorization>,
    pub command: SessionCommand,
}

impl SessionRequest {
    /// Request an exact set of local capabilities. Grant issuance is accepted
    /// only on the daemon's owner-scoped local control endpoint.
    pub fn authorize(client_id: String, scopes: BTreeSet<SessionScope>) -> Self {
        Self {
            version: SESSION_PROTOCOL_VERSION,
            authorization: None,
            command: SessionCommand::Authorize { client_id, scopes },
        }
    }

    /// Build a command using a previously issued exact-scope grant.
    pub fn authorized(grant: &SessionGrant, command: SessionCommand) -> Self {
        Self {
            version: SESSION_PROTOCOL_VERSION,
            authorization: Some(grant.authorization.clone()),
            command,
        }
    }
}

/// A command-scoped capability on the owner-only local session endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionScope {
    Start,
    List,
    Peek,
    Transcript,
    Attach,
    Respond,
    Stop,
    Remove,
}

/// Opaque proof that the daemon issued a local capability grant.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionAuthorization(String);

impl fmt::Debug for SessionAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionAuthorization([redacted])")
    }
}

/// Exact-scope local capability grant. Debug output deliberately omits the
/// bearer proof so diagnostics cannot disclose it.
#[derive(Clone, Serialize, Deserialize)]
pub struct SessionGrant {
    pub authorization: SessionAuthorization,
    pub client_id: String,
    pub scopes: BTreeSet<SessionScope>,
    /// `None` means the grant lasts for this daemon lifetime, subject to the
    /// bounded least-recently-used grant table.
    pub expires_at: Option<DateTime<Utc>>,
}

impl fmt::Debug for SessionGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionGrant")
            .field("authorization", &"[redacted]")
            .field("client_id", &self.client_id)
            .field("scopes", &self.scopes)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Local operations. Read-only operations deliberately carry no lease;
/// mutations carry [`ControlFence`] and are generation-fenced.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SessionCommand {
    Authorize {
        client_id: String,
        scopes: BTreeSet<SessionScope>,
    },
    Start(Box<StartRunRequest>),
    List,
    PeekAll,
    Snapshot {
        run_id: String,
    },
    /// v2: inspect an original operation receipt without applying it again.
    ActionStatus {
        fence: ControlFence,
    },
    Attach {
        run_id: String,
        client_id: String,
        after_seq: Option<u64>,
        action_request_id: String,
        takeover: bool,
    },
    Events {
        run_id: String,
        after_seq: u64,
    },
    AcquireLease {
        run_id: String,
        client_id: String,
        mode: LeaseMode,
        action_request_id: String,
        takeover: bool,
    },
    Heartbeat {
        run_id: String,
        client_id: String,
        lease_generation: u64,
    },
    ReleaseLease {
        fence: ControlFence,
    },
    Mutate(SessionMutation),
    NativeList {
        run_id: String,
        cwd: Option<PathBuf>,
        cursor: Option<String>,
    },
    RouteList {
        run_id: String,
    },
    Stop {
        fence: ControlFence,
        confirmed: bool,
    },
    Remove {
        run_id: String,
        action_request_id: String,
    },
}

/// One daemon-owned launch. The daemon resolves its own current configuration;
/// no client-provided `Config` crosses the control boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartRunRequest {
    pub action_request_id: String,
    pub client_id: Option<String>,
    pub label: Option<String>,
    pub agent_id: String,
    pub prompt: Option<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub routing: RoutingOptions,
    #[serde(default)]
    pub launch: LaunchOptions,
    #[serde(default)]
    pub session: SessionSelection,
    pub presentation: Presentation,
    pub parent_run_id: Option<String>,
    #[serde(default)]
    pub allow_shared_directory: bool,
    #[serde(default)]
    pub permission_policy: PermissionPolicy,
    pub result_schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presentation {
    Foreground,
    Background,
}

/// Supervisor-aware permission behavior. `Ask` is intentionally distinct from
/// the ordinary headless `DenyAll` default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionPolicy {
    #[serde(default)]
    pub auto_approve: Vec<String>,
    #[serde(default)]
    pub auto_deny: Vec<String>,
    #[serde(default)]
    pub unmatched: PermissionDefault,
}

impl Default for PermissionPolicy {
    fn default() -> Self {
        Self {
            auto_approve: Vec::new(),
            auto_deny: Vec::new(),
            unmatched: PermissionDefault::Ask,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDefault {
    #[default]
    Ask,
    ApproveAll,
    ApproveReads,
    DenyAll,
}

/// Identity fence shared by every session mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlFence {
    pub run_id: String,
    pub client_id: String,
    pub lease_generation: u64,
    pub action_request_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMutation {
    pub fence: ControlFence,
    pub action: SessionAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SessionAction {
    Prompt {
        text: String,
    },
    Cancel,
    Permission {
        permission_id: String,
        option_id: String,
    },
    SelectSession {
        selection: SessionSelection,
    },
    SetMode {
        mode_id: String,
    },
    SetConfig {
        config_id: String,
        value: SessionConfigOptionValue,
    },
    RouteSet {
        route: String,
    },
    RouteClear,
    MarkReviewed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum SessionResponse {
    Authorized {
        grant: SessionGrant,
    },
    Started {
        snapshot: Box<RunSnapshot>,
        diagnostics: Vec<String>,
    },
    RunSummaries {
        runs: Vec<RunSummary>,
    },
    Runs {
        runs: Vec<RunSnapshot>,
    },
    Snapshot {
        snapshot: Box<RunSnapshot>,
    },
    Attachment {
        attachment: Box<RunAttachment>,
    },
    Events {
        replay: ReplayBatch,
    },
    Lease {
        lease: ControlLease,
    },
    Action {
        acknowledgement: Box<ActionAcknowledgement>,
    },
    ActionStatus {
        acknowledgement: Option<Box<ActionAcknowledgement>>,
    },
    NativeSessions {
        response: ListSessionsResponse,
    },
    RouteState {
        state: RouteState,
    },
    Removed {
        run_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionAcknowledgement {
    pub action_request_id: String,
    pub snapshot: RunSnapshot,
    pub value: Option<ActionValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "value", rename_all = "snake_case")]
pub enum ActionValue {
    ConfigOptions { options: Vec<SessionConfigOption> },
    Route { current: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunAttachment {
    pub snapshot: RunSnapshot,
    pub replay: ReplayBatch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayBatch {
    pub first_retained_seq: u64,
    pub snapshot_seq: u64,
    pub history_complete: bool,
    pub events: Vec<SessionEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    pub seq: u64,
    pub at: DateTime<Utc>,
    pub kind: SessionEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SessionEventKind {
    Lifecycle {
        process: ProcessState,
    },
    Turn {
        turn: TurnState,
    },
    Update {
        update: SessionUpdate,
    },
    PayloadOmitted {
        description: String,
    },
    Permission {
        permission: PendingPermissionSnapshot,
    },
    PermissionResolved {
        permission_id: String,
        option_id: Option<String>,
        automatic: bool,
    },
    Activity {
        text: String,
    },
    UserPrompt {
        text: String,
    },
    TurnSettled {
        result: TurnResult,
    },
    TurnFailed {
        message: String,
    },
    NativeSessionSelected {
        native_session_id: String,
        agent_session_id: Option<String>,
        initial_settings: SessionInitialSettings,
    },
    Lease {
        lease: Option<ControlLease>,
    },
    Review {
        review: ReviewState,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSnapshot {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_deliveries: Vec<crate::skills::delivery::Report>,
    pub run_id: String,
    pub label: String,
    pub agent_id: String,
    pub native_session_id: Option<String>,
    pub agent_session_id: Option<String>,
    pub via: Option<String>,
    pub cwd: PathBuf,
    pub claim_key: PathBuf,
    pub parent_run_id: Option<String>,
    pub presentation: Presentation,
    pub capabilities: Option<CapabilitySnapshot>,
    pub initial_settings: Option<SessionInitialSettings>,
    pub process: ProcessState,
    pub turn: TurnState,
    pub attention: AttentionState,
    pub attachment: AttachmentState,
    pub review: ReviewState,
    pub activity: String,
    pub confirmed_route: Option<String>,
    pub attributed_cost: Option<AttributedCost>,
    pub pending_permissions: Vec<PendingPermissionSnapshot>,
    pub failure: Option<String>,
    pub lease: Option<ControlLease>,
    pub last_seq: u64,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Metadata-only list projection. It intentionally excludes capabilities,
/// initial settings, failures, permission context/options, prompts, results,
/// and tool input/output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub run_id: String,
    pub label: String,
    pub agent_id: String,
    pub native_session_id: Option<String>,
    pub agent_session_id: Option<String>,
    pub via: Option<String>,
    pub cwd: PathBuf,
    pub parent_run_id: Option<String>,
    pub presentation: Presentation,
    pub process: ProcessState,
    pub turn: TurnState,
    pub attention: AttentionState,
    pub attachment: AttachmentState,
    pub review: ReviewState,
    pub activity: String,
    pub confirmed_route: Option<String>,
    pub attributed_cost: Option<AttributedCost>,
    pub pending_permissions: Vec<PendingPermissionSummary>,
    pub has_failure: bool,
    pub last_seq: u64,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Safe permission identity for list rows, without ACP metadata, raw values,
/// content, locations, or the available response options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingPermissionSummary {
    pub permission_id: String,
    pub title: Option<String>,
}

impl From<&RunSnapshot> for RunSummary {
    fn from(snapshot: &RunSnapshot) -> Self {
        Self {
            run_id: snapshot.run_id.clone(),
            label: snapshot.label.clone(),
            agent_id: snapshot.agent_id.clone(),
            native_session_id: snapshot.native_session_id.clone(),
            agent_session_id: snapshot.agent_session_id.clone(),
            via: snapshot.via.clone(),
            cwd: snapshot.cwd.clone(),
            parent_run_id: snapshot.parent_run_id.clone(),
            presentation: snapshot.presentation,
            process: snapshot.process,
            turn: snapshot.turn,
            attention: snapshot.attention,
            attachment: snapshot.attachment.clone(),
            review: snapshot.review,
            activity: if snapshot.attention == AttentionState::Error {
                "Needs attention".to_string()
            } else {
                snapshot.activity.clone()
            },
            confirmed_route: snapshot.confirmed_route.clone(),
            attributed_cost: snapshot.attributed_cost.clone(),
            pending_permissions: snapshot
                .pending_permissions
                .iter()
                .map(|permission| PendingPermissionSummary {
                    permission_id: permission.permission_id.clone(),
                    title: permission.tool_call.fields.title.clone(),
                })
                .collect(),
            has_failure: snapshot.failure.is_some(),
            last_seq: snapshot.last_seq,
            started_at: snapshot.started_at,
            updated_at: snapshot.updated_at,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    Idle,
    Submitting,
    Working,
    Cancelling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionState {
    None,
    Question,
    Permission,
    Result,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AttachmentState {
    Detached,
    Observed,
    Controlled { client_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Unread,
    Reviewed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingPermissionSnapshot {
    pub permission_id: String,
    pub tool_call: ToolCallUpdate,
    pub options: Vec<PermissionOption>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttributedCost {
    pub amount: f64,
    pub currency: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnResult {
    pub stop_reason: String,
    pub result: Option<serde_json::Value>,
    pub schema_ok: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteState {
    pub available: Vec<String>,
    pub current: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseMode {
    Transient,
    Attached,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlLease {
    pub owner_client_id: String,
    pub generation: u64,
    pub mode: LeaseMode,
    pub expires_at: DateTime<Utc>,
}

impl SessionAuthorization {
    pub(crate) fn new(token: String) -> Self {
        Self(token)
    }
}
