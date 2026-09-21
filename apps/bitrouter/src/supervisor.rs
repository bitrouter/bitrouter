//! Daemon-owned ACP session lifecycle and its owner-scoped local wire contract.
//!
//! The supervisor is the process owner. Terminal and headless clients receive
//! snapshots and sequenced events; they never receive an in-process
//! `SessionHandle`. The only transport mounted for this protocol is the
//! daemon's owner-scoped local control endpoint. The remote control server does
//! not dispatch [`SessionRequest`] and therefore cannot start or inspect local
//! agent processes.

use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    ListSessionsResponse, PermissionOption, SessionConfigOption, SessionConfigOptionValue,
    SessionUpdate, ToolCallUpdate,
};
use anyhow::{Context, Result};
use bitrouter_sdk::acp::client::SessionInitialSettings;
use bitrouter_sdk::config::{Config, ConfigRoutingTable};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify, RwLock, mpsc, oneshot};
use uuid::Uuid;

use crate::acp_cli::{
    CapabilitySnapshot, LaunchOptions, RoutingOptions, SessionHandle, SessionHost,
    SessionSelection, SpawnContext, is_lifecycle_cancelled, is_lifecycle_teardown_unconfirmed,
};
use crate::paths::ConfigSource;

/// Version of the local session-control JSON contract.
pub const SESSION_PROTOCOL_VERSION: u16 = 1;

const JOURNAL_EVENT_LIMIT: usize = 2_048;
const JOURNAL_PAYLOAD_LIMIT: usize = 64 * 1_024;
const LEASE_TTL: Duration = Duration::from_secs(30);
const GRANT_LIMIT: usize = 256;

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

// Runtime implementation follows the wire declarations so presentation
// clients can depend on the types without depending on the process owner.

/// Daemon-owned collection of supervised ACP controller lifetimes.
#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<SupervisorInner>,
}

struct SupervisorInner {
    source: ConfigSource,
    routing: Arc<ConfigRoutingTable>,
    runs: RwLock<HashMap<String, Arc<Run>>>,
    starts: Mutex<HashMap<String, StartState>>,
    removed: Mutex<HashMap<String, String>>,
    grants: Mutex<HashMap<SessionAuthorization, GrantRecord>>,
    claims: Arc<Mutex<HashMap<PathBuf, Vec<String>>>>,
    ledger: Arc<LedgerStore>,
    shutting_down: AtomicBool,
    shutdown_token: tokio_util::sync::CancellationToken,
}

struct GrantRecord {
    client_id: String,
    scopes: BTreeSet<SessionScope>,
    last_used_at: DateTime<Utc>,
}

enum StartState {
    Starting {
        notify: Arc<Notify>,
        fingerprint: String,
    },
    Started {
        run_id: String,
        fingerprint: String,
    },
    Failed {
        message: String,
        fingerprint: String,
    },
}

struct Run {
    state: Mutex<RunState>,
    mutation_gate: Mutex<()>,
    startup_cancel: tokio_util::sync::CancellationToken,
    startup_completion: Mutex<StartupCompletion>,
    startup_finished: Notify,
    handle: Mutex<Option<SessionHandle>>,
    session_commands: Mutex<Option<mpsc::UnboundedSender<SessionActorCommand>>>,
    pending_permissions: Mutex<HashMap<String, bitrouter_sdk::acp::client::PendingPermission>>,
    permission_policy: PermissionPolicy,
    result_schema: Option<serde_json::Value>,
    turn_timeout: Option<Duration>,
    ledger: Arc<LedgerStore>,
    claims: Arc<Mutex<HashMap<PathBuf, Vec<String>>>>,
}

#[derive(Clone, Copy)]
enum StartupCompletion {
    Pending,
    Finished { teardown_confirmed: bool },
}

struct RunState {
    snapshot: RunSnapshot,
    journal: VecDeque<JournalEntry>,
    history_complete: bool,
    next_seq: u64,
    lease_generation: u64,
    turn_generation: u64,
    current_reply: String,
    actions: HashMap<String, ActionRecord>,
}

#[derive(Clone)]
struct ActionRecord {
    owner_client_id: String,
    lease_generation: u64,
    fingerprint: String,
    acknowledgement: ActionAcknowledgement,
}

type UpdateStream =
    Pin<Box<dyn futures::Stream<Item = bitrouter_sdk::acp::client::SequencedSessionUpdate> + Send>>;

enum SessionActorCommand {
    Prompt(PromptActorInput),
    ReplaceUpdates {
        updates: UpdateStream,
        acknowledgement: oneshot::Sender<()>,
    },
}

struct PromptActorInput {
    client: Box<bitrouter_sdk::acp::client::AcpClient>,
    session_id: String,
    text: String,
    generation: u64,
    deadline: Option<tokio::time::Instant>,
}

struct JournalEntry {
    event: SessionEvent,
    pinned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LedgerFile {
    version: u16,
    runs: Vec<LedgerEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LedgerEntry {
    run_id: String,
    label: String,
    agent_id: String,
    native_session_id: Option<String>,
    agent_session_id: Option<String>,
    via: Option<String>,
    cwd: PathBuf,
    claim_key: PathBuf,
    parent_run_id: Option<String>,
    presentation: Presentation,
    process: ProcessState,
    activity: String,
    failure: Option<String>,
    started_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

struct LedgerStore {
    path: PathBuf,
    entries: Mutex<HashMap<String, LedgerEntry>>,
    write: Mutex<()>,
}

impl LedgerStore {
    async fn open(path: PathBuf) -> Result<(Arc<Self>, Vec<LedgerEntry>)> {
        let prior = match tokio::fs::read(&path).await {
            Ok(bytes) => {
                serde_json::from_slice::<LedgerFile>(&bytes)
                    .with_context(|| format!("parsing supervisor ledger {}", path.display()))?
                    .runs
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading supervisor ledger {}", path.display()));
            }
        };
        let now = Utc::now();
        let entries = prior
            .into_iter()
            .map(|mut entry| {
                if matches!(
                    entry.process,
                    ProcessState::Starting | ProcessState::Running | ProcessState::Stopping
                ) {
                    entry.process = ProcessState::Interrupted;
                    entry.activity = "Interrupted by daemon restart".to_string();
                    entry.failure = Some("the owning BitRouter daemon restarted".to_string());
                    entry.updated_at = now;
                }
                (entry.run_id.clone(), entry)
            })
            .collect::<HashMap<_, _>>();
        let recovered = entries.values().cloned().collect();
        let store = Arc::new(Self {
            path,
            entries: Mutex::new(entries),
            write: Mutex::new(()),
        });
        store.flush().await?;
        Ok((store, recovered))
    }

    async fn record(&self, snapshot: &RunSnapshot) -> Result<()> {
        self.entries
            .lock()
            .await
            .insert(snapshot.run_id.clone(), LedgerEntry::from(snapshot));
        self.flush().await
    }

    async fn remove(&self, run_id: &str) -> Result<()> {
        self.entries.lock().await.remove(run_id);
        self.flush().await
    }

    async fn flush(&self) -> Result<()> {
        let _write = self.write.lock().await;
        let mut runs = self
            .entries
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        runs.sort_by_key(|run| run.started_at);
        let bytes = serde_json::to_vec_pretty(&LedgerFile {
            version: SESSION_PROTOCOL_VERSION,
            runs,
        })
        .context("serialising supervisor ledger")?;
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("creating supervisor ledger directory {}", parent.display())
            })?;
        }
        let temporary = self.path.with_extension("json.tmp");
        tokio::fs::write(&temporary, bytes)
            .await
            .with_context(|| format!("writing supervisor ledger {}", temporary.display()))?;
        tokio::fs::rename(&temporary, &self.path)
            .await
            .with_context(|| format!("installing supervisor ledger {}", self.path.display()))
    }
}

impl From<&RunSnapshot> for LedgerEntry {
    fn from(snapshot: &RunSnapshot) -> Self {
        Self {
            run_id: snapshot.run_id.clone(),
            label: snapshot.label.clone(),
            agent_id: snapshot.agent_id.clone(),
            native_session_id: snapshot.native_session_id.clone(),
            agent_session_id: snapshot.agent_session_id.clone(),
            via: snapshot.via.clone(),
            cwd: snapshot.cwd.clone(),
            claim_key: snapshot.claim_key.clone(),
            parent_run_id: snapshot.parent_run_id.clone(),
            presentation: snapshot.presentation,
            process: snapshot.process,
            activity: snapshot.activity.clone(),
            failure: snapshot.failure.clone(),
            started_at: snapshot.started_at,
            updated_at: snapshot.updated_at,
        }
    }
}

impl Supervisor {
    /// Restore the minimal ledger and mark every formerly live row interrupted.
    pub async fn open(source: ConfigSource, routing: Arc<ConfigRoutingTable>) -> Result<Self> {
        let identity = crate::daemon_locator::config_identity_digest(&source)?;
        let path = source
            .home()
            .join(format!("supervisor-runs-v1-{identity}.json"));
        let (ledger, recovered) = LedgerStore::open(path).await?;
        let claims = Arc::new(Mutex::new(HashMap::new()));
        let runs = recovered
            .into_iter()
            .map(|entry| {
                let id = entry.run_id.clone();
                let run = Run::from_ledger(entry, ledger.clone(), claims.clone());
                (id, Arc::new(run))
            })
            .collect();
        Ok(Self {
            inner: Arc::new(SupervisorInner {
                source,
                routing,
                runs: RwLock::new(runs),
                starts: Mutex::new(HashMap::new()),
                removed: Mutex::new(HashMap::new()),
                grants: Mutex::new(HashMap::new()),
                claims,
                ledger,
                shutting_down: AtomicBool::new(false),
                shutdown_token: tokio_util::sync::CancellationToken::new(),
            }),
        })
    }

    /// Dispatch one owner-scoped local command.
    pub async fn dispatch(&self, request: SessionRequest) -> Result<SessionResponse> {
        anyhow::ensure!(
            request.version == SESSION_PROTOCOL_VERSION,
            "unsupported session protocol version {}; expected {}",
            request.version,
            SESSION_PROTOCOL_VERSION
        );
        if let SessionCommand::Authorize { client_id, scopes } = &request.command {
            anyhow::ensure!(
                request.authorization.is_none(),
                "grant issuance must not carry an existing authorization"
            );
            return self.issue_grant(client_id, scopes).await;
        }
        self.authorize_command(request.authorization.as_ref(), &request.command)
            .await?;
        match request.command {
            SessionCommand::Authorize { .. } => {
                Err(anyhow::anyhow!("grant issuance was not dispatched"))
            }
            SessionCommand::Start(request) => self.start(*request).await,
            SessionCommand::List => self.list().await,
            SessionCommand::PeekAll => self.peek_all().await,
            SessionCommand::Snapshot { run_id } => Ok(SessionResponse::Snapshot {
                snapshot: Box::new(self.run(&run_id).await?.snapshot().await),
            }),
            SessionCommand::Attach {
                run_id,
                client_id,
                after_seq,
                action_request_id,
                takeover,
            } => {
                let run = self.run(&run_id).await?;
                let _gate = run.mutation_gate.lock().await;
                let fingerprint =
                    action_fingerprint("attach", &(after_seq, takeover, LeaseMode::Attached))?;
                if run
                    .cached_action(&action_request_id, &client_id, None, &fingerprint)
                    .await?
                    .is_none()
                {
                    let lease = run
                        .acquire_lease(&client_id, LeaseMode::Attached, takeover)
                        .await?;
                    run.remember_action(
                        &action_request_id,
                        &client_id,
                        lease.generation,
                        fingerprint,
                        None,
                    )
                    .await;
                }
                let attachment = run.attachment(after_seq).await;
                Ok(SessionResponse::Attachment {
                    attachment: Box::new(attachment),
                })
            }
            SessionCommand::Events { run_id, after_seq } => {
                let run = self.run(&run_id).await?;
                Ok(SessionResponse::Events {
                    replay: run.replay(Some(after_seq)).await,
                })
            }
            SessionCommand::AcquireLease {
                run_id,
                client_id,
                mode,
                action_request_id,
                takeover,
            } => {
                let run = self.run(&run_id).await?;
                let _gate = run.mutation_gate.lock().await;
                let fingerprint = action_fingerprint("acquire_lease", &(mode, takeover))?;
                if let Some(ack) = run
                    .cached_action(&action_request_id, &client_id, None, &fingerprint)
                    .await?
                {
                    return ack
                        .snapshot
                        .lease
                        .ok_or_else(|| anyhow::anyhow!("cached lease action has no lease"))
                        .map(|lease| SessionResponse::Lease { lease });
                }
                let lease = run.acquire_lease(&client_id, mode, takeover).await?;
                run.remember_action(
                    &action_request_id,
                    &client_id,
                    lease.generation,
                    fingerprint,
                    None,
                )
                .await;
                Ok(SessionResponse::Lease { lease })
            }
            SessionCommand::Heartbeat {
                run_id,
                client_id,
                lease_generation,
            } => {
                let run = self.run(&run_id).await?;
                let _gate = run.mutation_gate.lock().await;
                let lease = run.heartbeat(&client_id, lease_generation).await?;
                Ok(SessionResponse::Lease { lease })
            }
            SessionCommand::ReleaseLease { fence } => {
                let run = self.run(&fence.run_id).await?;
                let _gate = run.mutation_gate.lock().await;
                let fingerprint = action_fingerprint("release_lease", &())?;
                if let Some(ack) = run
                    .cached_action(
                        &fence.action_request_id,
                        &fence.client_id,
                        Some(fence.lease_generation),
                        &fingerprint,
                    )
                    .await?
                {
                    return Ok(SessionResponse::Action {
                        acknowledgement: Box::new(ack),
                    });
                }
                run.verify_fence(&fence).await?;
                run.release_lease(&fence.client_id, fence.lease_generation)
                    .await?;
                let acknowledgement = run
                    .remember_action(
                        &fence.action_request_id,
                        &fence.client_id,
                        fence.lease_generation,
                        fingerprint,
                        None,
                    )
                    .await;
                Ok(SessionResponse::Action {
                    acknowledgement: Box::new(acknowledgement),
                })
            }
            SessionCommand::Mutate(mutation) => self.mutate(mutation).await,
            SessionCommand::NativeList {
                run_id,
                cwd,
                cursor,
            } => {
                let run = self.run(&run_id).await?;
                let client = run.client().await?;
                let response = client.list_sessions(cwd, cursor).await?;
                Ok(SessionResponse::NativeSessions { response })
            }
            SessionCommand::RouteList { run_id } => {
                let run = self.run(&run_id).await?;
                let (client, session_id) = run.client_and_session().await?;
                let state = client.route_list(&session_id).await?;
                Ok(SessionResponse::RouteState {
                    state: RouteState {
                        available: state.available,
                        current: state.current,
                    },
                })
            }
            SessionCommand::Stop { fence, confirmed } => {
                anyhow::ensure!(confirmed, "stopping a supervised run requires confirmation");
                let run = self.run(&fence.run_id).await?;
                let _gate = run.mutation_gate.lock().await;
                let fingerprint = action_fingerprint("stop", &confirmed)?;
                if let Some(ack) = run
                    .cached_action(
                        &fence.action_request_id,
                        &fence.client_id,
                        Some(fence.lease_generation),
                        &fingerprint,
                    )
                    .await?
                {
                    return Ok(SessionResponse::Action {
                        acknowledgement: Box::new(ack),
                    });
                }
                run.verify_fence(&fence).await?;
                run.stop().await?;
                run.release_if_transient(&fence.client_id, fence.lease_generation)
                    .await;
                let acknowledgement = run
                    .remember_action(
                        &fence.action_request_id,
                        &fence.client_id,
                        fence.lease_generation,
                        fingerprint,
                        None,
                    )
                    .await;
                Ok(SessionResponse::Action {
                    acknowledgement: Box::new(acknowledgement),
                })
            }
            SessionCommand::Remove {
                run_id,
                action_request_id,
            } => self.remove(&run_id, &action_request_id).await,
        }
    }

    async fn issue_grant(
        &self,
        client_id: &str,
        scopes: &BTreeSet<SessionScope>,
    ) -> Result<SessionResponse> {
        validate_identifier("session client", client_id)?;
        anyhow::ensure!(!scopes.is_empty(), "at least one session scope is required");
        let now = Utc::now();
        let mut grants = self.inner.grants.lock().await;
        if let Some((authorization, grant)) = grants
            .iter_mut()
            .find(|(_, grant)| grant.client_id == client_id && grant.scopes == *scopes)
        {
            grant.last_used_at = now;
            return Ok(SessionResponse::Authorized {
                grant: SessionGrant {
                    authorization: authorization.clone(),
                    client_id: client_id.to_string(),
                    scopes: scopes.clone(),
                    expires_at: None,
                },
            });
        }
        if grants.len() >= GRANT_LIMIT
            && let Some(oldest) = grants
                .iter()
                .min_by_key(|(_, grant)| grant.last_used_at)
                .map(|(authorization, _)| authorization.clone())
        {
            grants.remove(&oldest);
        }
        let authorization = SessionAuthorization(Uuid::new_v4().to_string());
        grants.insert(
            authorization.clone(),
            GrantRecord {
                client_id: client_id.to_string(),
                scopes: scopes.clone(),
                last_used_at: now,
            },
        );
        Ok(SessionResponse::Authorized {
            grant: SessionGrant {
                authorization,
                client_id: client_id.to_string(),
                scopes: scopes.clone(),
                expires_at: None,
            },
        })
    }

    async fn authorize_command(
        &self,
        authorization: Option<&SessionAuthorization>,
        command: &SessionCommand,
    ) -> Result<()> {
        let authorization = authorization.context("session authorization is required")?;
        let now = Utc::now();
        let mut grants = self.inner.grants.lock().await;
        let Some(grant) = grants.get_mut(authorization) else {
            return Err(anyhow::anyhow!(
                "session authorization is invalid or expired"
            ));
        };
        let (all, any, bound_client) = command_scope_requirement(command);
        anyhow::ensure!(
            all.iter().all(|scope| grant.scopes.contains(scope))
                && (any.is_empty() || any.iter().any(|scope| grant.scopes.contains(scope))),
            "session authorization does not grant the required command scope"
        );
        if let Some(client_id) = bound_client {
            anyhow::ensure!(
                grant.client_id == client_id,
                "session authorization belongs to a different client"
            );
        }
        grant.last_used_at = now;
        Ok(())
    }

    /// Deterministically stop every live controller before daemon shutdown.
    pub async fn shutdown(&self) {
        self.inner.shutting_down.store(true, Ordering::SeqCst);
        self.inner.shutdown_token.cancel();
        loop {
            let starting = self
                .inner
                .starts
                .lock()
                .await
                .values()
                .filter_map(|state| match state {
                    StartState::Starting { notify, .. } => Some(notify.clone()),
                    StartState::Started { .. } | StartState::Failed { .. } => None,
                })
                .collect::<Vec<_>>();
            if starting.is_empty() {
                break;
            }
            for notify in starting {
                tokio::select! {
                    _ = notify.notified() => {}
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
            }
        }
        let runs = self
            .inner
            .runs
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for run in runs {
            let _gate = run.mutation_gate.lock().await;
            if let Err(error) = run.stop().await {
                let run_id = run.snapshot().await.run_id;
                tracing::warn!(%run_id, %error, "supervised run shutdown failed");
            }
        }
    }

    async fn run(&self, run_id: &str) -> Result<Arc<Run>> {
        self.inner
            .runs
            .read()
            .await
            .get(run_id)
            .cloned()
            .with_context(|| format!("supervised run '{run_id}' was not found"))
    }

    async fn list(&self) -> Result<SessionResponse> {
        let runs = self
            .inner
            .runs
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut summaries = Vec::with_capacity(runs.len());
        for run in runs {
            let snapshot = run.snapshot().await;
            summaries.push(RunSummary::from(&snapshot));
        }
        summaries.sort_by_key(|summary| Reverse(summary.updated_at));
        Ok(SessionResponse::RunSummaries { runs: summaries })
    }

    async fn peek_all(&self) -> Result<SessionResponse> {
        let runs = self
            .inner
            .runs
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut snapshots = Vec::with_capacity(runs.len());
        for run in runs {
            snapshots.push(run.snapshot().await);
        }
        snapshots.sort_by_key(|snapshot| Reverse(snapshot.updated_at));
        Ok(SessionResponse::Runs { runs: snapshots })
    }

    async fn start(&self, request: StartRunRequest) -> Result<SessionResponse> {
        validate_identifier("action request", &request.action_request_id)?;
        anyhow::ensure!(
            !self.inner.shutting_down.load(Ordering::SeqCst),
            "the session supervisor is shutting down"
        );
        let fingerprint =
            serde_json::to_string(&request).context("fingerprinting start request")?;
        loop {
            let wait = {
                let mut starts = self.inner.starts.lock().await;
                match starts.get(&request.action_request_id) {
                    Some(StartState::Started {
                        run_id,
                        fingerprint: accepted,
                    }) => {
                        anyhow::ensure!(
                            accepted == &fingerprint,
                            "action request id was already used for a different start request"
                        );
                        let run_id = run_id.clone();
                        drop(starts);
                        return Ok(SessionResponse::Started {
                            snapshot: Box::new(self.run(&run_id).await?.snapshot().await),
                            diagnostics: Vec::new(),
                        });
                    }
                    Some(StartState::Failed {
                        message,
                        fingerprint: accepted,
                    }) => {
                        anyhow::ensure!(
                            accepted == &fingerprint,
                            "action request id was already used for a different start request"
                        );
                        return Err(anyhow::anyhow!(message.clone()));
                    }
                    Some(StartState::Starting {
                        notify,
                        fingerprint: accepted,
                    }) => {
                        anyhow::ensure!(
                            accepted == &fingerprint,
                            "action request id was already used for a different start request"
                        );
                        Some(notify.clone())
                    }
                    None => {
                        anyhow::ensure!(
                            !self.inner.shutting_down.load(Ordering::SeqCst),
                            "the session supervisor is shutting down"
                        );
                        let notify = Arc::new(Notify::new());
                        starts.insert(
                            request.action_request_id.clone(),
                            StartState::Starting {
                                notify,
                                fingerprint: fingerprint.clone(),
                            },
                        );
                        None
                    }
                }
            };
            if let Some(wait) = wait {
                tokio::select! {
                    _ = wait.notified() => {}
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
                continue;
            }
            let result = self.start_new(request.clone()).await;
            let mut starts = self.inner.starts.lock().await;
            let notify = match starts.get(&request.action_request_id) {
                Some(StartState::Starting { notify, .. }) => Some(notify.clone()),
                _ => None,
            };
            match &result {
                Ok(SessionResponse::Started { snapshot, .. }) => {
                    starts.insert(
                        request.action_request_id.clone(),
                        StartState::Started {
                            run_id: snapshot.run_id.clone(),
                            fingerprint: fingerprint.clone(),
                        },
                    );
                }
                Ok(_) => {
                    starts.insert(
                        request.action_request_id.clone(),
                        StartState::Failed {
                            message: "invalid start response".to_string(),
                            fingerprint: fingerprint.clone(),
                        },
                    );
                }
                Err(error) => {
                    starts.insert(
                        request.action_request_id.clone(),
                        StartState::Failed {
                            message: format!("{error:#}"),
                            fingerprint: fingerprint.clone(),
                        },
                    );
                }
            }
            if let Some(notify) = notify {
                notify.notify_waiters();
            }
            return result;
        }
    }

    async fn start_new(&self, request: StartRunRequest) -> Result<SessionResponse> {
        anyhow::ensure!(
            !self.inner.shutting_down.load(Ordering::SeqCst),
            "the session supervisor is shutting down"
        );
        validate_start(&request)?;
        if let Some(schema) = &request.result_schema {
            jsonschema::validator_for(schema)
                .map_err(|error| anyhow::anyhow!("result schema is not valid: {error}"))?;
        }
        let cwd = tokio::fs::canonicalize(&request.cwd)
            .await
            .with_context(|| format!("resolving working directory {}", request.cwd.display()))?;
        let run_id = Uuid::new_v4().to_string();
        let claim_key = claim_key(&cwd).await?;
        let now = Utc::now();
        let lease = match request.presentation {
            Presentation::Foreground => {
                let client_id = request.client_id.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("a foreground supervised run requires a client id")
                })?;
                Some(ControlLease {
                    owner_client_id: client_id.to_string(),
                    generation: 1,
                    mode: LeaseMode::Attached,
                    expires_at: lease_expiry(),
                })
            }
            Presentation::Background => None,
        };
        anyhow::ensure!(
            !self.inner.shutting_down.load(Ordering::SeqCst),
            "the session supervisor is shutting down"
        );
        self.reserve_claim(&claim_key, &run_id, request.allow_shared_directory)
            .await?;
        let snapshot = RunSnapshot {
            run_id: run_id.clone(),
            label: request
                .label
                .clone()
                .unwrap_or_else(|| format!("{}-{}", request.agent_id, &run_id[..8])),
            agent_id: request.agent_id.clone(),
            native_session_id: None,
            agent_session_id: None,
            via: None,
            cwd: cwd.clone(),
            claim_key,
            parent_run_id: request.parent_run_id.clone(),
            presentation: request.presentation,
            capabilities: None,
            initial_settings: None,
            process: ProcessState::Starting,
            turn: TurnState::Idle,
            attention: AttentionState::None,
            attachment: match &lease {
                Some(lease) => AttachmentState::Controlled {
                    client_id: lease.owner_client_id.clone(),
                },
                None => AttachmentState::Detached,
            },
            review: ReviewState::Reviewed,
            activity: "Starting".to_string(),
            confirmed_route: None,
            attributed_cost: None,
            pending_permissions: Vec::new(),
            failure: None,
            lease,
            last_seq: 0,
            started_at: now,
            updated_at: now,
        };
        let run = Arc::new(Run::new(
            snapshot,
            request.permission_policy.clone(),
            request.result_schema.clone(),
            request.launch.turn_timeout,
            self.inner.ledger.clone(),
            self.inner.claims.clone(),
            self.inner.shutdown_token.child_token(),
        ));
        self.inner
            .runs
            .write()
            .await
            .insert(run_id.clone(), run.clone());
        if let Err(error) = run.persist().await {
            run.fail_launch(format!("failed to persist starting session: {error:#}"))
                .await;
            run.finish_startup(true).await;
            return Err(error);
        }

        let config: Config = self.inner.routing.snapshot_config();
        let mut diagnostics = Vec::new();
        let mut launch = request.launch;
        launch.turn_timeout = None;
        let host = {
            let mut record_diagnostic = |message| diagnostics.push(message);
            tokio::select! {
                biased;
                _ = run.startup_cancel.cancelled() => None,
                result = SessionHost::prepare_with_diagnostics(
                    SpawnContext {
                        source: &self.inner.source,
                        config,
                        agent_id: &request.agent_id,
                        options: launch,
                        routing: request.routing,
                    },
                    false,
                    &mut record_diagnostic,
                ) => Some(result),
            }
        };
        let Some(host) = host else {
            run.finish_startup(true).await;
            anyhow::bail!("supervised session startup was cancelled");
        };
        let host = match host {
            Ok(host) => host,
            Err(error) => {
                run.fail_launch(format!("session preparation failed: {error:#}"))
                    .await;
                run.finish_startup(true).await;
                return Err(error);
            }
        };
        let mut handle = match host
            .open_with_cancel(&request.session, cwd, &run.startup_cancel)
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                let teardown_confirmed = !is_lifecycle_teardown_unconfirmed(&error);
                if is_lifecycle_cancelled(&error) && run.startup_cancel.is_cancelled() {
                    run.finish_startup(true).await;
                    return Err(error);
                }
                if teardown_confirmed {
                    run.fail_launch(format!("agent failed to start: {error:#}"))
                        .await;
                } else {
                    run.fail(format!("agent failed to start: {error:#}")).await;
                }
                run.finish_startup(teardown_confirmed).await;
                return Err(error);
            }
        };
        let updates = handle.take_sequenced_updates();
        let permissions = handle.take_permissions();
        let closed = handle.closed();
        let route = if handle.capabilities.route_list {
            tokio::select! {
                biased;
                _ = run.startup_cancel.cancelled() => None,
                state = handle.client.route_list(&handle.session_id) => {
                    state.ok().and_then(|state| state.current)
                }
            }
        } else {
            None
        };
        let agent_id = handle.agent_id.clone();
        let native_session_id = handle.session_id.clone();
        let agent_session_id = handle.agent_session_id.clone();
        let via = handle.via.clone();
        let capabilities = handle.capabilities.clone();
        let initial_settings = handle.initial_settings.clone();
        let cancelled = run.startup_cancel.is_cancelled()
            || run.state.lock().await.snapshot.process != ProcessState::Starting;
        if cancelled {
            let clean = handle.shutdown().await;
            if !clean {
                run.fail("cancelled startup cleanup did not confirm".to_string())
                    .await;
            }
            run.finish_startup(clean).await;
            if clean {
                anyhow::bail!("supervised session startup was cancelled");
            }
            anyhow::bail!("cancelled startup cleanup did not confirm");
        }
        *run.handle.lock().await = Some(handle);
        let published = {
            let mut state = run.state.lock().await;
            if state.snapshot.process == ProcessState::Starting
                && !run.startup_cancel.is_cancelled()
            {
                state.snapshot.agent_id = agent_id;
                state.snapshot.native_session_id = Some(native_session_id.clone());
                state.snapshot.agent_session_id = agent_session_id.clone();
                state.snapshot.via = via;
                state.snapshot.capabilities = Some(capabilities);
                state.snapshot.initial_settings = Some(initial_settings.clone());
                state.snapshot.confirmed_route = route;
                if let Some(lease) = &mut state.snapshot.lease {
                    lease.expires_at = lease_expiry();
                }
                state.snapshot.process = ProcessState::Running;
                state.snapshot.activity = "Idle".to_string();
                state.append(
                    SessionEventKind::Lifecycle {
                        process: ProcessState::Running,
                    },
                    false,
                );
                state.append(
                    SessionEventKind::NativeSessionSelected {
                        native_session_id,
                        agent_session_id,
                        initial_settings,
                    },
                    false,
                );
                true
            } else {
                false
            }
        };
        if !published {
            let clean = run
                .discard_startup_handle("cancelled startup cleanup did not confirm")
                .await;
            run.finish_startup(clean).await;
            anyhow::ensure!(clean, "cancelled startup cleanup did not confirm");
            anyhow::bail!("supervised session startup was cancelled");
        }
        if let Err(error) = run.persist().await {
            let clean = run
                .discard_startup_handle(
                    "live-session persistence failed and cleanup did not confirm",
                )
                .await;
            if clean {
                run.fail_launch(format!("failed to persist live session: {error:#}"))
                    .await;
            }
            run.finish_startup(clean).await;
            return Err(error);
        }
        if run.startup_cancel.is_cancelled() {
            let clean = run
                .discard_startup_handle("cancelled startup cleanup did not confirm")
                .await;
            run.finish_startup(clean).await;
            anyhow::ensure!(clean, "cancelled startup cleanup did not confirm");
            anyhow::bail!("supervised session startup was cancelled");
        }
        run.spawn_session_actor(updates).await;
        run.spawn_permission_forwarder(permissions);
        run.spawn_closed_watcher(closed);
        run.finish_startup(true).await;
        if let Some(prompt) = request.prompt {
            let _gate = run.mutation_gate.lock().await;
            anyhow::ensure!(
                !run.startup_cancel.is_cancelled()
                    && run.state.lock().await.snapshot.process == ProcessState::Running,
                "supervised session stopped before its initial prompt"
            );
            run.submit_prompt(prompt).await?;
        }
        Ok(SessionResponse::Started {
            snapshot: Box::new(run.snapshot().await),
            diagnostics,
        })
    }

    async fn reserve_claim(&self, claim: &Path, run_id: &str, allow_shared: bool) -> Result<()> {
        let mut claims = self.inner.claims.lock().await;
        let owners = claims.entry(claim.to_path_buf()).or_default();
        if !allow_shared && !owners.is_empty() {
            anyhow::bail!(
                "working directory is already claimed by supervised run '{}'; use a separate worktree or explicitly allow sharing",
                owners.join(", ")
            );
        }
        owners.push(run_id.to_string());
        Ok(())
    }

    async fn mutate(&self, mutation: SessionMutation) -> Result<SessionResponse> {
        let run = self.run(&mutation.fence.run_id).await?;
        let _gate = run.mutation_gate.lock().await;
        let fingerprint = action_fingerprint("mutate", &mutation.action)?;
        if let Some(ack) = run
            .cached_action(
                &mutation.fence.action_request_id,
                &mutation.fence.client_id,
                Some(mutation.fence.lease_generation),
                &fingerprint,
            )
            .await?
        {
            return Ok(SessionResponse::Action {
                acknowledgement: Box::new(ack),
            });
        }
        run.verify_fence(&mutation.fence).await?;
        let value = match mutation.action {
            SessionAction::Prompt { text } => {
                run.submit_prompt(text).await?;
                None
            }
            SessionAction::Cancel => {
                run.cancel().await?;
                None
            }
            SessionAction::Permission {
                permission_id,
                option_id,
            } => {
                run.resolve_permission(&permission_id, &option_id, false)
                    .await?;
                None
            }
            SessionAction::SelectSession { selection } => {
                run.select_session(selection).await?;
                None
            }
            SessionAction::SetMode { mode_id } => {
                let (client, session_id) = run.client_and_session().await?;
                client.set_session_mode(&session_id, mode_id).await?;
                None
            }
            SessionAction::SetConfig { config_id, value } => {
                let (client, session_id) = run.client_and_session().await?;
                let response = client
                    .set_session_config_option(&session_id, config_id, value)
                    .await?;
                {
                    let mut state = run.state.lock().await;
                    if let Some(settings) = &mut state.snapshot.initial_settings {
                        settings.config_options = Some(response.config_options.clone());
                    }
                }
                Some(ActionValue::ConfigOptions {
                    options: response.config_options,
                })
            }
            SessionAction::RouteSet { route } => {
                let (client, session_id) = run.client_and_session().await?;
                let current = client.route_set(&session_id, &route).await?;
                run.state.lock().await.snapshot.confirmed_route = Some(current.clone());
                Some(ActionValue::Route {
                    current: Some(current),
                })
            }
            SessionAction::RouteClear => {
                let (client, session_id) = run.client_and_session().await?;
                client.route_reset(&session_id).await?;
                run.state.lock().await.snapshot.confirmed_route = None;
                Some(ActionValue::Route { current: None })
            }
            SessionAction::MarkReviewed => {
                run.mark_reviewed().await;
                None
            }
        };
        run.release_if_transient(&mutation.fence.client_id, mutation.fence.lease_generation)
            .await;
        let acknowledgement = run
            .remember_action(
                &mutation.fence.action_request_id,
                &mutation.fence.client_id,
                mutation.fence.lease_generation,
                fingerprint,
                value,
            )
            .await;
        Ok(SessionResponse::Action {
            acknowledgement: Box::new(acknowledgement),
        })
    }

    async fn remove(&self, run_id: &str, action_request_id: &str) -> Result<SessionResponse> {
        validate_identifier("action request", action_request_id)?;
        if let Some(removed) = self.inner.removed.lock().await.get(action_request_id) {
            anyhow::ensure!(
                removed == run_id,
                "action request id was already used to remove a different run"
            );
            return Ok(SessionResponse::Removed {
                run_id: removed.clone(),
            });
        }
        let run = self.run(run_id).await?;
        let snapshot = run.snapshot().await;
        anyhow::ensure!(
            matches!(
                snapshot.process,
                ProcessState::Stopped | ProcessState::Failed | ProcessState::Interrupted
            ),
            "run must be stopped, failed, or interrupted before removal"
        );
        anyhow::ensure!(
            run.handle.lock().await.is_none(),
            "run cleanup has not settled; stop it before removal"
        );
        anyhow::ensure!(
            run.startup_teardown_confirmed().await == Some(true),
            "run startup cleanup was not confirmed; metadata must remain until daemon restart"
        );
        self.inner.runs.write().await.remove(run_id);
        self.inner.ledger.remove(run_id).await?;
        self.inner
            .removed
            .lock()
            .await
            .insert(action_request_id.to_string(), run_id.to_string());
        Ok(SessionResponse::Removed {
            run_id: run_id.to_string(),
        })
    }
}

impl Run {
    fn new(
        snapshot: RunSnapshot,
        permission_policy: PermissionPolicy,
        result_schema: Option<serde_json::Value>,
        turn_timeout: Option<Duration>,
        ledger: Arc<LedgerStore>,
        claims: Arc<Mutex<HashMap<PathBuf, Vec<String>>>>,
        startup_cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        let lease_generation = snapshot.lease.as_ref().map_or(0, |lease| lease.generation);
        let startup_completion = if snapshot.process == ProcessState::Starting {
            StartupCompletion::Pending
        } else {
            StartupCompletion::Finished {
                teardown_confirmed: true,
            }
        };
        Self {
            state: Mutex::new(RunState {
                snapshot,
                journal: VecDeque::new(),
                history_complete: true,
                next_seq: 1,
                lease_generation,
                turn_generation: 0,
                current_reply: String::new(),
                actions: HashMap::new(),
            }),
            mutation_gate: Mutex::new(()),
            startup_cancel,
            startup_completion: Mutex::new(startup_completion),
            startup_finished: Notify::new(),
            handle: Mutex::new(None),
            session_commands: Mutex::new(None),
            pending_permissions: Mutex::new(HashMap::new()),
            permission_policy,
            result_schema,
            turn_timeout,
            ledger,
            claims,
        }
    }

    fn from_ledger(
        entry: LedgerEntry,
        ledger: Arc<LedgerStore>,
        claims: Arc<Mutex<HashMap<PathBuf, Vec<String>>>>,
    ) -> Self {
        let snapshot = RunSnapshot {
            run_id: entry.run_id,
            label: entry.label,
            agent_id: entry.agent_id,
            native_session_id: entry.native_session_id,
            agent_session_id: entry.agent_session_id,
            via: entry.via,
            cwd: entry.cwd,
            claim_key: entry.claim_key,
            parent_run_id: entry.parent_run_id,
            presentation: entry.presentation,
            capabilities: None,
            initial_settings: None,
            process: entry.process,
            turn: TurnState::Idle,
            attention: if entry.failure.is_some() {
                AttentionState::Error
            } else {
                AttentionState::None
            },
            attachment: AttachmentState::Detached,
            review: if entry.failure.is_some() {
                ReviewState::Unread
            } else {
                ReviewState::Reviewed
            },
            activity: entry.activity,
            confirmed_route: None,
            attributed_cost: None,
            pending_permissions: Vec::new(),
            failure: entry.failure,
            lease: None,
            last_seq: 0,
            started_at: entry.started_at,
            updated_at: entry.updated_at,
        };
        let mut run = Self::new(
            snapshot,
            PermissionPolicy::default(),
            None,
            None,
            ledger,
            claims,
            tokio_util::sync::CancellationToken::new(),
        );
        run.state.get_mut().history_complete = false;
        run
    }

    async fn snapshot(&self) -> RunSnapshot {
        let mut snapshot = self.state.lock().await.snapshot.clone();
        if snapshot
            .lease
            .as_ref()
            .is_some_and(|lease| lease.expires_at <= Utc::now())
        {
            self.expire_lease().await;
            snapshot = self.state.lock().await.snapshot.clone();
        }
        snapshot
    }

    async fn persist(&self) -> Result<()> {
        self.ledger.record(&self.state.lock().await.snapshot).await
    }

    async fn finish_startup(&self, teardown_confirmed: bool) {
        let mut completion = self.startup_completion.lock().await;
        if matches!(*completion, StartupCompletion::Pending) {
            *completion = StartupCompletion::Finished { teardown_confirmed };
            self.startup_finished.notify_waiters();
        }
    }

    async fn wait_for_startup(&self) -> bool {
        loop {
            let notified = self.startup_finished.notified();
            let completion = *self.startup_completion.lock().await;
            match completion {
                StartupCompletion::Pending => notified.await,
                StartupCompletion::Finished { teardown_confirmed } => return teardown_confirmed,
            }
        }
    }

    async fn startup_pending(&self) -> bool {
        matches!(
            *self.startup_completion.lock().await,
            StartupCompletion::Pending
        )
    }

    async fn startup_teardown_confirmed(&self) -> Option<bool> {
        match *self.startup_completion.lock().await {
            StartupCompletion::Pending => None,
            StartupCompletion::Finished { teardown_confirmed } => Some(teardown_confirmed),
        }
    }

    async fn confirm_startup_teardown(&self) {
        *self.startup_completion.lock().await = StartupCompletion::Finished {
            teardown_confirmed: true,
        };
    }

    async fn discard_startup_handle(&self, message: &str) -> bool {
        let clean = match self.handle.lock().await.as_mut() {
            Some(handle) => handle.shutdown().await,
            None => true,
        };
        if clean {
            *self.handle.lock().await = None;
        } else {
            self.fail(message.to_string()).await;
        }
        clean
    }

    async fn fail(&self, message: String) {
        self.abandon_permissions(false).await;
        {
            let mut state = self.state.lock().await;
            state.snapshot.process = ProcessState::Failed;
            state.snapshot.turn = TurnState::Idle;
            state.snapshot.attention = AttentionState::Error;
            state.snapshot.review = ReviewState::Unread;
            state.snapshot.activity = message.clone();
            state.snapshot.failure = Some(message.clone());
            state.append(SessionEventKind::TurnFailed { message }, true);
        }
        if let Err(error) = self.persist().await {
            tracing::warn!(%error, "failed to persist supervised run failure");
        }
    }

    async fn fail_launch(&self, message: String) {
        self.fail(message).await;
        self.release_claim().await;
    }

    async fn release_claim(&self) {
        let snapshot = self.state.lock().await.snapshot.clone();
        let mut claims = self.claims.lock().await;
        if let Some(owners) = claims.get_mut(&snapshot.claim_key) {
            owners.retain(|owner| owner != &snapshot.run_id);
            if owners.is_empty() {
                claims.remove(&snapshot.claim_key);
            }
        }
    }

    async fn client(&self) -> Result<bitrouter_sdk::acp::client::AcpClient> {
        self.handle
            .lock()
            .await
            .as_ref()
            .map(|handle| handle.client.clone())
            .context("supervised controller is not live")
    }

    async fn client_and_session(&self) -> Result<(bitrouter_sdk::acp::client::AcpClient, String)> {
        let handle = self.handle.lock().await;
        let handle = handle
            .as_ref()
            .context("supervised controller is not live")?;
        Ok((handle.client.clone(), handle.session_id.clone()))
    }

    async fn spawn_session_actor(self: &Arc<Self>, updates: UpdateStream) {
        let (sender, receiver) = mpsc::unbounded_channel();
        *self.session_commands.lock().await = Some(sender);
        let run = self.clone();
        tokio::spawn(async move {
            run.session_actor(updates, receiver).await;
        });
    }

    async fn session_actor(
        self: Arc<Self>,
        mut updates: UpdateStream,
        mut commands: mpsc::UnboundedReceiver<SessionActorCommand>,
    ) {
        let mut updates_open = true;
        let mut last_update_sequence = 0;
        loop {
            let command = if updates_open {
                tokio::select! {
                    biased;
                    update = updates.next() => {
                        match update {
                            Some(update) => {
                                self.record_sequenced_update(update, &mut last_update_sequence)
                                    .await;
                            }
                            None => updates_open = false,
                        }
                        continue;
                    }
                    command = commands.recv() => command,
                }
            } else {
                commands.recv().await
            };
            let Some(command) = command else {
                return;
            };
            match command {
                SessionActorCommand::Prompt(prompt) => {
                    self.run_prompt(
                        &mut updates,
                        &mut updates_open,
                        &mut last_update_sequence,
                        prompt,
                    )
                    .await;
                }
                SessionActorCommand::ReplaceUpdates {
                    updates: replacement,
                    acknowledgement,
                } => {
                    updates = replacement;
                    updates_open = true;
                    last_update_sequence = 0;
                    let _ = acknowledgement.send(());
                }
            }
        }
    }

    async fn run_prompt(
        &self,
        updates: &mut UpdateStream,
        updates_open: &mut bool,
        last_update_sequence: &mut u64,
        input: PromptActorInput,
    ) {
        let mut prompt = Box::pin(
            input
                .client
                .prompt_with_boundary(&input.session_id, &input.text),
        );
        let initial_timeout = self
            .turn_timeout
            .unwrap_or_else(|| Duration::from_secs(100 * 365 * 24 * 60 * 60));
        let deadline = tokio::time::sleep_until(input.deadline.unwrap_or_else(|| {
            tokio::time::Instant::now() + Duration::from_secs(100 * 365 * 24 * 60 * 60)
        }));
        tokio::pin!(deadline);
        let mut timed_out = false;
        loop {
            tokio::select! {
                biased;
                update = updates.next(), if *updates_open => {
                    match update {
                        Some(update) => {
                            self.record_sequenced_update(update, last_update_sequence).await;
                        }
                        None => *updates_open = false,
                    }
                }
                result = &mut prompt => {
                    match result {
                        Ok(outcome) => {
                            if let Err(message) = self
                                .drain_updates_through(
                                    updates,
                                    updates_open,
                                    last_update_sequence,
                                    outcome.notification_boundary,
                                )
                                .await
                            {
                                self.fail_turn_if_current(input.generation, message).await;
                                return;
                            }
                            if timed_out {
                                self.fail_turn_if_current(
                                    input.generation,
                                    format!("turn timed out after {} seconds", initial_timeout.as_secs()),
                                )
                                .await;
                            } else {
                                let stop_reason = serde_json::to_value(outcome.response.stop_reason)
                                    .ok()
                                    .and_then(|value| value.as_str().map(ToOwned::to_owned))
                                    .unwrap_or_else(|| "unknown".to_string());
                                self.settle_turn(input.generation, stop_reason).await;
                            }
                        }
                        Err(error) => {
                            self.fail_turn_if_current(input.generation, format!("{error:#}"))
                                .await;
                        }
                    }
                    return;
                }
                _ = &mut deadline => {
                    if timed_out {
                        self.fail_if_current(
                            input.generation,
                            format!(
                                "turn timed out after {} seconds and cancellation did not settle",
                                initial_timeout.as_secs()
                            ),
                        )
                        .await;
                        return;
                    }
                    if self.is_turn_cancelling(input.generation).await {
                        deadline.as_mut().reset(
                            tokio::time::Instant::now()
                                + Duration::from_secs(100 * 365 * 24 * 60 * 60),
                        );
                        continue;
                    }
                    timed_out = true;
                    self.mark_cancelling_if_current(input.generation).await;
                    self.abandon_permissions(true).await;
                    if let Err(error) = input.client.cancel(&input.session_id).await {
                        tracing::warn!(%error, "timed-out supervised turn cancellation failed");
                    }
                    deadline.as_mut().reset(
                        tokio::time::Instant::now()
                            + bitrouter_sdk::acp::client::AcpClient::cancellation_grace(),
                    );
                }
            }
        }
    }

    async fn drain_updates_through(
        &self,
        updates: &mut UpdateStream,
        updates_open: &mut bool,
        last_update_sequence: &mut u64,
        boundary: u64,
    ) -> std::result::Result<(), String> {
        while *last_update_sequence < boundary {
            let Some(update) = updates.next().await else {
                *updates_open = false;
                return Err(format!(
                    "ACP update stream ended before prompt boundary {boundary}"
                ));
            };
            self.record_sequenced_update(update, last_update_sequence)
                .await;
        }
        Ok(())
    }

    async fn record_sequenced_update(
        &self,
        entry: bitrouter_sdk::acp::client::SequencedSessionUpdate,
        last_update_sequence: &mut u64,
    ) {
        *last_update_sequence = (*last_update_sequence).max(entry.sequence);
        if let Some(update) = entry.update {
            self.record_update(update).await;
        }
    }

    fn spawn_permission_forwarder(
        self: &Arc<Self>,
        mut permissions: std::pin::Pin<
            Box<dyn futures::Stream<Item = bitrouter_sdk::acp::client::PendingPermission> + Send>,
        >,
    ) {
        let run = self.clone();
        tokio::spawn(async move {
            while let Some(permission) = permissions.next().await {
                run.record_permission(permission).await;
            }
        });
    }

    fn spawn_closed_watcher(self: &Arc<Self>, closed: futures::future::BoxFuture<'static, ()>) {
        let run = self.clone();
        tokio::spawn(async move {
            closed.await;
            let _gate = run.mutation_gate.lock().await;
            let should_fail = matches!(
                run.state.lock().await.snapshot.process,
                ProcessState::Starting | ProcessState::Running
            );
            if should_fail {
                run.fail("ACP adapter exited unexpectedly".to_string())
                    .await;
                if let Err(error) = run.stop().await {
                    tracing::warn!(%error, "exited supervised controller cleanup did not confirm");
                }
            }
        });
    }

    async fn record_update(&self, update: SessionUpdate) {
        let mut state = self.state.lock().await;
        if let Some(bitrouter_sdk::acp::translate::SessionUpdateKind::MessageChunk {
            text, ..
        }) = bitrouter_sdk::acp::translate::translate(update.clone())
        {
            state.current_reply.push_str(&text);
        }
        let event = SessionEventKind::Update {
            update: update.clone(),
        };
        let event = if serde_json::to_vec(&event)
            .map_or(true, |bytes| bytes.len() > JOURNAL_PAYLOAD_LIMIT)
        {
            SessionEventKind::PayloadOmitted {
                description: "large ACP update was not retained by BitRouter".to_string(),
            }
        } else {
            event
        };
        if let SessionUpdate::ToolCallUpdate(tool) = &update
            && let Some(title) = &tool.fields.title
        {
            state.snapshot.activity = title.clone();
        }
        if let SessionUpdate::UsageUpdate(usage) = &update
            && let Some(cost) = &usage.cost
        {
            let marker = usage
                .meta
                .as_ref()
                .and_then(|meta| meta.get(bitrouter_tui::cost::COST_PROVENANCE_META_KEY));
            let source = match marker {
                None => Some("harness"),
                Some(value)
                    if value.as_str() == Some(bitrouter_tui::cost::COST_PROVENANCE_ROUTER) =>
                {
                    Some("router")
                }
                Some(_) => None,
            };
            state.snapshot.attributed_cost = source.map(|source| AttributedCost {
                amount: cost.amount,
                currency: cost.currency.clone(),
                source: source.to_string(),
            });
        }
        state.append(event, false);
    }

    async fn record_permission(&self, permission: bitrouter_sdk::acp::client::PendingPermission) {
        let snapshot = PendingPermissionSnapshot {
            permission_id: permission.request_id.clone(),
            tool_call: permission.tool_call.clone(),
            options: permission.options.clone(),
        };
        let mut state = self.state.lock().await;
        if state.snapshot.turn == TurnState::Cancelling {
            state.append(
                SessionEventKind::PermissionResolved {
                    permission_id: snapshot.permission_id,
                    option_id: None,
                    automatic: true,
                },
                false,
            );
            permission
                .resolve(agent_client_protocol::schema::v1::RequestPermissionOutcome::Cancelled);
            return;
        }
        if matches!(
            state.snapshot.process,
            ProcessState::Stopping
                | ProcessState::Stopped
                | ProcessState::Failed
                | ProcessState::Interrupted
        ) {
            let outcome = permission_prompt(&snapshot).unanswered();
            let selected = selected_option(&outcome);
            state.append(
                SessionEventKind::PermissionResolved {
                    permission_id: snapshot.permission_id,
                    option_id: selected,
                    automatic: true,
                },
                false,
            );
            permission.resolve(outcome);
            return;
        }
        if let Some(decision) = self.permission_policy.decide(&snapshot) {
            let prompt = permission_prompt(&snapshot);
            let (_, outcome) = prompt.answer(decision);
            let selected = selected_option(&outcome);
            state.append(
                SessionEventKind::PermissionResolved {
                    permission_id: snapshot.permission_id,
                    option_id: selected,
                    automatic: true,
                },
                false,
            );
            permission.resolve(outcome);
            return;
        }
        self.pending_permissions
            .lock()
            .await
            .insert(permission.request_id.clone(), permission);
        state.snapshot.pending_permissions.push(snapshot.clone());
        state.snapshot.attention = AttentionState::Permission;
        state.snapshot.activity = snapshot
            .tool_call
            .fields
            .title
            .clone()
            .unwrap_or_else(|| "Permission needed".to_string());
        state.append(
            SessionEventKind::Permission {
                permission: snapshot,
            },
            true,
        );
    }

    async fn submit_prompt(self: &Arc<Self>, mut text: String) -> Result<()> {
        anyhow::ensure!(!text.trim().is_empty(), "prompt must not be empty");
        let (client, session_id) = self.client_and_session().await?;
        let original = text.clone();
        let generation = {
            let mut state = self.state.lock().await;
            anyhow::ensure!(
                state.snapshot.turn == TurnState::Idle,
                "a turn is already active"
            );
            anyhow::ensure!(
                state.snapshot.process == ProcessState::Running,
                "supervised controller is not running"
            );
            if let Some(schema) = &self.result_schema {
                text.push_str(&result_schema_instruction(schema)?);
            }
            state.turn_generation = state.turn_generation.saturating_add(1);
            let generation = state.turn_generation;
            state.snapshot.turn = TurnState::Submitting;
            state.snapshot.attention = AttentionState::None;
            state.snapshot.review = ReviewState::Reviewed;
            state.snapshot.failure = None;
            state.snapshot.activity = "Submitting".to_string();
            state.current_reply.clear();
            state.append(SessionEventKind::UserPrompt { text: original }, false);
            state.append(
                SessionEventKind::Turn {
                    turn: TurnState::Submitting,
                },
                false,
            );
            state.snapshot.turn = TurnState::Working;
            state.snapshot.activity = "Working".to_string();
            state.append(
                SessionEventKind::Turn {
                    turn: TurnState::Working,
                },
                false,
            );
            generation
        };
        let sender = self
            .session_commands
            .lock()
            .await
            .clone()
            .context("supervised session actor is not live")?;
        if sender
            .send(SessionActorCommand::Prompt(PromptActorInput {
                client: Box::new(client),
                session_id,
                text,
                generation,
                deadline: self
                    .turn_timeout
                    .map(|timeout| tokio::time::Instant::now() + timeout),
            }))
            .is_err()
        {
            self.fail("supervised session actor stopped unexpectedly".to_string())
                .await;
            if let Err(error) = self.stop().await {
                tracing::warn!(%error, "failed session actor cleanup did not confirm");
            }
            anyhow::bail!("supervised session actor stopped unexpectedly");
        }
        Ok(())
    }

    async fn settle_turn(&self, generation: u64, stop_reason: String) {
        let _gate = self.mutation_gate.lock().await;
        {
            let state = self.state.lock().await;
            if state.turn_generation != generation
                || !matches!(
                    state.snapshot.turn,
                    TurnState::Submitting | TurnState::Working | TurnState::Cancelling
                )
            {
                return;
            }
        }
        self.abandon_permissions(false).await;
        let (result, schema_ok, schema_error) = {
            let state = self.state.lock().await;
            if state.turn_generation != generation
                || !matches!(
                    state.snapshot.turn,
                    TurnState::Submitting | TurnState::Working | TurnState::Cancelling
                )
            {
                return;
            }
            if state.snapshot.turn == TurnState::Cancelling {
                (None, None, None)
            } else {
                match &self.result_schema {
                    Some(schema) => match validate_result(schema, &state.current_reply) {
                        Ok(value) => (Some(value), Some(true), None),
                        Err(error) => (None, Some(false), Some(error)),
                    },
                    None => (None, None, None),
                }
            }
        };
        let mut state = self.state.lock().await;
        if state.turn_generation != generation
            || !matches!(
                state.snapshot.turn,
                TurnState::Submitting | TurnState::Working | TurnState::Cancelling
            )
        {
            return;
        }
        let was_cancelling = state.snapshot.turn == TurnState::Cancelling;
        state.snapshot.turn = TurnState::Idle;
        state.snapshot.review = if was_cancelling {
            ReviewState::Reviewed
        } else {
            ReviewState::Unread
        };
        if let Some(message) = schema_error {
            state.snapshot.attention = AttentionState::Error;
            state.snapshot.activity = message.clone();
            state.snapshot.failure = Some(message.clone());
            state.append(SessionEventKind::TurnFailed { message }, true);
        } else {
            state.snapshot.attention = if was_cancelling {
                AttentionState::None
            } else {
                AttentionState::Result
            };
            state.snapshot.activity = if was_cancelling {
                "Cancelled".to_string()
            } else {
                "Ready for review".to_string()
            };
            let stop_reason = if was_cancelling {
                "cancelled".to_string()
            } else {
                stop_reason
            };
            state.append(
                SessionEventKind::TurnSettled {
                    result: TurnResult {
                        stop_reason,
                        result,
                        schema_ok,
                    },
                },
                true,
            );
        }
        drop(state);
        if let Err(error) = self.persist().await {
            tracing::warn!(%error, "failed to persist settled supervised run");
        }
    }

    async fn fail_turn_if_current(&self, generation: u64, message: String) {
        let _gate = self.mutation_gate.lock().await;
        {
            let state = self.state.lock().await;
            if state.turn_generation != generation
                || !matches!(
                    state.snapshot.turn,
                    TurnState::Submitting | TurnState::Working | TurnState::Cancelling
                )
            {
                return;
            }
        }
        self.abandon_permissions(false).await;
        let mut state = self.state.lock().await;
        if state.turn_generation != generation
            || !matches!(
                state.snapshot.turn,
                TurnState::Submitting | TurnState::Working | TurnState::Cancelling
            )
        {
            return;
        }
        state.snapshot.turn = TurnState::Idle;
        state.snapshot.attention = AttentionState::Error;
        state.snapshot.review = ReviewState::Unread;
        state.snapshot.activity = message.clone();
        state.snapshot.failure = Some(message.clone());
        state.append(SessionEventKind::TurnFailed { message }, true);
        drop(state);
        if let Err(error) = self.persist().await {
            tracing::warn!(%error, "failed to persist supervised turn failure");
        }
    }

    async fn mark_cancelling_if_current(&self, generation: u64) {
        let mut state = self.state.lock().await;
        if state.turn_generation == generation
            && matches!(
                state.snapshot.turn,
                TurnState::Submitting | TurnState::Working
            )
        {
            state.snapshot.turn = TurnState::Cancelling;
            state.snapshot.activity = "Cancelling".to_string();
            state.append(
                SessionEventKind::Turn {
                    turn: TurnState::Cancelling,
                },
                false,
            );
        }
    }

    async fn is_turn_cancelling(&self, generation: u64) -> bool {
        let state = self.state.lock().await;
        state.turn_generation == generation && state.snapshot.turn == TurnState::Cancelling
    }

    async fn fail_if_current(&self, generation: u64, message: String) {
        let _gate = self.mutation_gate.lock().await;
        let current = {
            let state = self.state.lock().await;
            state.turn_generation == generation && state.snapshot.turn == TurnState::Cancelling
        };
        if current {
            self.fail(message).await;
            if let Err(error) = self.stop().await {
                tracing::warn!(%error, "failed supervised turn cleanup did not confirm");
            }
        }
    }

    async fn cancel(self: &Arc<Self>) -> Result<()> {
        let (client, session_id) = self.client_and_session().await?;
        let generation = {
            let mut state = self.state.lock().await;
            anyhow::ensure!(
                matches!(
                    state.snapshot.turn,
                    TurnState::Submitting | TurnState::Working
                ),
                "there is no active turn to cancel"
            );
            state.snapshot.turn = TurnState::Cancelling;
            state.snapshot.activity = "Cancelling".to_string();
            state.append(
                SessionEventKind::Turn {
                    turn: TurnState::Cancelling,
                },
                false,
            );
            state.turn_generation
        };
        self.abandon_permissions(true).await;
        client.cancel(&session_id).await?;
        let run = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(bitrouter_sdk::acp::client::AcpClient::cancellation_grace()).await;
            run.fail_if_current(
                generation,
                "cancelled turn did not settle before the controller grace deadline".to_string(),
            )
            .await;
        });
        Ok(())
    }

    async fn abandon_permissions(&self, cancelled: bool) {
        let permissions = std::mem::take(&mut *self.pending_permissions.lock().await);
        let mut resolutions = Vec::with_capacity(permissions.len());
        for (permission_id, permission) in permissions {
            let snapshot = PendingPermissionSnapshot {
                permission_id: permission_id.clone(),
                tool_call: permission.tool_call.clone(),
                options: permission.options.clone(),
            };
            let outcome = if cancelled {
                agent_client_protocol::schema::v1::RequestPermissionOutcome::Cancelled
            } else {
                permission_prompt(&snapshot).unanswered()
            };
            let option_id = selected_option(&outcome);
            resolutions.push((permission_id, option_id, permission, outcome));
        }
        let mut state = self.state.lock().await;
        state.snapshot.pending_permissions.clear();
        if state.snapshot.attention == AttentionState::Permission {
            state.snapshot.attention = AttentionState::None;
        }
        for entry in &mut state.journal {
            if matches!(&entry.event.kind, SessionEventKind::Permission { .. }) {
                entry.pinned = false;
            }
        }
        for (permission_id, option_id, _, _) in &resolutions {
            state.append(
                SessionEventKind::PermissionResolved {
                    permission_id: permission_id.clone(),
                    option_id: option_id.clone(),
                    automatic: false,
                },
                false,
            );
        }
        drop(state);
        for (_, _, permission, outcome) in resolutions {
            permission.resolve(outcome);
        }
    }

    async fn resolve_permission(
        &self,
        permission_id: &str,
        option_id: &str,
        automatic: bool,
    ) -> Result<()> {
        let mut permissions = self.pending_permissions.lock().await;
        let selected = permissions
            .get(permission_id)
            .with_context(|| format!("permission '{permission_id}' is not pending"))?
            .options
            .iter()
            .find(|option| option.option_id.0.as_ref() == option_id)
            .map(|option| option.option_id.clone())
            .with_context(|| format!("permission option '{option_id}' was not offered"))?;
        let permission = permissions
            .remove(permission_id)
            .with_context(|| format!("permission '{permission_id}' is not pending"))?;
        drop(permissions);
        let mut state = self.state.lock().await;
        state
            .snapshot
            .pending_permissions
            .retain(|pending| pending.permission_id != permission_id);
        if state.snapshot.pending_permissions.is_empty() {
            state.snapshot.attention = AttentionState::None;
            state.snapshot.activity = if state.snapshot.turn == TurnState::Idle {
                "Idle".to_string()
            } else {
                "Working".to_string()
            };
        }
        for entry in &mut state.journal {
            if matches!(
                &entry.event.kind,
                SessionEventKind::Permission { permission }
                    if permission.permission_id == permission_id
            ) {
                entry.pinned = false;
            }
        }
        state.append(
            SessionEventKind::PermissionResolved {
                permission_id: permission_id.to_string(),
                option_id: Some(option_id.to_string()),
                automatic,
            },
            false,
        );
        drop(state);
        permission.resolve(
            agent_client_protocol::schema::v1::RequestPermissionOutcome::Selected(
                agent_client_protocol::schema::v1::SelectedPermissionOutcome::new(selected),
            ),
        );
        Ok(())
    }

    async fn select_session(self: &Arc<Self>, selection: SessionSelection) -> Result<()> {
        {
            let state = self.state.lock().await;
            anyhow::ensure!(state.snapshot.turn == TurnState::Idle, "a turn is active");
            anyhow::ensure!(
                state.snapshot.process == ProcessState::Running,
                "supervised controller is not running"
            );
        }
        let (updates, native_session_id, agent_session_id, initial_settings) = {
            let mut handle = self.handle.lock().await;
            let handle = handle
                .as_mut()
                .context("supervised controller is not live")?;
            handle
                .select_with_cancel(&selection, &tokio_util::sync::CancellationToken::new())
                .await?;
            (
                handle.take_sequenced_updates(),
                handle.session_id.clone(),
                handle.agent_session_id.clone(),
                handle.initial_settings.clone(),
            )
        };
        {
            let mut state = self.state.lock().await;
            state.snapshot.native_session_id = Some(native_session_id.clone());
            state.snapshot.agent_session_id = agent_session_id.clone();
            state.snapshot.initial_settings = Some(initial_settings.clone());
            state.snapshot.attention = AttentionState::None;
            state.snapshot.review = ReviewState::Reviewed;
            state.snapshot.failure = None;
            state.append(
                SessionEventKind::NativeSessionSelected {
                    native_session_id,
                    agent_session_id,
                    initial_settings,
                },
                false,
            );
        }
        let sender = self
            .session_commands
            .lock()
            .await
            .clone()
            .context("supervised session actor is not live")?;
        let (acknowledgement, received) = oneshot::channel();
        if sender
            .send(SessionActorCommand::ReplaceUpdates {
                updates,
                acknowledgement,
            })
            .is_err()
        {
            self.fail("supervised session actor stopped unexpectedly".to_string())
                .await;
            let _ = self.stop().await;
            anyhow::bail!("supervised session actor stopped unexpectedly");
        }
        if received.await.is_err() {
            self.fail(
                "supervised session actor stopped before replacing its update stream".to_string(),
            )
            .await;
            let _ = self.stop().await;
            anyhow::bail!("supervised session actor stopped before replacing its update stream");
        }
        self.persist().await
    }

    async fn stop(&self) -> Result<()> {
        let mut prior = self.state.lock().await.snapshot.process;
        if prior == ProcessState::Stopped {
            return Ok(());
        }
        let startup_pending = self.startup_pending().await;
        {
            let mut state = self.state.lock().await;
            state.snapshot.process = ProcessState::Stopping;
            state.snapshot.activity = "Stopping".to_string();
            state.append(
                SessionEventKind::Lifecycle {
                    process: ProcessState::Stopping,
                },
                false,
            );
        }
        self.startup_cancel.cancel();
        let startup_confirmed = if startup_pending {
            let teardown_confirmed = self.wait_for_startup().await;
            let after_startup = self.state.lock().await.snapshot.process;
            if after_startup == ProcessState::Failed {
                prior = ProcessState::Failed;
            }
            if !teardown_confirmed {
                if after_startup != ProcessState::Failed {
                    self.fail("cancelled startup cleanup did not confirm".to_string())
                        .await;
                }
                self.persist().await?;
                anyhow::bail!("cancelled startup cleanup did not confirm");
            }
            true
        } else {
            self.startup_teardown_confirmed().await.unwrap_or(false)
        };
        if !startup_confirmed && self.handle.lock().await.is_none() {
            self.fail("startup cleanup did not confirm".to_string())
                .await;
            self.persist().await?;
            anyhow::bail!("startup cleanup did not confirm");
        }
        self.abandon_permissions(false).await;
        let mut handle = self.handle.lock().await;
        let clean = match handle.as_mut() {
            Some(live) => live.shutdown().await,
            None => true,
        };
        if clean {
            *handle = None;
            *self.session_commands.lock().await = None;
            if !startup_confirmed {
                self.confirm_startup_teardown().await;
            }
        }
        drop(handle);
        let mut state = self.state.lock().await;
        state.snapshot.turn = TurnState::Idle;
        state.snapshot.pending_permissions.clear();
        state.snapshot.lease = None;
        state.snapshot.attachment = AttachmentState::Detached;
        if !clean {
            state.snapshot.process = ProcessState::Failed;
            state.snapshot.attention = AttentionState::Error;
            state.snapshot.review = ReviewState::Unread;
            state.snapshot.activity = "controller cleanup did not confirm".to_string();
            state.snapshot.failure = Some("controller cleanup did not confirm".to_string());
            state.append(
                SessionEventKind::TurnFailed {
                    message: "controller cleanup did not confirm".to_string(),
                },
                true,
            );
            drop(state);
            self.persist().await?;
            anyhow::bail!("controller cleanup did not confirm");
        }
        if prior != ProcessState::Failed {
            state.snapshot.process = ProcessState::Stopped;
            state.snapshot.attention = AttentionState::None;
            state.snapshot.review = ReviewState::Reviewed;
            state.snapshot.activity = "Stopped".to_string();
            state.append(
                SessionEventKind::Lifecycle {
                    process: ProcessState::Stopped,
                },
                false,
            );
        } else {
            state.snapshot.process = ProcessState::Failed;
            state.snapshot.activity = state
                .snapshot
                .failure
                .clone()
                .unwrap_or_else(|| "Failed".to_string());
        }
        drop(state);
        self.persist().await?;
        self.release_claim().await;
        Ok(())
    }

    async fn mark_reviewed(&self) {
        let mut state = self.state.lock().await;
        state.snapshot.review = ReviewState::Reviewed;
        if matches!(
            state.snapshot.attention,
            AttentionState::Result | AttentionState::Error
        ) {
            state.snapshot.attention = AttentionState::None;
        }
        for entry in &mut state.journal {
            if matches!(
                &entry.event.kind,
                SessionEventKind::TurnSettled { .. } | SessionEventKind::TurnFailed { .. }
            ) {
                entry.pinned = false;
            }
        }
        state.append(
            SessionEventKind::Review {
                review: ReviewState::Reviewed,
            },
            false,
        );
    }

    async fn acquire_lease(
        &self,
        client_id: &str,
        mode: LeaseMode,
        takeover: bool,
    ) -> Result<ControlLease> {
        validate_identifier("client", client_id)?;
        self.expire_lease().await;
        let mut state = self.state.lock().await;
        let lease = match &state.snapshot.lease {
            Some(lease) if lease.owner_client_id == client_id => ControlLease {
                owner_client_id: client_id.to_string(),
                generation: lease.generation,
                mode: if mode == LeaseMode::Attached {
                    LeaseMode::Attached
                } else {
                    lease.mode
                },
                expires_at: lease_expiry(),
            },
            Some(lease) if !takeover => {
                anyhow::bail!(
                    "run is controlled by client '{}' at generation {}",
                    lease.owner_client_id,
                    lease.generation
                );
            }
            _ => {
                state.lease_generation = state.lease_generation.saturating_add(1);
                ControlLease {
                    owner_client_id: client_id.to_string(),
                    generation: state.lease_generation,
                    mode,
                    expires_at: lease_expiry(),
                }
            }
        };
        state.snapshot.lease = Some(lease.clone());
        state.snapshot.attachment = if lease.mode == LeaseMode::Attached {
            AttachmentState::Controlled {
                client_id: client_id.to_string(),
            }
        } else {
            AttachmentState::Observed
        };
        state.append(
            SessionEventKind::Lease {
                lease: Some(lease.clone()),
            },
            false,
        );
        Ok(lease)
    }

    async fn heartbeat(&self, client_id: &str, generation: u64) -> Result<ControlLease> {
        self.expire_lease().await;
        let mut state = self.state.lock().await;
        let lease = state
            .snapshot
            .lease
            .as_mut()
            .context("control lease is not held")?;
        anyhow::ensure!(
            lease.owner_client_id == client_id && lease.generation == generation,
            "control lease was lost or taken over"
        );
        lease.expires_at = lease_expiry();
        Ok(lease.clone())
    }

    async fn verify_fence(&self, fence: &ControlFence) -> Result<()> {
        validate_identifier("action request", &fence.action_request_id)?;
        self.expire_lease().await;
        let state = self.state.lock().await;
        let lease = state
            .snapshot
            .lease
            .as_ref()
            .context("control lease is not held")?;
        anyhow::ensure!(
            lease.owner_client_id == fence.client_id && lease.generation == fence.lease_generation,
            "stale control lease generation"
        );
        Ok(())
    }

    async fn release_lease(&self, client_id: &str, generation: u64) -> Result<()> {
        let mut state = self.state.lock().await;
        let lease = state
            .snapshot
            .lease
            .as_ref()
            .context("control lease is not held")?;
        anyhow::ensure!(
            lease.owner_client_id == client_id && lease.generation == generation,
            "stale control lease generation"
        );
        state.snapshot.lease = None;
        state.snapshot.attachment = AttachmentState::Detached;
        state.append(SessionEventKind::Lease { lease: None }, false);
        Ok(())
    }

    async fn release_if_transient(&self, client_id: &str, generation: u64) {
        let transient = self
            .state
            .lock()
            .await
            .snapshot
            .lease
            .as_ref()
            .is_some_and(|lease| {
                lease.owner_client_id == client_id
                    && lease.generation == generation
                    && lease.mode == LeaseMode::Transient
            });
        if transient {
            let _ = self.release_lease(client_id, generation).await;
        }
    }

    async fn expire_lease(&self) {
        let mut state = self.state.lock().await;
        if state
            .snapshot
            .lease
            .as_ref()
            .is_some_and(|lease| lease.expires_at <= Utc::now())
        {
            state.snapshot.lease = None;
            state.snapshot.attachment = AttachmentState::Detached;
            state.append(SessionEventKind::Lease { lease: None }, false);
        }
    }

    async fn cached_action(
        &self,
        action_request_id: &str,
        client_id: &str,
        expected_generation: Option<u64>,
        fingerprint: &str,
    ) -> Result<Option<ActionAcknowledgement>> {
        validate_identifier("action request", action_request_id)?;
        let state = self.state.lock().await;
        let Some(record) = state.actions.get(action_request_id) else {
            return Ok(None);
        };
        anyhow::ensure!(
            record.owner_client_id == client_id && record.fingerprint == fingerprint,
            "action request id was already used for a different action"
        );
        let generation = expected_generation.unwrap_or(record.lease_generation);
        anyhow::ensure!(
            record.lease_generation == generation && state.lease_generation == generation,
            "stale control lease generation"
        );
        if expected_generation.is_none() {
            let lease = state
                .snapshot
                .lease
                .as_ref()
                .context("cached lease action is no longer current")?;
            anyhow::ensure!(
                lease.owner_client_id == client_id && lease.generation == generation,
                "cached lease action is no longer current"
            );
        }
        Ok(Some(record.acknowledgement.clone()))
    }

    async fn remember_action(
        &self,
        action_request_id: &str,
        client_id: &str,
        lease_generation: u64,
        fingerprint: String,
        value: Option<ActionValue>,
    ) -> ActionAcknowledgement {
        let mut state = self.state.lock().await;
        let acknowledgement = ActionAcknowledgement {
            action_request_id: action_request_id.to_string(),
            snapshot: state.snapshot.clone(),
            value,
        };
        state.actions.insert(
            action_request_id.to_string(),
            ActionRecord {
                owner_client_id: client_id.to_string(),
                lease_generation,
                fingerprint,
                acknowledgement: acknowledgement.clone(),
            },
        );
        acknowledgement
    }

    async fn attachment(&self, after_seq: Option<u64>) -> RunAttachment {
        let state = self.state.lock().await;
        RunAttachment {
            snapshot: state.snapshot.clone(),
            replay: Self::replay_from_state(&state, after_seq),
        }
    }

    async fn replay(&self, after_seq: Option<u64>) -> ReplayBatch {
        let state = self.state.lock().await;
        Self::replay_from_state(&state, after_seq)
    }

    fn replay_from_state(state: &RunState, after_seq: Option<u64>) -> ReplayBatch {
        let first_retained_seq = state
            .journal
            .front()
            .map_or(state.snapshot.last_seq.saturating_add(1), |entry| {
                entry.event.seq
            });
        let requested = after_seq.map_or(0, |seq| seq.saturating_add(1));
        let gap = requested != 0 && requested < first_retained_seq;
        ReplayBatch {
            first_retained_seq,
            snapshot_seq: state.snapshot.last_seq,
            history_complete: state.history_complete && !gap,
            events: state
                .journal
                .iter()
                .filter(|entry| after_seq.is_none_or(|seq| entry.event.seq > seq))
                .map(|entry| entry.event.clone())
                .collect(),
        }
    }
}

impl RunState {
    fn append(&mut self, kind: SessionEventKind, pinned: bool) {
        let event = SessionEvent {
            seq: self.next_seq,
            at: Utc::now(),
            kind,
        };
        self.next_seq = self.next_seq.saturating_add(1);
        self.snapshot.last_seq = event.seq;
        self.snapshot.updated_at = event.at;
        self.journal.push_back(JournalEntry { event, pinned });
        while self.journal.len() > JOURNAL_EVENT_LIMIT {
            let removable = self.journal.iter().position(|entry| !entry.pinned);
            let Some(index) = removable else {
                break;
            };
            self.journal.remove(index);
            self.history_complete = false;
        }
    }
}

impl PermissionPolicy {
    fn decide(
        &self,
        permission: &PendingPermissionSnapshot,
    ) -> Option<bitrouter_tui::permission::Decision> {
        let prompt = permission_prompt(permission);
        let title = prompt.title().trim();
        let head = title.split_whitespace().next().unwrap_or_default();
        let kind = prompt
            .kind()
            .and_then(|kind| serde_json::to_value(kind).ok())
            .and_then(|value| value.as_str().map(ToOwned::to_owned));
        let matches = |patterns: &[String]| {
            patterns.iter().any(|pattern| {
                title.eq_ignore_ascii_case(pattern)
                    || head.eq_ignore_ascii_case(pattern)
                    || kind
                        .as_deref()
                        .is_some_and(|kind| kind.eq_ignore_ascii_case(pattern))
            })
        };
        if matches(&self.auto_deny) {
            return Some(bitrouter_tui::permission::Decision::Deny);
        }
        if matches(&self.auto_approve) {
            return Some(bitrouter_tui::permission::Decision::Approve);
        }
        match self.unmatched {
            PermissionDefault::Ask => None,
            PermissionDefault::ApproveAll => Some(bitrouter_tui::permission::Decision::Approve),
            PermissionDefault::ApproveReads => match prompt.kind() {
                Some(
                    agent_client_protocol::schema::v1::ToolKind::Read
                    | agent_client_protocol::schema::v1::ToolKind::Search,
                ) => Some(bitrouter_tui::permission::Decision::Approve),
                _ => Some(bitrouter_tui::permission::Decision::Deny),
            },
            PermissionDefault::DenyAll => Some(bitrouter_tui::permission::Decision::Deny),
        }
    }
}

fn permission_prompt(permission: &PendingPermissionSnapshot) -> bitrouter_tui::permission::Prompt {
    bitrouter_tui::permission::Prompt::new(
        permission.permission_id.clone(),
        permission.tool_call.fields.title.clone(),
        permission.tool_call.tool_call_id.0.to_string(),
        permission.tool_call.fields.kind,
        permission.options.clone(),
    )
}

fn selected_option(
    outcome: &agent_client_protocol::schema::v1::RequestPermissionOutcome,
) -> Option<String> {
    match outcome {
        agent_client_protocol::schema::v1::RequestPermissionOutcome::Selected(selected) => {
            Some(selected.option_id.0.to_string())
        }
        agent_client_protocol::schema::v1::RequestPermissionOutcome::Cancelled => None,
        _ => None,
    }
}

fn validate_start(request: &StartRunRequest) -> Result<()> {
    validate_identifier("agent", &request.agent_id)?;
    if let Some(label) = &request.label {
        validate_identifier("label", label)?;
    }
    if let Some(client) = &request.client_id {
        validate_identifier("client", client)?;
    }
    if let Some(parent) = &request.parent_run_id {
        validate_identifier("parent run", parent)?;
    }
    Ok(())
}

fn validate_identifier(kind: &str, value: &str) -> Result<()> {
    anyhow::ensure!(
        !value.trim().is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
        "{kind} id must contain 1–256 bytes and no control characters"
    );
    Ok(())
}

fn action_fingerprint<T: Serialize>(scope: &str, value: &T) -> Result<String> {
    let payload = serde_json::to_string(value).context("fingerprinting supervised action")?;
    Ok(format!("{scope}:{payload}"))
}

fn lease_expiry() -> DateTime<Utc> {
    Utc::now()
        + chrono::Duration::from_std(LEASE_TTL).unwrap_or_else(|_| chrono::Duration::seconds(30))
}

async fn claim_key(cwd: &Path) -> Result<PathBuf> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .await;
    if let Ok(output) = output
        && output.status.success()
    {
        let root = String::from_utf8(output.stdout).context("git returned a non-UTF-8 worktree")?;
        return tokio::fs::canonicalize(root.trim())
            .await
            .context("canonicalising Git worktree root");
    }
    Ok(cwd.to_path_buf())
}

fn result_schema_instruction(schema: &serde_json::Value) -> Result<String> {
    Ok(format!(
        "\n\nWhen you are done, end your reply with your final result as a single JSON object inside a ```json fenced code block. It must match this JSON Schema:\n```json\n{}\n```",
        serde_json::to_string_pretty(schema).context("serialising result schema")?
    ))
}

fn validate_result(
    schema: &serde_json::Value,
    reply: &str,
) -> std::result::Result<serde_json::Value, String> {
    let candidate =
        extract_json(reply).ok_or_else(|| "no JSON result found in the reply".to_string())?;
    let value: serde_json::Value = serde_json::from_str(&candidate)
        .map_err(|error| format!("result block is not valid JSON: {error}"))?;
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| format!("result schema is not valid: {error}"))?;
    let errors = validator
        .iter_errors(&value)
        .map(|error| format!("{}: {error}", error.instance_path()))
        .collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(value)
    } else {
        Err(format!(
            "result schema validation failed: {}",
            errors.join("; ")
        ))
    }
}

fn extract_json(reply: &str) -> Option<String> {
    let mut last = None;
    let mut in_block: Option<String> = None;
    for line in reply.lines() {
        let trimmed = line.trim();
        match &mut in_block {
            None => {
                if trimmed
                    .strip_prefix("```")
                    .is_some_and(|info| info.trim().eq_ignore_ascii_case("json"))
                {
                    in_block = Some(String::new());
                }
            }
            Some(_) if trimmed.starts_with("```") => {
                last = in_block.take();
            }
            Some(buffer) => {
                buffer.push_str(line);
                buffer.push('\n');
            }
        }
    }
    last.or_else(|| {
        let trimmed = reply.trim();
        (trimmed.starts_with('{') && trimmed.ends_with('}')).then(|| trimmed.to_string())
    })
}

fn command_scope_requirement(
    command: &SessionCommand,
) -> (
    &'static [SessionScope],
    &'static [SessionScope],
    Option<&str>,
) {
    match command {
        SessionCommand::Authorize { .. } => (&[], &[], None),
        SessionCommand::Start(request) => {
            (&[SessionScope::Start], &[], request.client_id.as_deref())
        }
        SessionCommand::List => (&[SessionScope::List], &[], None),
        SessionCommand::PeekAll
        | SessionCommand::Snapshot { .. }
        | SessionCommand::NativeList { .. }
        | SessionCommand::RouteList { .. } => (&[SessionScope::Peek], &[], None),
        SessionCommand::Attach { client_id, .. } => (
            &[SessionScope::Attach, SessionScope::Transcript],
            &[],
            Some(client_id),
        ),
        SessionCommand::Events { .. } => (&[SessionScope::Transcript], &[], None),
        SessionCommand::AcquireLease {
            client_id, mode, ..
        } => match mode {
            LeaseMode::Attached => (&[SessionScope::Attach], &[], Some(client_id)),
            LeaseMode::Transient => (
                &[],
                &[SessionScope::Respond, SessionScope::Stop],
                Some(client_id),
            ),
        },
        SessionCommand::Heartbeat { client_id, .. } => (
            &[],
            &[
                SessionScope::Attach,
                SessionScope::Respond,
                SessionScope::Stop,
            ],
            Some(client_id),
        ),
        SessionCommand::ReleaseLease { fence } => (
            &[],
            &[
                SessionScope::Attach,
                SessionScope::Respond,
                SessionScope::Stop,
            ],
            Some(&fence.client_id),
        ),
        SessionCommand::Mutate(mutation) => (
            &[SessionScope::Respond],
            &[],
            Some(&mutation.fence.client_id),
        ),
        SessionCommand::Stop { fence, .. } => (&[SessionScope::Stop], &[], Some(&fence.client_id)),
        SessionCommand::Remove { .. } => (&[SessionScope::Remove], &[], None),
    }
}

/// Request one exact set of capabilities over the owner-scoped local
/// transport. Starting a daemon is deliberately the caller's responsibility.
pub async fn authorize(
    socket_path: &Path,
    client_id: &str,
    scopes: BTreeSet<SessionScope>,
) -> Result<SessionGrant> {
    let requested_scopes = scopes.clone();
    match send_session_request(
        socket_path,
        SessionRequest::authorize(client_id.to_string(), scopes),
    )
    .await?
    {
        SessionResponse::Authorized { grant } => {
            anyhow::ensure!(
                grant.client_id == client_id && grant.scopes == requested_scopes,
                "daemon returned a grant for a different client or scope set"
            );
            Ok(grant)
        }
        _ => Err(anyhow::anyhow!(
            "daemon returned an unexpected grant response variant"
        )),
    }
}

/// Send one authorized session command over the daemon's owner-scoped local
/// transport. This helper never adds scopes or renews a failed grant.
pub async fn request(
    socket_path: &Path,
    grant: &SessionGrant,
    command: SessionCommand,
) -> Result<SessionResponse> {
    send_session_request(socket_path, SessionRequest::authorized(grant, command)).await
}

async fn send_session_request(
    socket_path: &Path,
    request: SessionRequest,
) -> Result<SessionResponse> {
    match crate::daemon::send_command(
        socket_path,
        &crate::daemon::DaemonCommand::Sessions { request },
    )
    .await?
    {
        crate::daemon::DaemonResponse::Sessions { response } => Ok(response),
        crate::daemon::DaemonResponse::Error { message } => Err(anyhow::anyhow!(message)),
        _ => Err(anyhow::anyhow!(
            "daemon returned an unexpected session response variant"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{PermissionOptionKind, ToolCallUpdateFields};

    async fn test_run(directory: &Path, owner: &str) -> Result<(tempfile::TempDir, Arc<Run>)> {
        let temporary = tempfile::tempdir_in(directory).context("creating supervisor test dir")?;
        let (ledger, _) = LedgerStore::open(temporary.path().join("ledger.json")).await?;
        let now = Utc::now();
        let snapshot = RunSnapshot {
            run_id: "run-1".to_string(),
            label: "test".to_string(),
            agent_id: "stub".to_string(),
            native_session_id: Some("native-1".to_string()),
            agent_session_id: None,
            via: None,
            cwd: directory.to_path_buf(),
            claim_key: directory.to_path_buf(),
            parent_run_id: None,
            presentation: Presentation::Foreground,
            capabilities: None,
            initial_settings: None,
            process: ProcessState::Running,
            turn: TurnState::Idle,
            attention: AttentionState::None,
            attachment: AttachmentState::Controlled {
                client_id: owner.to_string(),
            },
            review: ReviewState::Reviewed,
            activity: "Idle".to_string(),
            confirmed_route: None,
            attributed_cost: None,
            pending_permissions: Vec::new(),
            failure: None,
            lease: Some(ControlLease {
                owner_client_id: owner.to_string(),
                generation: 1,
                mode: LeaseMode::Attached,
                expires_at: now + chrono::Duration::minutes(5),
            }),
            last_seq: 0,
            started_at: now,
            updated_at: now,
        };
        Ok((
            temporary,
            Arc::new(Run::new(
                snapshot,
                PermissionPolicy::default(),
                None,
                None,
                ledger,
                Arc::new(Mutex::new(HashMap::new())),
                tokio_util::sync::CancellationToken::new(),
            )),
        ))
    }

    fn test_supervisor(directory: &Path, run: Arc<Run>) -> Supervisor {
        Supervisor {
            inner: Arc::new(SupervisorInner {
                source: ConfigSource::Default {
                    home: directory.to_path_buf(),
                },
                routing: Arc::new(ConfigRoutingTable::from_config(Config::default())),
                runs: RwLock::new(HashMap::from([("run-1".to_string(), run.clone())])),
                starts: Mutex::new(HashMap::new()),
                removed: Mutex::new(HashMap::new()),
                grants: Mutex::new(HashMap::new()),
                claims: run.claims.clone(),
                ledger: run.ledger.clone(),
                shutting_down: AtomicBool::new(false),
                shutdown_token: tokio_util::sync::CancellationToken::new(),
            }),
        }
    }

    fn fence(client_id: &str, generation: u64, action_request_id: &str) -> ControlFence {
        ControlFence {
            run_id: "run-1".to_string(),
            client_id: client_id.to_string(),
            lease_generation: generation,
            action_request_id: action_request_id.to_string(),
        }
    }

    async fn test_grant(
        supervisor: &Supervisor,
        client_id: &str,
        scopes: impl IntoIterator<Item = SessionScope>,
    ) -> Result<SessionGrant> {
        let scopes = scopes.into_iter().collect::<BTreeSet<_>>();
        match supervisor
            .dispatch(SessionRequest::authorize(client_id.to_string(), scopes))
            .await?
        {
            SessionResponse::Authorized { grant } => Ok(grant),
            _ => Err(anyhow::anyhow!("grant returned an unexpected response")),
        }
    }

    #[tokio::test]
    async fn takeover_serializes_before_queued_stale_mutation() -> Result<()> {
        let directory = tempfile::tempdir().context("creating work directory")?;
        let (_ledger_directory, run) = test_run(directory.path(), "old-client").await?;
        let held = run.mutation_gate.lock().await;

        let takeover_ready = Arc::new(Notify::new());
        let takeover_run = run.clone();
        let takeover_signal = takeover_ready.clone();
        let takeover = tokio::spawn(async move {
            takeover_signal.notify_one();
            let _gate = takeover_run.mutation_gate.lock().await;
            takeover_run
                .acquire_lease("new-client", LeaseMode::Attached, true)
                .await
        });
        takeover_ready.notified().await;

        let stale_ready = Arc::new(Notify::new());
        let stale_run = run.clone();
        let stale_signal = stale_ready.clone();
        let stale = tokio::spawn(async move {
            stale_signal.notify_one();
            let _gate = stale_run.mutation_gate.lock().await;
            stale_run
                .verify_fence(&ControlFence {
                    run_id: "run-1".to_string(),
                    client_id: "old-client".to_string(),
                    lease_generation: 1,
                    action_request_id: "old-action".to_string(),
                })
                .await
        });
        stale_ready.notified().await;
        drop(held);

        let lease = takeover.await.context("joining takeover task")??;
        anyhow::ensure!(lease.generation == 2, "takeover did not advance generation");
        let stale_result = stale.await.context("joining stale mutation task")?;
        anyhow::ensure!(
            stale_result.is_err(),
            "stale mutation crossed takeover fence"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cached_actions_reject_stale_generations_and_payload_reuse() -> Result<()> {
        let directory = tempfile::tempdir().context("creating work directory")?;
        let (_ledger_directory, run) = test_run(directory.path(), "old-client").await?;
        let fingerprint = action_fingerprint("mutate", &SessionAction::MarkReviewed)?;
        run.remember_action("action-1", "old-client", 1, fingerprint.clone(), None)
            .await;
        anyhow::ensure!(
            run.cached_action("action-1", "old-client", Some(1), &fingerprint)
                .await?
                .is_some(),
            "matching replay was not cached"
        );
        let reused = run
            .cached_action("action-1", "old-client", Some(1), "mutate:different")
            .await;
        anyhow::ensure!(reused.is_err(), "different action reused a cached response");
        run.acquire_lease("new-client", LeaseMode::Attached, true)
            .await?;
        let stale = run
            .cached_action("action-1", "old-client", Some(1), &fingerprint)
            .await;
        anyhow::ensure!(stale.is_err(), "old generation replay survived takeover");
        Ok(())
    }

    #[tokio::test]
    async fn local_capability_grants_are_exact_reused_and_redacted() -> Result<()> {
        let directory = tempfile::tempdir().context("creating work directory")?;
        let (_ledger_directory, run) = test_run(directory.path(), "list-client").await?;
        let supervisor = test_supervisor(directory.path(), run.clone());
        let list_grant = test_grant(&supervisor, "list-client", [SessionScope::List]).await?;
        let reused = test_grant(&supervisor, "list-client", [SessionScope::List]).await?;
        anyhow::ensure!(
            list_grant.authorization == reused.authorization,
            "an identical client and scope set did not reuse its grant"
        );
        let debug = format!("{list_grant:?}");
        anyhow::ensure!(
            debug.contains("[redacted]") && !debug.contains(&list_grant.authorization.0),
            "grant diagnostics disclosed the bearer proof"
        );

        let denied = [
            SessionCommand::Events {
                run_id: "run-1".to_string(),
                after_seq: 0,
            },
            SessionCommand::Attach {
                run_id: "run-1".to_string(),
                client_id: "list-client".to_string(),
                after_seq: None,
                action_request_id: "denied-attach".to_string(),
                takeover: false,
            },
            SessionCommand::Stop {
                fence: fence("list-client", 1, "denied-stop"),
                confirmed: true,
            },
            SessionCommand::Remove {
                run_id: "run-1".to_string(),
                action_request_id: "denied-remove".to_string(),
            },
        ];
        for command in denied {
            let error = supervisor
                .dispatch(SessionRequest::authorized(&list_grant, command))
                .await
                .err()
                .context("list-only grant unexpectedly authorized another operation")?;
            anyhow::ensure!(
                error
                    .to_string()
                    .contains("does not grant the required command scope"),
                "scope rejection returned a different error: {error:#}"
            );
        }

        let transcript_grant =
            test_grant(&supervisor, "list-client", [SessionScope::Transcript]).await?;
        anyhow::ensure!(
            list_grant.authorization != transcript_grant.authorization,
            "different scope sets shared one bearer proof"
        );
        Ok(())
    }

    #[tokio::test]
    async fn list_projection_omits_permission_context_and_failure_text() -> Result<()> {
        let directory = tempfile::tempdir().context("creating work directory")?;
        let (_ledger_directory, run) = test_run(directory.path(), "list-client").await?;
        let mut fields = ToolCallUpdateFields::default();
        fields.title = Some("Read settings".to_string());
        fields.raw_input = Some(serde_json::json!({ "secret": "raw-marker" }));
        {
            let mut state = run.state.lock().await;
            state.snapshot.pending_permissions = vec![PendingPermissionSnapshot {
                permission_id: "permission-identity".to_string(),
                tool_call: ToolCallUpdate::new("tool-identity", fields),
                options: Vec::new(),
            }];
            state.snapshot.failure = Some("failure-marker".to_string());
        }
        let supervisor = test_supervisor(directory.path(), run);
        let grant = test_grant(&supervisor, "list-client", [SessionScope::List]).await?;
        let response = supervisor
            .dispatch(SessionRequest::authorized(&grant, SessionCommand::List))
            .await?;
        let SessionResponse::RunSummaries { runs } = response else {
            anyhow::bail!("list returned an unexpected response")
        };
        let encoded = serde_json::to_string(&runs)?;
        anyhow::ensure!(
            encoded.contains("permission-identity") && encoded.contains("Read settings"),
            "list projection omitted permission identity"
        );
        anyhow::ensure!(
            !encoded.contains("raw-marker")
                && !encoded.contains("failure-marker")
                && !encoded.contains("tool-identity"),
            "list projection disclosed permission context or failure text"
        );
        Ok(())
    }

    #[tokio::test]
    async fn dispatcher_fences_every_control_mutation_and_cached_replay() -> Result<()> {
        let directory = tempfile::tempdir().context("creating work directory")?;
        let (_ledger_directory, run) = test_run(directory.path(), "old-client").await?;
        let supervisor = test_supervisor(directory.path(), run.clone());
        let new_attach = test_grant(&supervisor, "new-client", [SessionScope::Attach]).await?;
        let lease = match supervisor
            .dispatch(SessionRequest::authorized(
                &new_attach,
                SessionCommand::AcquireLease {
                    run_id: "run-1".to_string(),
                    client_id: "new-client".to_string(),
                    mode: LeaseMode::Attached,
                    action_request_id: "takeover-1".to_string(),
                    takeover: true,
                },
            ))
            .await?
        {
            SessionResponse::Lease { lease } => lease,
            _ => anyhow::bail!("takeover returned an unexpected response"),
        };
        anyhow::ensure!(lease.generation == 2, "takeover did not advance generation");
        let baseline = serde_json::to_value(run.snapshot().await)?;
        let old_respond = test_grant(&supervisor, "old-client", [SessionScope::Respond]).await?;

        let stale_actions = [
            SessionAction::Prompt {
                text: "must not submit".to_string(),
            },
            SessionAction::Permission {
                permission_id: "permission-1".to_string(),
                option_id: "allow".to_string(),
            },
            SessionAction::Cancel,
        ];
        for (index, action) in stale_actions.into_iter().enumerate() {
            let error = supervisor
                .dispatch(SessionRequest::authorized(
                    &old_respond,
                    SessionCommand::Mutate(SessionMutation {
                        fence: fence("old-client", 1, &format!("stale-mutation-{index}")),
                        action,
                    }),
                ))
                .await
                .err()
                .context("stale mutation unexpectedly succeeded")?;
            anyhow::ensure!(
                error.to_string() == "stale control lease generation",
                "stale mutation returned a different fence error: {error:#}"
            );
            anyhow::ensure!(
                serde_json::to_value(run.snapshot().await)? == baseline,
                "stale mutation changed supervised state"
            );
        }
        let old_stop = test_grant(&supervisor, "old-client", [SessionScope::Stop]).await?;
        let stop_error = supervisor
            .dispatch(SessionRequest::authorized(
                &old_stop,
                SessionCommand::Stop {
                    fence: fence("old-client", 1, "stale-stop"),
                    confirmed: true,
                },
            ))
            .await
            .err()
            .context("stale stop unexpectedly succeeded")?;
        anyhow::ensure!(
            stop_error.to_string() == "stale control lease generation",
            "stale stop returned a different fence error: {stop_error:#}"
        );
        anyhow::ensure!(
            serde_json::to_value(run.snapshot().await)? == baseline,
            "stale stop changed supervised state"
        );

        let new_respond = test_grant(&supervisor, "new-client", [SessionScope::Respond]).await?;
        let replayed = SessionRequest::authorized(
            &new_respond,
            SessionCommand::Mutate(SessionMutation {
                fence: fence("new-client", 2, "review-1"),
                action: SessionAction::MarkReviewed,
            }),
        );
        let first = supervisor.dispatch(replayed.clone()).await?;
        let second = supervisor.dispatch(replayed.clone()).await?;
        anyhow::ensure!(
            serde_json::to_value(first)? == serde_json::to_value(second)?,
            "idempotent dispatcher replay returned a different acknowledgement"
        );
        let collision = supervisor
            .dispatch(SessionRequest::authorized(
                &new_respond,
                SessionCommand::Mutate(SessionMutation {
                    fence: fence("new-client", 2, "review-1"),
                    action: SessionAction::Prompt {
                        text: "different envelope".to_string(),
                    },
                }),
            ))
            .await
            .err()
            .context("action id collision unexpectedly succeeded")?;
        anyhow::ensure!(
            collision.to_string() == "action request id was already used for a different action",
            "action id collision returned a different error: {collision:#}"
        );

        let third_attach = test_grant(&supervisor, "third-client", [SessionScope::Attach]).await?;
        supervisor
            .dispatch(SessionRequest::authorized(
                &third_attach,
                SessionCommand::AcquireLease {
                    run_id: "run-1".to_string(),
                    client_id: "third-client".to_string(),
                    mode: LeaseMode::Attached,
                    action_request_id: "takeover-2".to_string(),
                    takeover: true,
                },
            ))
            .await?;
        let before_stale_replay = serde_json::to_value(run.snapshot().await)?;
        let stale_replay = supervisor
            .dispatch(replayed)
            .await
            .err()
            .context("old-generation cached replay unexpectedly succeeded")?;
        anyhow::ensure!(
            stale_replay.to_string() == "stale control lease generation",
            "cached replay returned a different fence error: {stale_replay:#}"
        );
        anyhow::ensure!(
            serde_json::to_value(run.snapshot().await)? == before_stale_replay,
            "old-generation cached replay changed supervised state"
        );
        Ok(())
    }

    #[tokio::test]
    async fn stopping_startup_releases_completion_lock_before_waiting() -> Result<()> {
        let directory = tempfile::tempdir().context("creating work directory")?;
        let (_ledger_directory, run) = test_run(directory.path(), "client").await?;
        {
            run.state.lock().await.snapshot.process = ProcessState::Starting;
            *run.startup_completion.lock().await = StartupCompletion::Pending;
            run.claims
                .lock()
                .await
                .insert(directory.path().to_path_buf(), vec!["run-1".to_string()]);
        }
        let lifecycle = run.clone();
        let lifecycle_task = tokio::spawn(async move {
            lifecycle.startup_cancel.cancelled().await;
            lifecycle.finish_startup(true).await;
        });
        tokio::time::timeout(Duration::from_secs(1), run.stop())
            .await
            .context("stop deadlocked waiting for startup completion")??;
        lifecycle_task
            .await
            .context("joining simulated startup lifecycle")?;
        anyhow::ensure!(
            run.snapshot().await.process == ProcessState::Stopped,
            "confirmed cancelled startup did not stop"
        );
        anyhow::ensure!(
            !run.claims.lock().await.contains_key(directory.path()),
            "confirmed cancelled startup retained its directory claim"
        );
        Ok(())
    }

    #[tokio::test]
    async fn unconfirmed_startup_cannot_stop_cleanly_or_be_removed() -> Result<()> {
        let directory = tempfile::tempdir().context("creating work directory")?;
        let (_ledger_directory, run) = test_run(directory.path(), "client").await?;
        {
            let mut state = run.state.lock().await;
            state.snapshot.process = ProcessState::Failed;
            state.snapshot.failure = Some("startup cleanup did not confirm".to_string());
            *run.startup_completion.lock().await = StartupCompletion::Finished {
                teardown_confirmed: false,
            };
            run.claims
                .lock()
                .await
                .insert(directory.path().to_path_buf(), vec!["run-1".to_string()]);
        }
        let supervisor = test_supervisor(directory.path(), run.clone());
        let remove_grant = test_grant(&supervisor, "client", [SessionScope::Remove]).await?;
        let remove_error = supervisor
            .dispatch(SessionRequest::authorized(
                &remove_grant,
                SessionCommand::Remove {
                    run_id: "run-1".to_string(),
                    action_request_id: "remove-unconfirmed".to_string(),
                },
            ))
            .await
            .err()
            .context("unconfirmed startup metadata was removed")?;
        anyhow::ensure!(
            remove_error
                .to_string()
                .contains("startup cleanup was not confirmed"),
            "remove returned the wrong cleanup error: {remove_error:#}"
        );
        let stop_error = run
            .stop()
            .await
            .err()
            .context("unconfirmed startup was reported stopped")?;
        anyhow::ensure!(
            stop_error
                .to_string()
                .contains("startup cleanup did not confirm"),
            "stop returned the wrong cleanup error: {stop_error:#}"
        );
        anyhow::ensure!(
            run.snapshot().await.process == ProcessState::Failed,
            "unconfirmed cleanup rewrote failure state"
        );
        anyhow::ensure!(
            run.claims.lock().await.contains_key(directory.path()),
            "unconfirmed cleanup released its directory claim"
        );
        Ok(())
    }

    #[tokio::test]
    async fn replay_reports_a_bounded_history_gap() -> Result<()> {
        let directory = tempfile::tempdir().context("creating work directory")?;
        let (_ledger_directory, run) = test_run(directory.path(), "client").await?;
        {
            let mut state = run.state.lock().await;
            for index in 0..(JOURNAL_EVENT_LIMIT + 16) {
                state.append(
                    SessionEventKind::Activity {
                        text: format!("event-{index}"),
                    },
                    false,
                );
            }
        }
        let replay = run.replay(Some(0)).await;
        anyhow::ensure!(
            !replay.history_complete,
            "evicted replay claimed completeness"
        );
        anyhow::ensure!(
            replay.first_retained_seq > 1,
            "bounded replay did not advance its first retained sequence"
        );
        anyhow::ensure!(
            replay.events.len() == JOURNAL_EVENT_LIMIT,
            "bounded replay retained the wrong event count"
        );
        Ok(())
    }

    #[tokio::test]
    async fn replay_pins_unresolved_permission_and_unread_result_until_reviewed() -> Result<()> {
        let directory = tempfile::tempdir().context("creating work directory")?;
        let (_ledger_directory, run) = test_run(directory.path(), "client").await?;
        let permission = PendingPermissionSnapshot {
            permission_id: "permission-1".to_string(),
            tool_call: ToolCallUpdate::new("tool-1", ToolCallUpdateFields::default()),
            options: vec![PermissionOption::new(
                "reject-exact",
                "Reject",
                PermissionOptionKind::RejectOnce,
            )],
        };
        {
            let mut state = run.state.lock().await;
            state.snapshot.pending_permissions = vec![permission.clone()];
            state.append(
                SessionEventKind::Permission {
                    permission: permission.clone(),
                },
                true,
            );
            state.snapshot.review = ReviewState::Unread;
            state.snapshot.attention = AttentionState::Result;
            state.append(
                SessionEventKind::TurnSettled {
                    result: TurnResult {
                        stop_reason: "end_turn".to_string(),
                        result: None,
                        schema_ok: None,
                    },
                },
                true,
            );
            for index in 0..(JOURNAL_EVENT_LIMIT + 64) {
                state.append(
                    SessionEventKind::Activity {
                        text: format!("evictable-{index}"),
                    },
                    false,
                );
            }
        }
        let retained = run.replay(None).await;
        anyhow::ensure!(
            retained.events.iter().any(|event| matches!(
                &event.kind,
                SessionEventKind::Permission { permission }
                    if permission.permission_id == "permission-1"
            )),
            "unresolved permission was evicted"
        );
        anyhow::ensure!(
            retained
                .events
                .iter()
                .any(|event| matches!(&event.kind, SessionEventKind::TurnSettled { .. })),
            "unread terminal result was evicted"
        );
        anyhow::ensure!(
            !retained.history_complete,
            "bounded replay did not expose its permanent history gap"
        );

        run.mark_reviewed().await;
        let reviewed = run.replay(None).await;
        anyhow::ensure!(
            reviewed.events.iter().any(|event| matches!(
                &event.kind,
                SessionEventKind::Permission { permission }
                    if permission.permission_id == "permission-1"
            )),
            "marking the result reviewed unpinned an unresolved permission"
        );
        anyhow::ensure!(
            !reviewed
                .events
                .iter()
                .any(|event| matches!(&event.kind, SessionEventKind::TurnSettled { .. })),
            "reviewed terminal result remained pinned beyond the retention bound"
        );
        anyhow::ensure!(
            run.snapshot()
                .await
                .pending_permissions
                .iter()
                .any(|pending| pending.permission_id == "permission-1"),
            "reviewing a result cleared unresolved permission metadata"
        );
        Ok(())
    }

    #[test]
    fn unanswered_permission_selects_the_exact_reject_option() -> Result<()> {
        let permission = PendingPermissionSnapshot {
            permission_id: "permission-1".to_string(),
            tool_call: ToolCallUpdate::new("tool-1", ToolCallUpdateFields::default()),
            options: vec![
                PermissionOption::new("allow-other", "Allow", PermissionOptionKind::AllowOnce),
                PermissionOption::new("reject-exact", "Reject", PermissionOptionKind::RejectOnce),
            ],
        };
        let outcome = permission_prompt(&permission).unanswered();
        anyhow::ensure!(
            selected_option(&outcome).as_deref() == Some("reject-exact"),
            "unanswered teardown did not select the controller's exact reject option"
        );
        Ok(())
    }

    #[tokio::test]
    async fn attachment_snapshot_and_replay_share_one_sequence_boundary() -> Result<()> {
        let directory = tempfile::tempdir().context("creating work directory")?;
        let (_ledger_directory, run) = test_run(directory.path(), "client").await?;
        run.state.lock().await.append(
            SessionEventKind::Activity {
                text: "ordered".to_string(),
            },
            false,
        );
        let attachment = run.attachment(None).await;
        anyhow::ensure!(
            attachment.snapshot.last_seq == attachment.replay.snapshot_seq,
            "attachment metadata and replay used different sequence boundaries"
        );
        anyhow::ensure!(
            attachment.replay.events.last().map(|event| event.seq)
                == Some(attachment.replay.snapshot_seq),
            "attachment replay did not reach its declared snapshot boundary"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_turn_is_canonical_and_old_settlement_cannot_touch_successor() -> Result<()> {
        let directory = tempfile::tempdir().context("creating work directory")?;
        let (_ledger_directory, run) = test_run(directory.path(), "client").await?;
        {
            let mut state = run.state.lock().await;
            state.turn_generation = 1;
            state.snapshot.turn = TurnState::Cancelling;
        }
        run.settle_turn(1, "end_turn".to_string()).await;
        {
            let state = run.state.lock().await;
            let stop_reason = state.journal.iter().rev().find_map(|entry| {
                if let SessionEventKind::TurnSettled { result } = &entry.event.kind {
                    Some(result.stop_reason.as_str())
                } else {
                    None
                }
            });
            anyhow::ensure!(
                stop_reason == Some("cancelled"),
                "explicit cancellation did not publish the canonical stop reason"
            );
        }
        {
            let mut state = run.state.lock().await;
            state.turn_generation = 2;
            state.snapshot.turn = TurnState::Working;
            state.snapshot.activity = "successor".to_string();
            state
                .snapshot
                .pending_permissions
                .push(PendingPermissionSnapshot {
                    permission_id: "successor-permission".to_string(),
                    tool_call: ToolCallUpdate::new(
                        "successor-tool",
                        ToolCallUpdateFields::default(),
                    ),
                    options: Vec::new(),
                });
        }
        run.settle_turn(1, "cancelled".to_string()).await;
        let snapshot = run.snapshot().await;
        anyhow::ensure!(
            snapshot.turn == TurnState::Working
                && snapshot.activity == "successor"
                && snapshot
                    .pending_permissions
                    .iter()
                    .any(|permission| permission.permission_id == "successor-permission"),
            "delayed prior-turn settlement changed the successor"
        );
        Ok(())
    }

    #[tokio::test]
    async fn live_ledger_rows_recover_as_interrupted() -> Result<()> {
        let directory = tempfile::tempdir().context("creating ledger directory")?;
        let ledger_path = directory.path().join("ledger.json");
        let (ledger, _) = LedgerStore::open(ledger_path.clone()).await?;
        let (_run_directory, run) = test_run(directory.path(), "client").await?;
        ledger.record(&run.snapshot().await).await?;
        drop(ledger);

        let (reopened, recovered) = LedgerStore::open(ledger_path).await?;
        let entry = recovered
            .into_iter()
            .find(|entry| entry.run_id == "run-1")
            .context("recovering persisted run")?;
        anyhow::ensure!(
            entry.process == ProcessState::Interrupted,
            "live ledger row was not fenced on restart"
        );
        let recovered_run = Run::from_ledger(entry, reopened, Arc::new(Mutex::new(HashMap::new())));
        anyhow::ensure!(
            !recovered_run.replay(None).await.history_complete,
            "minimal restart ledger masqueraded as a complete transcript"
        );
        Ok(())
    }
}
