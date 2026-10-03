//! Server-owned agent runtime with transactional execution facts. State, execution and observation share one
//! authority; disconnecting an observer never cancels its task.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bitrouter_sdk::App;
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::{Message, ToolResultOutput};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::agent::{
    Agent, AgentConfig, ApprovalRequest, RunChannels, RunEvent, RunInput, RunStatus, ToolMode,
};
use crate::store::{
    CallOrigin, CallRecord, CommitRequest, EffectStatus, ExecutionRecord, ExecutionStore,
    MemoryExecutionStore,
};
use crate::thread::{PermissionProfile, ThreadStatus};
use crate::tools::WorkspaceTools;

pub mod observation;
#[cfg(test)]
mod observation_tests;
mod ownership;
#[cfg(test)]
mod ownership_tests;
#[cfg(all(test, unix))]
mod process_recovery_tests;
mod recovery;
#[cfg(test)]
mod recovery_tests;
pub mod startup;
#[cfg(test)]
mod startup_tests;
mod steering;
#[cfg(test)]
mod steering_tests;
#[cfg(test)]
mod thread_tests;
mod threads;
#[cfg(test)]
mod unification_tests;
mod workspace;

const MAX_EVENT_PAGE: usize = 1000;
const MAX_LIVE_BYTES: usize = 32 * 1024;

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

impl TurnStatus {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationEvidence {
    pub command: String,
    pub exit_status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub timed_out: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnEventPayload {
    SteeringUpdated {
        receipt: crate::thread::SteeringReceipt,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnEvent {
    pub thread_id: String,
    pub server_instance_id: String,
    pub turn_id: String,
    pub seq: u64,
    pub timestamp_ms: u64,
    pub payload: TurnEventPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnSnapshot {
    #[serde(default)]
    pub steering: Vec<crate::thread::SteeringReceipt>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveActivity {
    #[serde(default)]
    pub item_id: Option<String>,
    pub truncated: bool,
    pub kind: String,
    pub text: String,
}

/// Complete approval metadata is available even when its event was evicted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputRequest {
    pub request_id: String,
    pub tool_id: String,
    pub tool_name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeCapabilities {
    #[serde(default)]
    pub startup_discovery: Option<startup::StartupDiscovery>,
    pub server_instance_id: String,
    pub limits: RuntimeLimits,
    #[serde(default)]
    pub execution_ownership: Option<crate::store::OwnerClaim>,
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

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            startup_roots: 1024,
            startup_records: 1_000_000,
            startup_metadata_bytes: 4 * 1024 * 1024,
            recovery_readers: 2,
            recovery_page_records: 64,
            recovery_page_bytes: 4 * 1024 * 1024,
            recovery_records_per_thread: 1_000_000,
            history_page_bytes: 2 * 1024 * 1024,
            events_per_thread: 256,
            event_bytes_per_thread: 2 * 1024 * 1024,
            subscribers_per_thread: 8,
            subscriber_bytes_per_thread: 8 * 1024 * 1024,
            steering_inputs_per_turn: 32,
            steering_bytes_per_turn: 64 * 1024,
            hot_threads: 32,
            queued_turns_per_thread: 32,
            context_bytes_per_thread: 2 * 1024 * 1024,
            hot_context_bytes: 64 * 1024 * 1024,
            tools_per_turn: 4,
            global_tools: 16,
            active_turns: 8,
            retained_turns: 32,
            retained_bytes: 64 * 1024 * 1024,
            retention_seconds: 1800,
            subscriber_queue: 32,
            request_bytes: 64 * 1024,
        }
    }
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceError {
    pub code: ErrorCode,
    pub message: String,
}

impl ServiceError {
    fn storage(error: impl ToString) -> Self {
        let message = error.to_string();
        let code = if message.contains("unsupported_runtime_format") {
            ErrorCode::RecoveryRequired
        } else {
            ErrorCode::StorageUnavailable
        };
        Self::new(code, message)
    }

    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ServiceError {}
impl From<String> for ServiceError {
    fn from(message: String) -> Self {
        Self::new(ErrorCode::InvalidRequest, message)
    }
}
impl From<&str> for ServiceError {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

fn unknown_turn() -> ServiceError {
    ServiceError::new(
        ErrorCode::UnknownTurn,
        "Turn is unknown or no longer cached; use the stored Thread query",
    )
}

#[cfg(test)]
struct TurnFixture {
    pub prompt: String,
    pub workspace: PathBuf,
    pub caller: CallerContext,
    pub config: AgentConfig,
    pub verification_command: Option<String>,
    pub idempotency_key: Option<String>,
}

struct PendingInput {
    response: oneshot::Sender<bool>,
}

struct VerificationBudget {
    duration: Duration,
    active_duration_ms: u64,
    calls: u32,
    max_calls: u32,
}

struct TurnRecord {
    fence: Arc<crate::control::LaunchFence>,
    steering: Vec<steering::SteeringInput>,
    verification_budget: Option<(u64, u32)>,
    thread_id: String,
    permission_profile: PermissionProfile,
    settled: Option<(Vec<Message>, u64)>,
    snapshot: TurnSnapshot,
    terminal_at: Option<Instant>,
    cancel: CancellationToken,
    pending: Option<PendingInput>,
    storage_error: Option<String>,
}

struct State {
    startup_discovery: Option<startup::StartupDiscovery>,
    cold_executions: HashMap<String, startup::ColdExecution>,
    workspace_fences: HashMap<PathBuf, Arc<workspace::WorkspaceFence>>,
    execution_ownership: Option<crate::store::OwnerClaim>,
    threads: HashMap<String, threads::ThreadRecord>,
    ready_threads: VecDeque<String>,
    running_turns: HashMap<String, String>,
    workspace_profiles: HashMap<PathBuf, Vec<PermissionProfile>>,
    turns: HashMap<String, TurnRecord>,
    active_workspaces: HashMap<PathBuf, String>,
    allowed_workspaces: Vec<PathBuf>,
    closing: bool,
}

struct Inner {
    queue_waker_started: std::sync::atomic::AtomicBool,
    ownership_init: tokio::sync::Mutex<()>,
    cleanup_unconfirmed: std::sync::atomic::AtomicBool,
    app: Arc<App>,
    instance_id: String,
    limits: RuntimeLimits,
    workers: TaskTracker,
    state: Mutex<State>,
    store: Arc<dyn ExecutionStore>,
    admission: tokio::sync::Mutex<()>,
    tool_workers: Arc<tokio::sync::Semaphore>,
    recovery_readers: tokio::sync::Semaphore,
}

#[derive(Clone)]
pub struct ThreadService {
    inner: Arc<Inner>,
}

struct TurnWorker {
    service: ThreadService,
    turn_id: String,
}
impl Drop for TurnWorker {
    fn drop(&mut self) {
        self.service
            .lock_state()
            .running_turns
            .remove(&self.turn_id);
    }
}

impl ThreadService {
    pub fn new(app: Arc<App>, allowed_workspaces: &[PathBuf]) -> Result<Self, ServiceError> {
        Self::with_store(
            app,
            allowed_workspaces,
            Arc::new(MemoryExecutionStore::default()),
        )
    }

    pub fn with_store(
        app: Arc<App>,
        allowed_workspaces: &[PathBuf],
        store: Arc<dyn ExecutionStore>,
    ) -> Result<Self, ServiceError> {
        Self::with_limits_and_store(app, allowed_workspaces, RuntimeLimits::default(), store)
    }

    #[cfg(test)]
    fn with_limits(
        app: Arc<App>,
        allowed_workspaces: &[PathBuf],
        limits: RuntimeLimits,
    ) -> Result<Self, ServiceError> {
        Self::with_limits_and_store(
            app,
            allowed_workspaces,
            limits,
            Arc::new(MemoryExecutionStore::default()),
        )
    }

    fn with_limits_and_store(
        app: Arc<App>,
        allowed_workspaces: &[PathBuf],
        limits: RuntimeLimits,
        store: Arc<dyn ExecutionStore>,
    ) -> Result<Self, ServiceError> {
        if limits.startup_roots == 0
            || limits.startup_roots > 100_000
            || limits.startup_records == 0
            || limits.startup_records > 10_000_000
            || limits.startup_metadata_bytes == 0
            || limits.startup_metadata_bytes > 64 * 1024 * 1024
            || limits.active_turns == 0
            || limits.recovery_readers == 0
            || !(1..=128).contains(&limits.recovery_page_records)
            || !(1..=4 * 1024 * 1024).contains(&limits.recovery_page_bytes)
            || !(1..=1_000_000).contains(&limits.recovery_records_per_thread)
            || limits.history_page_bytes < 2 * 1024 * 1024
            || limits.history_page_bytes > 4 * 1024 * 1024
            || limits.subscriber_bytes_per_thread < limits.history_page_bytes
            || limits.events_per_thread == 0
            || limits.event_bytes_per_thread == 0
            || limits.subscribers_per_thread == 0
            || limits.steering_inputs_per_turn == 0
            || limits.steering_bytes_per_turn == 0
            || limits.hot_threads == 0
            || limits.queued_turns_per_thread == 0
            || limits.context_bytes_per_thread == 0
            || limits.hot_context_bytes < limits.context_bytes_per_thread
            || limits.tools_per_turn == 0
            || limits.global_tools == 0
            || limits.retained_turns == 0
            || limits.subscriber_queue == 0
        {
            return Err("invalid runtime limits".into());
        }
        let allowed_workspaces = allowed_workspaces
            .iter()
            .map(|path| path.canonicalize().map_err(|error| error.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            inner: Arc::new(Inner {
                queue_waker_started: std::sync::atomic::AtomicBool::new(false),
                ownership_init: tokio::sync::Mutex::new(()),
                cleanup_unconfirmed: std::sync::atomic::AtomicBool::new(false),
                app,
                instance_id: uuid::Uuid::new_v4().to_string(),
                tool_workers: Arc::new(tokio::sync::Semaphore::new(limits.global_tools)),
                recovery_readers: tokio::sync::Semaphore::new(limits.recovery_readers),
                limits,
                workers: TaskTracker::new(),
                store,
                admission: tokio::sync::Mutex::new(()),
                state: Mutex::new(State {
                    startup_discovery: None,
                    cold_executions: HashMap::new(),
                    workspace_fences: HashMap::new(),
                    execution_ownership: None,
                    threads: HashMap::new(),
                    ready_threads: VecDeque::new(),
                    running_turns: HashMap::new(),
                    workspace_profiles: allowed_workspaces
                        .iter()
                        .cloned()
                        .map(|workspace| {
                            (
                                workspace,
                                vec![PermissionProfile::ReadOnly, PermissionProfile::Ask],
                            )
                        })
                        .collect(),
                    turns: HashMap::new(),
                    active_workspaces: HashMap::new(),
                    allowed_workspaces,
                    closing: false,
                }),
            }),
        })
    }

    pub fn capabilities(&self) -> RuntimeCapabilities {
        let state = self.lock_state();
        RuntimeCapabilities {
            startup_discovery: state.startup_discovery.clone(),
            server_instance_id: self.inner.instance_id.clone(),
            limits: self.inner.limits.clone(),
            execution_ownership: state.execution_ownership.clone(),
        }
    }

    pub fn ensure_instance(&self, instance: Option<&str>) -> Result<(), ServiceError> {
        if instance != Some(self.inner.instance_id.as_str()) {
            return Err(ServiceError::new(
                ErrorCode::InstanceChanged,
                "server instance changed or was not specified; do not automatically resubmit",
            ));
        }
        Ok(())
    }

    pub async fn shutdown(&self) {
        {
            let _admission = self.inner.admission.lock().await;
            let mut state = self.lock_state();
            state.closing = true;
            for record in state.turns.values() {
                record.cancel.cancel();
            }
            // Spawn and close are serialized under the same state lock.
            self.inner.workers.close();
        }
        self.inner.workers.wait().await;
        self.pause_queues_after_shutdown().await;
        self.stop_execution_owner().await;
    }

    fn prune(&self, state: &mut State) {
        let mut terminal: Vec<_> = state
            .turns
            .iter()
            .filter_map(|(id, record)| {
                record.terminal_at.map(|at| {
                    let bytes = serde_json::to_vec(&record.snapshot)
                        .map_or(usize::MAX, |encoded| encoded.len());
                    (id.clone(), at, bytes)
                })
            })
            .collect();
        terminal.sort_by_key(|(_, at, _)| *at);
        let excess = terminal
            .len()
            .saturating_sub(self.inner.limits.retained_turns);
        let mut retained_bytes = terminal
            .iter()
            .fold(0_usize, |total, (_, _, bytes)| total.saturating_add(*bytes));
        for (index, (id, at, bytes)) in terminal.into_iter().enumerate() {
            if index < excess
                || retained_bytes > self.inner.limits.retained_bytes
                || at.elapsed() >= Duration::from_secs(self.inner.limits.retention_seconds)
            {
                state.turns.remove(&id);
                retained_bytes = retained_bytes.saturating_sub(bytes);
            }
        }
    }

    /// Register a path offered by an OS-local client after the local transport
    /// has authenticated that client through socket or pipe permissions.
    pub fn register_local_workspace(&self, workspace: &Path) -> Result<PathBuf, ServiceError> {
        let workspace = workspace
            .canonicalize()
            .map_err(|error| error.to_string())?;
        if !workspace.is_dir() {
            return Err("workspace is not a directory".into());
        }
        let mut state = self.lock_state();
        if state.closing {
            return Err(ServiceError::new(
                ErrorCode::ShuttingDown,
                "runtime is shutting down",
            ));
        }
        if state.allowed_workspaces.len() >= 256 && !state.allowed_workspaces.contains(&workspace) {
            return Err(ServiceError::new(
                ErrorCode::Overloaded,
                "workspace registration limit reached",
            ));
        }
        if !state.allowed_workspaces.contains(&workspace) {
            state.workspace_profiles.insert(
                workspace.clone(),
                vec![PermissionProfile::ReadOnly, PermissionProfile::Ask],
            );
            state.allowed_workspaces.push(workspace.clone());
        }
        Ok(workspace)
    }

    // Test fixture: exercise the same two native admission operations as CLI.
    #[cfg(test)]
    async fn submit_fixture(&self, request: TurnFixture) -> Result<TurnSnapshot, ServiceError> {
        use crate::thread::{ThreadRequest, ThreadTarget, TurnRequest};
        let caller = request.caller.clone();
        let key = request
            .idempotency_key
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let permission_profile = if request.config.tool_mode() == ToolMode::ReadOnly {
            PermissionProfile::ReadOnly
        } else {
            PermissionProfile::Ask
        };
        let thread = self
            .create_thread(
                &self.inner.instance_id,
                ThreadRequest {
                    caller: request.caller,
                    workspace: request.workspace,
                    config: request.config,
                    permission_profile,
                    verification_command: request.verification_command,
                    idempotency_key: key.clone(),
                },
            )
            .await?;
        let target = ThreadTarget {
            thread_id: thread.thread_id,
            server_instance_id: thread.server_instance_id,
        };
        let receipt = self
            .start_turn(
                &target,
                &caller,
                TurnRequest {
                    prompt: request.prompt,
                    idempotency_key: key,
                },
            )
            .await?;
        self.read_stored_turn(&target, &caller, &receipt.turn_id)
            .await
    }

    #[cfg(test)]
    fn read(&self, turn_id: &str) -> Result<TurnSnapshot, ServiceError> {
        let mut state = self.lock_state();
        self.prune(&mut state);
        state
            .turns
            .get(turn_id)
            .map(|record| record.snapshot.clone())
            .ok_or_else(unknown_turn)
    }

    #[cfg(test)]
    async fn answer_input(
        &self,
        turn_id: &str,
        request_id: &str,
        approved: bool,
    ) -> Result<(), ServiceError> {
        let gate = self.commit_gate(turn_id)?;
        let _guard = gate.lock().await;
        self.answer_input_serialized(turn_id, request_id, approved, &[])
            .await
    }

    async fn answer_input_serialized(
        &self,
        turn_id: &str,
        request_id: &str,
        approved: bool,
        extra_facts: &[ExecutionRecord],
    ) -> Result<(), ServiceError> {
        {
            let state = self.lock_state();
            let record = state.turns.get(turn_id).ok_or_else(unknown_turn)?;
            if record.snapshot.status != TurnStatus::WaitingForInput
                || record.snapshot.pending_input_id.as_deref() != Some(request_id)
                || record.cancel.is_cancelled()
            {
                return Err(ServiceError::new(
                    ErrorCode::Conflict,
                    "input is not pending for this task",
                ));
            }
        }
        self.append_facts_serialized(
            turn_id,
            TurnEventPayload::InputResolved {
                request_id: request_id.into(),
                approved,
            },
            extra_facts,
        )
        .await?;
        let sender = self
            .lock_state()
            .turns
            .get_mut(turn_id)
            .and_then(|record| record.pending.take())
            .ok_or_else(|| "pending input channel is unavailable".to_string())?
            .response;
        sender.send(approved).map_err(|_| {
            ServiceError::new(ErrorCode::Conflict, "task stopped before accepting input")
        })
    }

    #[cfg(test)]
    async fn cancel(&self, turn_id: &str) -> Result<(), ServiceError> {
        let gate = self.commit_gate(turn_id)?;
        let _guard = gate.lock().await;
        let token = {
            let state = self.lock_state();
            let record = state.turns.get(turn_id).ok_or_else(unknown_turn)?;
            if record.snapshot.status.terminal() || record.cancel.is_cancelled() {
                return Ok(());
            }
            if record.snapshot.status == TurnStatus::Queued {
                return Err(ServiceError::new(
                    ErrorCode::Conflict,
                    "use targeted queued withdrawal for an unstarted Turn",
                ));
            }
            record.cancel.clone()
        };
        self.append_serialized(turn_id, TurnEventPayload::CancelRequested)
            .await?;
        token.cancel();
        let pending = self
            .lock_state()
            .turns
            .get(turn_id)
            .and_then(|record| record.snapshot.pending_input_id.clone());
        if let Some(request_id) = pending {
            self.append_serialized(
                turn_id,
                TurnEventPayload::InputResolved {
                    request_id,
                    approved: false,
                },
            )
            .await?;
            if let Some(pending) = self
                .lock_state()
                .turns
                .get_mut(turn_id)
                .and_then(|record| record.pending.take())
            {
                let _ = pending.response.send(false);
            }
        }
        Ok(())
    }

    fn run_turn(
        &self,
        turn_id: String,
        agent: Agent,
        prompt: RunInput,
        verification_command: Option<String>,
        workspace: PathBuf,
        cancel: CancellationToken,
    ) -> futures::future::BoxFuture<'static, ()> {
        {
            let mut state = self.lock_state();
            if let Some(thread_id) = state.turns.get(&turn_id).map(|turn| turn.thread_id.clone()) {
                state.running_turns.insert(turn_id.clone(), thread_id);
            }
        }
        let service = self.clone();
        let worker = TurnWorker {
            service: self.clone(),
            turn_id: turn_id.clone(),
        };
        Box::pin(async move {
            let _worker = worker;
            service
                .run_turn_inner(
                    turn_id,
                    agent,
                    prompt,
                    verification_command,
                    workspace,
                    cancel,
                )
                .await
        })
    }

    async fn run_turn_inner(
        &self,
        turn_id: String,
        agent: Agent,
        prompt: RunInput,
        verification_command: Option<String>,
        workspace: PathBuf,
        cancel: CancellationToken,
    ) {
        if self
            .append(&turn_id, TurnEventPayload::Started)
            .await
            .is_err()
        {
            cancel.cancel();
            return;
        }
        let mut prompt = prompt;
        loop {
            let restored_verification = prompt.restored_verification.take();
            let (event_tx, mut event_rx) = mpsc::channel(64);
            let (approval_tx, mut approval_rx) = mpsc::channel(1);
            let (commit_tx, mut commit_rx) = mpsc::channel::<CommitRequest>(1);
            let (model_tx, mut model_rx) = mpsc::channel::<crate::control::ModelBoundary>(1);
            let controls =
                self.lock_state()
                    .turns
                    .get(&turn_id)
                    .map(|turn| crate::control::TurnControl {
                        fence: Arc::clone(&turn.fence),
                        models: model_tx,
                    });
            let run_cancel = cancel.clone();
            let profile = self
                .lock_state()
                .turns
                .get(&turn_id)
                .map_or(PermissionProfile::Ask, |record| record.permission_profile);
            let verification_limits = agent.verification_limits();
            let runner = agent.clone();
            let mut run = tokio::spawn(async move {
                runner
                    .run_context(
                        prompt,
                        run_cancel,
                        RunChannels {
                            events: Some(event_tx),
                            approvals: if profile == PermissionProfile::AllowEffects {
                                None
                            } else {
                                Some(approval_tx)
                            },
                            commits: Some(commit_tx),
                            control: controls,
                        },
                    )
                    .await
            });
            let report = loop {
                tokio::select! {
                    Some(mut request) = model_rx.recv() => {
                        let result = self.prepare_model(&turn_id, &mut request).await;
                        let _ = request.response.send(result);
                    }
                    Some(request) = commit_rx.recv() => {
                        let result = self.commit_records(&turn_id, &request.records).await.map_err(|error| error.to_string());
                        let failed = result.is_err();
                        let _ = request.response.send(result);
                        if failed { cancel.cancel(); }
                    }
                    Some(event) = event_rx.recv() => {
                        if self.append_agent_event(&turn_id, event).await.is_err() {
                            break None;
                        }
                    }
                    Some(request) = approval_rx.recv() => {
                        // Agent control events precede its approval handoff. Drain
                        // that finite prefix before publishing the input request.
                        while let Ok(event) = event_rx.try_recv() {
                            if self.append_agent_event(&turn_id, event).await.is_err() {
                                cancel.cancel();
                            }
                        }
                        if self.request_approval(&turn_id, request).await.is_err() {
                            break None;
                        }
                    }
                    result = &mut run => match result {
                        Ok(report) => break Some(report),
                        Err(error) => {
                            cancel.cancel();
                            let _ = self.append(&turn_id, TurnEventPayload::Finished {
                                status: TurnStatus::Interrupted, detail: format!("agent execution lost: {error}; effects may have occurred"),
                                final_answer: None, verification: VerificationStatus::Unavailable, verification_evidence: None, unknown_effect: true,
                            }).await;
                            return;
                        }
                    },
                }
            };
            if report.is_none() {
                cancel.cancel();
                // Continue draining while cancellation cleans up shell readers.
                loop {
                    tokio::select! {
                        result = &mut run => { let _ = result; break; },
                        Some(_) = event_rx.recv() => {},
                        Some(request) = approval_rx.recv() => { let _ = request.response.send(false); },
                        Some(request) = commit_rx.recv() => { let _ = request.response.send(Err("execution stopped after commit failure".into())); },
                        Some(request) = model_rx.recv() => { let _ = request.response.send(Err("execution stopped after commit failure".into())); },
                    }
                }
                let _ = self
                    .append(
                        &turn_id,
                        TurnEventPayload::Finished {
                            status: TurnStatus::Interrupted,
                            detail:
                                "agent execution stopped unexpectedly; effects may have occurred"
                                    .into(),
                            final_answer: None,
                            verification: VerificationStatus::Unavailable,
                            verification_evidence: None,
                            unknown_effect: true,
                        },
                    )
                    .await;
                return;
            }
            if let Some(mut report) = report {
                while let Ok(event) = event_rx.try_recv() {
                    if self.append_agent_event(&turn_id, event).await.is_err() {
                        return;
                    }
                }
                let mut status = match report.status {
                    RunStatus::Completed => TurnStatus::Completed,
                    RunStatus::Cancelled => TurnStatus::Cancelled,
                    RunStatus::Failed | RunStatus::BoundExceeded => TurnStatus::Failed,
                };
                if cancel.is_cancelled() {
                    status = TurnStatus::Cancelled;
                }
                if report.unknown_effect {
                    status = TurnStatus::RecoveryRequired;
                }
                let mut unknown_effect = report.unknown_effect;
                let (verification, evidence) = if status == TurnStatus::Completed {
                    match (restored_verification, verification_command.clone()) {
                        (Some((verification, evidence)), _) => (verification, Some(evidence)),
                        (None, Some(command)) => {
                            let budget = VerificationBudget {
                                duration: verification_limits.0.saturating_sub(
                                    Duration::from_millis(report.active_duration_ms),
                                ),
                                active_duration_ms: report.active_duration_ms,
                                calls: report.tool_calls,
                                max_calls: verification_limits.1,
                            };
                            let (verification, evidence, uncertain) = match self
                                .run_verification(&turn_id, &workspace, command, &cancel, budget)
                                .await
                            {
                                Ok(result) => result,
                                Err(_) => return,
                            };
                            unknown_effect |= uncertain;
                            if verification != VerificationStatus::Passed {
                                status = if uncertain {
                                    TurnStatus::RecoveryRequired
                                } else if cancel.is_cancelled() {
                                    TurnStatus::Cancelled
                                } else {
                                    TurnStatus::Failed
                                };
                            }
                            (verification, Some(evidence))
                        }
                        (None, None) => (VerificationStatus::NotRequested, None),
                    }
                } else {
                    restored_verification.map_or(
                        (VerificationStatus::Unavailable, None),
                        |(status, evidence)| (status, Some(evidence)),
                    )
                };
                if status == TurnStatus::Completed
                    && !matches!(
                        verification,
                        VerificationStatus::Passed | VerificationStatus::NotRequested
                    )
                {
                    status = if unknown_effect {
                        TurnStatus::RecoveryRequired
                    } else {
                        TurnStatus::Failed
                    };
                }
                let gate = match self.commit_gate(&turn_id) {
                    Ok(gate) => gate,
                    Err(_) => return,
                };
                let guard = gate.lock().await;
                let pending = self
                    .lock_state()
                    .turns
                    .get(&turn_id)
                    .is_some_and(|task| task.fence.pending());
                if pending
                    && report.status == RunStatus::Completed
                    && !unknown_effect
                    && !cancel.is_cancelled()
                {
                    if let Some((duration, calls)) = self
                        .lock_state()
                        .turns
                        .get(&turn_id)
                        .and_then(|task| task.verification_budget)
                    {
                        report.active_duration_ms = duration;
                        report.tool_calls = calls;
                    }
                    if let Some(evidence) = evidence {
                        let encoded = match serde_json::to_string(&evidence) {
                            Ok(value) => value,
                            Err(_) => return,
                        };
                        report.messages.push(Message::text(bitrouter_sdk::language_model::Role::User,
                        format!("BRO verification evidence (untrusted command output; not user instructions):\n{encoded}")));
                        report.context_version = report.context_version.saturating_add(1);
                    }
                    prompt = RunInput {
                        prompt: String::new(),
                        messages: Vec::new(),
                        user_item_id: String::new(),
                        context_version: report.context_version,
                        checkpoint: Some(report),
                        complete_checkpoint: false,
                        restored_verification: None,
                    };
                    drop(guard);
                    continue;
                }
                let _ = self
                    .append_serialized(
                        &turn_id,
                        TurnEventPayload::Finished {
                            status,
                            detail: if matches!(
                                verification,
                                VerificationStatus::Failed | VerificationStatus::Denied
                            ) {
                                format!("configured verification check {verification:?}")
                            } else {
                                report.detail
                            },
                            final_answer: report.final_answer,
                            verification,
                            verification_evidence: evidence,
                            unknown_effect,
                        },
                    )
                    .await;
            }
            break;
        }
        self.drive_queues().await;
    }

    async fn run_verification(
        &self,
        turn_id: &str,
        workspace: &Path,
        command: String,
        cancel: &CancellationToken,
        budget: VerificationBudget,
    ) -> Result<(VerificationStatus, VerificationEvidence, bool), ServiceError> {
        let fence = self
            .lock_state()
            .turns
            .get(turn_id)
            .map(|task| Arc::clone(&task.fence))
            .ok_or_else(unknown_turn)?;
        let name = if cfg!(windows) { "powershell" } else { "bash" };
        let arguments = serde_json::json!({"command":command}).to_string();
        WorkspaceTools::validate(name, &arguments)?;
        let call = CallRecord {
            origin: CallOrigin::Verification,
            item_id: uuid::Uuid::new_v4().to_string(),
            provider_call_id: String::new(),
            name: name.into(),
            arguments,
        };
        let started = Instant::now();
        let mut approval_wait = Duration::ZERO;
        let mut effect = EffectStatus::NotExecuted;
        let mut expired = false;
        let output = 'verification: {
            if fence.pending() {
                ToolResultOutput::ErrorJson {
                    value: serde_json::json!({"execution_status":"not_executed","error":"not_executed_due_to_steer"}),
                }
            } else if budget.duration.is_zero()
                || budget.calls >= budget.max_calls
                || cancel.is_cancelled()
            {
                ToolResultOutput::ErrorJson {
                    value: serde_json::json!({"not_executed":true,"error":"verification cannot start within the remaining execution budget"}),
                }
            } else {
                let allow_effects =
                    self.lock_state().turns.get(turn_id).is_some_and(|task| {
                        task.permission_profile == PermissionProfile::AllowEffects
                    });
                let approved = if allow_effects {
                    true
                } else {
                    let (response, receiver) = oneshot::channel();
                    let wait_started = Instant::now();
                    self.request_approval(
                        turn_id,
                        ApprovalRequest {
                            id: uuid::Uuid::new_v4().to_string(),
                            tool_id: call.item_id.clone(),
                            tool_name: call.name.clone(),
                            arguments: call.arguments.clone(),
                            response,
                        },
                    )
                    .await?;
                    let approved = tokio::select! { biased; _ = cancel.cancelled() => false, result = receiver => result.unwrap_or(false) };
                    approval_wait += wait_started.elapsed();
                    approved
                };
                if cancel.is_cancelled() {
                    ToolResultOutput::ErrorJson {
                        value: serde_json::json!({"not_executed":true,"error":"verification cancelled before execution"}),
                    }
                } else if fence.pending() {
                    ToolResultOutput::ErrorJson {
                        value: serde_json::json!({"execution_status":"not_executed","error":"not_executed_due_to_steer"}),
                    }
                } else if !approved {
                    ToolResultOutput::ExecutionDenied {
                        reason: Some("verification denied".into()),
                    }
                } else {
                    let remaining = budget
                        .duration
                        .saturating_sub(started.elapsed().saturating_sub(approval_wait));
                    let permit = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => None,
                        _ = fence.received() => None,
                        result = tokio::time::timeout(remaining, Arc::clone(&self.inner.tool_workers).acquire_owned()) => result.ok().and_then(Result::ok),
                    };
                    if let Some(permit) = permit {
                        if cancel.is_cancelled()
                            || started.elapsed().saturating_sub(approval_wait) >= budget.duration
                        {
                            ToolResultOutput::ErrorJson {
                                value: serde_json::json!({"not_executed":true,"error":"verification stopped before execution"}),
                            }
                        } else {
                            let tools = match WorkspaceTools::new(workspace) {
                                Ok(tools) => tools,
                                Err(error) => {
                                    break 'verification ToolResultOutput::ErrorJson {
                                        value: serde_json::json!({"not_executed":true,"error":error.to_string()}),
                                    };
                                }
                            };
                            self.commit_records(
                                turn_id,
                                &[ExecutionRecord::ToolIntent {
                                    step_id: uuid::Uuid::new_v4().to_string(),
                                    call: call.clone(),
                                }],
                            )
                            .await?;
                            if cancel.is_cancelled()
                                || started.elapsed().saturating_sub(approval_wait)
                                    >= budget.duration
                            {
                                break 'verification ToolResultOutput::ErrorJson {
                                    value: serde_json::json!({"not_executed":true,"error":"verification stopped after intent commit"}),
                                };
                            }
                            let worker_cancel = cancel.child_token();
                            let (events, mut receiver) = mpsc::channel(64);
                            let tool_cancel = worker_cancel.clone();
                            let arguments = call.arguments.clone();
                            let item_id = call.item_id.clone();
                            let dispatched = fence.launch(|| {
                                let (start, ready) = oneshot::channel::<()>();
                                let run = tokio::spawn(async move {
                                    let _permit = permit;
                                    if ready.await.is_err() { return ToolResultOutput::ErrorJson {
                                        value: serde_json::json!({"execution_status":"not_executed","error":"verification start withdrawn"}),
                                    }; }
                                    tools.execute(name, &arguments, &tool_cancel, &item_id, Some(&events)).await
                                });
                                (start, run)
                            });
                            let Some((start, mut run)) = dispatched else {
                                break 'verification ToolResultOutput::ErrorJson {
                                    value: serde_json::json!({"execution_status":"not_executed","error":"not_executed_due_to_steer"}),
                                };
                            };
                            let _ = start.send(());
                            let remaining = budget
                                .duration
                                .saturating_sub(started.elapsed().saturating_sub(approval_wait));
                            let deadline = tokio::time::sleep(remaining);
                            tokio::pin!(deadline);
                            let mut storage_error = None;
                            let result = loop {
                                tokio::select! {
                                    result = &mut run => break result.unwrap_or_else(|error| ToolResultOutput::ErrorJson {
                                        value: serde_json::json!({"error":format!("verification worker lost: {error}"),"worker_lost":true}),
                                    }),
                                    _ = &mut deadline, if !expired => { expired = true; worker_cancel.cancel(); },
                                    Some(event) = receiver.recv() => if storage_error.is_none() && let Err(error) = self.append_agent_event(turn_id, event).await { storage_error = Some(error); worker_cancel.cancel(); },
                                }
                            };
                            while let Ok(event) = receiver.try_recv() {
                                if storage_error.is_none()
                                    && let Err(error) =
                                        self.append_agent_event(turn_id, event).await
                                {
                                    storage_error = Some(error);
                                }
                            }
                            if let Some(error) = storage_error {
                                return Err(error);
                            }
                            effect = if cancel.is_cancelled()
                                || expired
                                || matches!(result, ToolResultOutput::ErrorJson { .. })
                            {
                                EffectStatus::Unknown
                            } else {
                                EffectStatus::Completed
                            };
                            result
                        }
                    } else {
                        ToolResultOutput::ErrorJson {
                            value: serde_json::json!({"not_executed":true,"error":"verification worker cancelled, unavailable or time bound reached"}),
                        }
                    }
                }
            }
        };
        let mut evidence = verification_evidence(command, &output);
        evidence.timed_out |= expired;
        let verification = if matches!(output, ToolResultOutput::ExecutionDenied { .. }) {
            VerificationStatus::Denied
        } else if effect == EffectStatus::NotExecuted {
            VerificationStatus::Unavailable
        } else if evidence.exit_status == Some(0) && !evidence.timed_out && evidence.error.is_none()
        {
            VerificationStatus::Passed
        } else {
            VerificationStatus::Failed
        };
        let active_duration_ms = budget.active_duration_ms.saturating_add(
            u64::try_from(started.elapsed().saturating_sub(approval_wait).as_millis())
                .unwrap_or(u64::MAX),
        );
        self.commit_records(
            turn_id,
            &[ExecutionRecord::VerificationResult {
                status: Some(verification),
                call: call.clone(),
                evidence: evidence.clone(),
                effect,
                active_duration_ms,
                tool_calls: budget
                    .calls
                    .saturating_add(u32::from(budget.calls < budget.max_calls)),
            }],
        )
        .await?;
        Ok((verification, evidence, effect == EffectStatus::Unknown))
    }

    async fn request_approval(
        &self,
        turn_id: &str,
        request: ApprovalRequest,
    ) -> Result<(), ServiceError> {
        let gate = self.commit_gate(turn_id)?;
        let _guard = gate.lock().await;
        {
            let mut state = self.lock_state();
            let record = state.turns.get_mut(turn_id).ok_or_else(unknown_turn)?;
            if record.cancel.is_cancelled() || record.fence.pending() {
                let _ = request.response.send(false);
                return Ok(());
            }
            if record.snapshot.status.terminal() || record.pending.is_some() {
                return Err("task cannot accept another pending input".into());
            }
            record.pending = Some(PendingInput {
                response: request.response,
            });
        }
        self.append_serialized(
            turn_id,
            TurnEventPayload::InputRequested {
                request_id: request.id,
                tool_id: request.tool_id,
                tool_name: request.tool_name,
                arguments: request.arguments,
            },
        )
        .await
    }

    async fn append_agent_event(&self, turn_id: &str, event: RunEvent) -> Result<(), ServiceError> {
        let payload = match event {
            RunEvent::AssistantDelta { item_id, text } => {
                TurnEventPayload::AssistantDelta { item_id, text }
            }
            RunEvent::ToolOutputDelta { id, source, text } => {
                TurnEventPayload::ToolOutputDelta { id, source, text }
            }
            // Complete items and their start identities are published by the
            // acknowledged canonical transaction, never a second event commit.
            _ => return Ok(()),
        };
        self.append(turn_id, payload).await
    }

    fn commit_gate(&self, turn_id: &str) -> Result<Arc<tokio::sync::Mutex<()>>, ServiceError> {
        let state = self.lock_state();
        let record = state.turns.get(turn_id).ok_or_else(unknown_turn)?;
        state
            .threads
            .get(&record.thread_id)
            .map(|thread| Arc::clone(&thread.commit_lock))
            .ok_or_else(unknown_turn)
    }

    async fn commit_records(
        &self,
        turn_id: &str,
        records: &[ExecutionRecord],
    ) -> Result<(), ServiceError> {
        let gate = self.commit_gate(turn_id)?;
        let _guard = gate.lock().await;
        self.commit_serialized(turn_id, records).await?;
        for record in records {
            if let ExecutionRecord::VerificationResult {
                active_duration_ms,
                tool_calls,
                ..
            } = record
                && let Some(task) = self.lock_state().turns.get_mut(turn_id)
            {
                task.verification_budget = Some((*active_duration_ms, *tool_calls));
            }
        }
        Ok(())
    }

    async fn commit_serialized(
        &self,
        turn_id: &str,
        records: &[ExecutionRecord],
    ) -> Result<(), ServiceError> {
        let thread_id = self
            .lock_state()
            .turns
            .get(turn_id)
            .map(|record| record.thread_id.clone())
            .ok_or_else(unknown_turn)?;
        self.commit_turn_serialized(&thread_id, turn_id, records)
            .await
    }

    async fn append(&self, turn_id: &str, payload: TurnEventPayload) -> Result<(), ServiceError> {
        let gate = self.commit_gate(turn_id)?;
        let _guard = gate.lock().await;
        self.append_serialized(turn_id, payload).await
    }

    async fn append_serialized(
        &self,
        turn_id: &str,
        payload: TurnEventPayload,
    ) -> Result<(), ServiceError> {
        self.append_facts_serialized(turn_id, payload, &[]).await
    }

    async fn append_facts_serialized(
        &self,
        turn_id: &str,
        mut payload: TurnEventPayload,
        extra_facts: &[ExecutionRecord],
    ) -> Result<(), ServiceError> {
        if let TurnEventPayload::Finished {
            status,
            unknown_effect: true,
            ..
        } = &mut payload
        {
            *status = TurnStatus::RecoveryRequired;
        }
        if matches!(
            payload,
            TurnEventPayload::AssistantDelta { .. } | TurnEventPayload::ToolOutputDelta { .. }
        ) {
            return self.append_locked(&mut self.lock_state(), turn_id, payload);
        }
        if matches!(payload, TurnEventPayload::Finished { status, .. } if status.terminal())
            && let Err(error) = self.prepare_workspace_finish(turn_id).await
        {
            self.workspace_finish_failed(turn_id, &error);
            return Err(error);
        }
        let events = {
            let state = self.lock_state();
            let record = state.turns.get(turn_id).ok_or_else(unknown_turn)?;
            let mut payloads = Vec::new();
            if matches!(
                payload,
                TurnEventPayload::Finished { .. }
                    | TurnEventPayload::CancelRequested
                    | TurnEventPayload::SteeringUpdated { .. }
            ) && let Some(request_id) = record.snapshot.pending_input_id.clone()
            {
                payloads.push(TurnEventPayload::InputResolved {
                    request_id,
                    approved: false,
                });
            }
            if let TurnEventPayload::Finished { detail, .. } = &payload {
                for entry in &record.steering {
                    if entry.receipt.status == crate::thread::SteeringStatus::Received {
                        let mut receipt = entry.receipt.clone();
                        receipt.status = crate::thread::SteeringStatus::NotApplied;
                        receipt.reason = Some(detail.clone());
                        payloads.push(TurnEventPayload::SteeringUpdated {
                            receipt,
                            text: None,
                        });
                    }
                }
            }
            payloads.push(payload);
            payloads
                .into_iter()
                .enumerate()
                .map(|(offset, payload)| TurnEvent {
                    thread_id: record.thread_id.clone(),
                    server_instance_id: self.inner.instance_id.clone(),
                    turn_id: turn_id.into(),
                    seq: record.snapshot.cursor + offset as u64 + 1,
                    timestamp_ms: now_ms(),
                    payload,
                })
                .collect::<Vec<_>>()
        };
        let mut facts = events
            .iter()
            .cloned()
            .filter_map(|event| lifecycle_fact(&event))
            .collect::<Vec<_>>();
        facts.extend_from_slice(extra_facts);
        for event in &events {
            if let TurnEventPayload::SteeringUpdated {
                receipt,
                text: None,
            } = &event.payload
            {
                facts.push(ExecutionRecord::SteeringResolved {
                    receipt: receipt.clone(),
                });
            }
        }

        if let Some(fact) = self.terminal_thread_checkpoint(turn_id, &events, facts.len())? {
            facts.push(fact);
        }
        self.commit_serialized(turn_id, &facts).await?;
        let mut state = self.lock_state();
        for fact in &facts {
            if let ExecutionRecord::SteeringResolved { receipt } = fact
                && let Some(task) = state.turns.get_mut(turn_id)
                && let Some(entry) = task
                    .steering
                    .iter_mut()
                    .find(|entry| entry.receipt.input_id == receipt.input_id)
            {
                entry.receipt = receipt.clone();
            }
        }
        for event in events {
            self.append_locked(&mut state, turn_id, event.payload)?;
        }
        Ok(())
    }

    fn resolve_pending(&self, state: &mut State, turn_id: &str) -> Result<(), ServiceError> {
        if let Some(id) = state
            .turns
            .get(turn_id)
            .and_then(|record| record.snapshot.pending_input_id.clone())
        {
            self.append_locked(
                state,
                turn_id,
                TurnEventPayload::InputResolved {
                    request_id: id,
                    approved: false,
                },
            )?;
            if let Some(pending) = state
                .turns
                .get_mut(turn_id)
                .and_then(|record| record.pending.take())
            {
                let _ = pending.response.send(false);
            }
        }
        Ok(())
    }

    fn append_locked(
        &self,
        state: &mut State,
        turn_id: &str,
        mut payload: TurnEventPayload,
    ) -> Result<(), ServiceError> {
        if let TurnEventPayload::Finished { detail, .. } = &mut payload
            && detail.len() > MAX_LIVE_BYTES
        {
            let mut end = MAX_LIVE_BYTES;
            while !detail.is_char_boundary(end) {
                end -= 1;
            }
            detail.truncate(end);
            detail.push_str(" (detail truncated)");
        }
        if matches!(payload, TurnEventPayload::Finished { .. }) {
            self.resolve_pending(state, turn_id)?;
        }
        let record = state.turns.get(turn_id).ok_or_else(unknown_turn)?;
        let event = TurnEvent {
            thread_id: record.thread_id.clone(),
            server_instance_id: self.inner.instance_id.clone(),
            turn_id: turn_id.into(),
            seq: record.snapshot.cursor + 1,
            timestamp_ms: now_ms(),
            payload,
        };
        let record = state.turns.get_mut(turn_id).ok_or_else(unknown_turn)?;
        record.snapshot.apply(&event);
        if let Some(thread) = state.threads.get(&record.thread_id) {
            record.snapshot.cursor = thread.store_version;
        }
        if record.snapshot.status.terminal() {
            if state
                .active_workspaces
                .get(&record.snapshot.workspace)
                .is_some_and(|owner| owner == turn_id)
            {
                state.active_workspaces.remove(&record.snapshot.workspace);
                state.workspace_fences.remove(&record.snapshot.workspace);
            }
            record.pending.take();
            record.terminal_at = Some(Instant::now());
        }
        if matches!(
            event.payload,
            TurnEventPayload::AssistantDelta { .. } | TurnEventPayload::ToolOutputDelta { .. }
        ) && let Some(thread) = state.threads.get_mut(&event.thread_id)
        {
            thread.presentation.live(&event);
        }
        self.prune(state);
        Ok(())
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        match self.inner.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

fn lifecycle_fact(event: &TurnEvent) -> Option<ExecutionRecord> {
    use crate::thread::TurnLifecycle as L;
    let lifecycle = match &event.payload {
        TurnEventPayload::Started => L::Started,
        TurnEventPayload::InputRequested {
            request_id,
            tool_id,
            tool_name,
            arguments,
        } => L::InputRequested {
            request_id: request_id.clone(),
            tool_id: tool_id.clone(),
            tool_name: tool_name.clone(),
            arguments: arguments.clone(),
        },
        TurnEventPayload::InputResolved {
            request_id,
            approved,
        } => L::InputResolved {
            request_id: request_id.clone(),
            approved: *approved,
        },
        TurnEventPayload::CancelRequested => L::CancelRequested,
        TurnEventPayload::SteeringUpdated { receipt, text } => L::SteeringUpdated {
            receipt: receipt.clone(),
            text: text.clone(),
        },
        TurnEventPayload::Finished {
            status,
            detail,
            final_answer,
            verification,
            verification_evidence,
            unknown_effect,
        } => L::Finished {
            status: *status,
            detail: detail.clone(),
            final_answer: final_answer.clone(),
            verification: *verification,
            verification_evidence: verification_evidence.clone(),
            unknown_effect: *unknown_effect,
        },
        _ => return None,
    };
    Some(ExecutionRecord::TurnLifecycle {
        turn_id: event.turn_id.clone(),
        lifecycle,
    })
}

impl TurnSnapshot {
    pub fn apply(&mut self, event: &TurnEvent) {
        if event.server_instance_id != self.server_instance_id
            || event.turn_id != self.turn_id
            || event.thread_id != self.thread_id
            || event.seq <= self.cursor
                && !matches!(
                    event.payload,
                    TurnEventPayload::AssistantDelta { .. }
                        | TurnEventPayload::ToolOutputDelta { .. }
                )
        {
            return;
        }
        if !matches!(
            event.payload,
            TurnEventPayload::AssistantDelta { .. } | TurnEventPayload::ToolOutputDelta { .. }
        ) {
            self.cursor = event.seq;
        }
        self.apply_payload(&event.payload);
    }

    pub(crate) fn apply_payload(&mut self, payload: &TurnEventPayload) {
        match payload {
            TurnEventPayload::TurnQueued { .. } => self.status = TurnStatus::Queued,
            TurnEventPayload::SteeringUpdated { receipt, .. } => {
                if let Some(current) = self
                    .steering
                    .iter_mut()
                    .find(|current| current.input_id == receipt.input_id)
                {
                    *current = receipt.clone();
                } else {
                    self.steering.push(receipt.clone());
                }
            }
            TurnEventPayload::Accepted { .. } => self.status = TurnStatus::Accepted,
            TurnEventPayload::Started | TurnEventPayload::InputResolved { .. } => {
                self.status = TurnStatus::Running;
                self.pending_input_id = None;
                self.pending_input = None;
            }
            TurnEventPayload::InputRequested {
                request_id,
                tool_id,
                tool_name,
                arguments,
            } => {
                self.status = TurnStatus::WaitingForInput;
                self.pending_input_id = Some(request_id.clone());
                self.pending_input = Some(InputRequest {
                    request_id: request_id.clone(),
                    tool_id: tool_id.clone(),
                    tool_name: tool_name.clone(),
                    arguments: arguments.clone(),
                });
            }
            TurnEventPayload::Finished {
                status,
                detail,
                final_answer,
                verification,
                verification_evidence,
                unknown_effect,
            } => {
                self.status = *status;
                self.detail = Some(detail.clone());
                self.unknown_effect = *unknown_effect;
                self.final_answer = final_answer.clone();
                self.verification = *verification;
                self.verification_evidence = verification_evidence.clone();
                self.pending_input_id = None;
                self.pending_input = None;
                self.live = None;
            }
            TurnEventPayload::AssistantDelta { item_id, text } => {
                self.live(Some(item_id), "assistant", text)
            }
            TurnEventPayload::ToolOutputDelta { id, source, text } => self.live(
                Some(id),
                &format!("shell {id}"),
                &format!("[{source}] {text}"),
            ),
            TurnEventPayload::CancelRequested => {}
        }
    }
    fn live(&mut self, item_id: Option<&str>, kind: &str, text: &str) {
        let live = self.live.get_or_insert_with(|| LiveActivity {
            item_id: item_id.map(str::to_owned),
            kind: kind.into(),
            text: String::new(),
            truncated: false,
        });
        if live.kind != kind || live.item_id.as_deref() != item_id {
            live.item_id = item_id.map(str::to_owned);
            live.kind = kind.into();
            live.text.clear();
            live.truncated = false;
        }
        live.text.push_str(text);
        if live.text.len() > MAX_LIVE_BYTES {
            let mut start = live.text.len() - MAX_LIVE_BYTES;
            while !live.text.is_char_boundary(start) {
                start += 1;
            }
            live.text.drain(..start);
            live.truncated = true;
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn verification_evidence(command: String, result: &ToolResultOutput) -> VerificationEvidence {
    let mut evidence = VerificationEvidence {
        command: command.clone(),
        exit_status: None,
        stdout: String::new(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        timed_out: false,
        error: None,
    };
    match result {
        ToolResultOutput::Json { value } => {
            evidence.exit_status = value
                .get("exit_status")
                .and_then(serde_json::Value::as_i64)
                .and_then(|status| i32::try_from(status).ok());
            evidence.stdout = value
                .get("stdout")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .into();
            evidence.stderr = value
                .get("stderr")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .into();
            evidence.stdout_truncated = value
                .get("stdout_truncated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            evidence.stderr_truncated = value
                .get("stderr_truncated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            evidence.timed_out = value
                .get("timed_out")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
        }
        ToolResultOutput::ErrorJson { value } => {
            evidence.error = Some(
                value
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("verification command failed before execution")
                    .into(),
            );
        }
        ToolResultOutput::ExecutionDenied { reason } => evidence.error = reason.clone(),
        _ => evidence.error = Some("unexpected verification output".into()),
    }
    evidence
}

#[cfg(test)]
fn turn_fact(record: &ExecutionRecord) -> &ExecutionRecord {
    match record {
        ExecutionRecord::TurnRecord { fact, .. } => turn_fact(fact),
        _ => record,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_sdk::language_model::types::{
        AuthScheme, Content, FinishReason, GenerateResult, RoutingTarget, Usage,
    };
    use bitrouter_sdk::language_model::{
        ApiProtocol, MockExecutor, MockResponse, StaticRoutingTable, StreamPart,
    };
    use tempfile::TempDir;

    fn target() -> RoutingTarget {
        RoutingTarget {
            provider_name: "fixture".into(),
            service_id: "fixture-model".into(),
            api_base: "https://example.invalid".into(),
            api_key: "fixture-key".into(),
            api_protocol: ApiProtocol::ChatCompletions,
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
            reasoning_effort: None,
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: AuthScheme::Bearer,
            headers: Vec::new(),
        }
    }

    pub(super) fn app(turns: Vec<GenerateResult>) -> std::io::Result<Arc<App>> {
        app_with_executor(Arc::new(MockExecutor::new(
            turns.into_iter().map(mock_stream).collect(),
        )))
    }

    pub(super) fn app_with_executor(
        executor: Arc<dyn bitrouter_sdk::language_model::Executor>,
    ) -> std::io::Result<Arc<App>> {
        let table = StaticRoutingTable::new();
        table.insert("fixture-model", vec![target()]);
        App::builder()
            .language_model(|builder| {
                builder.routing_table(Arc::new(table)).executor(executor);
            })
            .build()
            .map(Arc::new)
            .map_err(std::io::Error::other)
    }

    pub(super) fn turn(parts: Vec<Content>) -> GenerateResult {
        GenerateResult {
            finish_reason: Some(
                if parts
                    .iter()
                    .any(|part| matches!(part, Content::ToolCall { .. }))
                {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Stop
                },
            ),
            content: parts,
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                ..Default::default()
            }),
            response_id: None,
            stop_details: None,
            provider_metadata: Default::default(),
        }
    }

    pub(super) fn mock_stream(turn: GenerateResult) -> MockResponse {
        let mut parts = Vec::new();
        for content in turn.content {
            match content {
                Content::Text { text, .. } => parts.push(StreamPart::TextDelta { text }),
                Content::ToolCall {
                    id,
                    name,
                    arguments,
                    provider_metadata,
                    ..
                } => {
                    parts.push(StreamPart::ToolCallDelta {
                        id,
                        name: Some(name),
                        arguments,
                        provider_metadata,
                    });
                }
                _ => {}
            }
        }
        if let Some(usage) = turn.usage {
            parts.push(StreamPart::Usage { usage });
        }
        if let Some(reason) = turn.finish_reason {
            parts.push(StreamPart::Finish { reason });
        }
        MockResponse::Stream(parts)
    }

    pub(super) fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> Content {
        Content::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.to_string(),
            provider_executed: false,
            dynamic: false,
            provider_metadata: Default::default(),
        }
    }

    pub(super) fn final_turn() -> GenerateResult {
        turn(vec![Content::Text {
            text: "done".into(),
            provider_metadata: Default::default(),
        }])
    }

    fn request(workspace: &TempDir) -> TurnFixture {
        TurnFixture {
            prompt: "change the file".into(),
            workspace: workspace.path().to_path_buf(),
            caller: CallerContext::local(),
            config: AgentConfig::fixed("fixture-model", None),
            verification_command: None,
            idempotency_key: None,
        }
    }

    pub(super) async fn wait_for(
        service: &ThreadService,
        turn_id: &str,
        status: TurnStatus,
    ) -> Result<TurnSnapshot, String> {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let snapshot = service.read(turn_id).map_err(|error| error.to_string())?;
                if snapshot.status == status || snapshot.status.terminal() {
                    return Ok(snapshot);
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .map_err(|error| error.to_string())?
    }

    #[tokio::test]
    async fn approval_is_task_bound_and_one_use() -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let other = TempDir::new()?;
        let app = app(vec![
            turn(vec![tool_call(
                "write",
                "write",
                serde_json::json!({"path":"created.txt", "content":"created"}),
            )]),
            final_turn(),
        ])?;
        let service = ThreadService::new(app, &[workspace.path().to_path_buf()])
            .map_err(std::io::Error::other)?;
        assert!(service.submit_fixture(request(&other)).await.is_err());
        let accepted = service
            .submit_fixture(request(&workspace))
            .await
            .map_err(std::io::Error::other)?;
        assert!(service.submit_fixture(request(&workspace)).await.is_err());
        let waiting = wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput)
            .await
            .map_err(std::io::Error::other)?;
        assert_eq!(waiting.status, TurnStatus::WaitingForInput);
        let input_id = waiting
            .pending_input_id
            .ok_or_else(|| std::io::Error::other("missing pending input"))?;
        assert!(
            service
                .answer_input(&accepted.turn_id, "wrong", true)
                .await
                .is_err()
        );
        assert!(!workspace.path().join("created.txt").exists());
        service
            .answer_input(&accepted.turn_id, &input_id, true)
            .await
            .map_err(std::io::Error::other)?;
        assert!(
            service
                .answer_input(&accepted.turn_id, &input_id, true)
                .await
                .is_err()
        );
        let completed = wait_for(&service, &accepted.turn_id, TurnStatus::Completed)
            .await
            .map_err(std::io::Error::other)?;
        assert_eq!(completed.status, TurnStatus::Completed);
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("created.txt"))?,
            "created"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_while_waiting_never_authorizes_a_write()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let service = ThreadService::new(
            app(vec![turn(vec![tool_call(
                "write",
                "write",
                serde_json::json!({"path":"created.txt", "content":"created"}),
            )])])?,
            &[workspace.path().to_path_buf()],
        )
        .map_err(std::io::Error::other)?;
        let accepted = service
            .submit_fixture(request(&workspace))
            .await
            .map_err(std::io::Error::other)?;
        let waiting = wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput)
            .await
            .map_err(std::io::Error::other)?;
        let input_id = waiting
            .pending_input_id
            .ok_or_else(|| std::io::Error::other("missing pending input"))?;
        service
            .cancel(&accepted.turn_id)
            .await
            .map_err(std::io::Error::other)?;
        assert!(
            service
                .answer_input(&accepted.turn_id, &input_id, true)
                .await
                .is_err()
        );
        let cancelled = wait_for(&service, &accepted.turn_id, TurnStatus::Cancelled)
            .await
            .map_err(std::io::Error::other)?;
        assert_eq!(cancelled.status, TurnStatus::Cancelled);
        assert!(!workspace.path().join("created.txt").exists());
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_seals_admission_and_joins_waiting_execution()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let other = TempDir::new()?;
        let limits = RuntimeLimits {
            active_turns: 1,
            ..RuntimeLimits::default()
        };
        let service = ThreadService::with_limits(
            app(vec![turn(vec![tool_call(
                "write",
                "write",
                serde_json::json!({"path":"created.txt", "content":"created"}),
            )])])?,
            &[workspace.path().to_path_buf(), other.path().to_path_buf()],
            limits,
        )?;
        let accepted = service.submit_fixture(request(&workspace)).await?;
        wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput).await?;
        assert_eq!(
            service
                .submit_fixture(request(&other))
                .await
                .err()
                .map(|error| error.code),
            Some(ErrorCode::Overloaded)
        );
        tokio::time::timeout(Duration::from_secs(2), service.shutdown()).await?;
        assert_eq!(
            service.read(&accepted.turn_id)?.status,
            TurnStatus::Cancelled
        );
        assert!(service.read(&accepted.turn_id)?.pending_input.is_none());
        assert!(service.inner.workers.is_empty());
        assert!(service.lock_state().active_workspaces.is_empty());
        assert_eq!(
            service
                .submit_fixture(request(&workspace))
                .await
                .err()
                .map(|error| error.code),
            Some(ErrorCode::ShuttingDown)
        );
        assert!(!workspace.path().join("created.txt").exists());
        Ok(())
    }

    #[tokio::test]
    async fn terminal_cache_eviction_preserves_durable_acceptance_keys()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let service = ThreadService::with_limits(
            app(vec![final_turn(), final_turn(), final_turn()])?,
            &[workspace.path().to_path_buf()],
            RuntimeLimits {
                retained_turns: 1,
                ..RuntimeLimits::default()
            },
        )?;
        let mut first = request(&workspace);
        first.idempotency_key = Some("first".into());
        let first = service.submit_fixture(first).await?;
        wait_for(&service, &first.turn_id, TurnStatus::Completed).await?;
        let second = service.submit_fixture(request(&workspace)).await?;
        wait_for(&service, &second.turn_id, TurnStatus::Completed).await?;
        assert_eq!(
            service.read(&first.turn_id).err().map(|error| error.code),
            Some(ErrorCode::UnknownTurn)
        );
        let mut reused = request(&workspace);
        reused.idempotency_key = Some("first".into());
        assert_eq!(service.submit_fixture(reused).await?.turn_id, first.turn_id);
        service.shutdown().await;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_waits_for_verification_process_cleanup()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let service =
            ThreadService::new(app(vec![final_turn()])?, &[workspace.path().to_path_buf()])?;
        let mut submitted = request(&workspace);
        submitted.verification_command = Some("touch started; sleep 30; touch leaked".into());
        let accepted = service.submit_fixture(submitted).await?;
        let waiting = wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput).await?;
        service
            .answer_input(
                &accepted.turn_id,
                waiting
                    .pending_input_id
                    .as_deref()
                    .ok_or("verification approval missing")?,
                true,
            )
            .await?;
        tokio::time::timeout(Duration::from_secs(3), async {
            while !workspace.path().join("started").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        tokio::time::timeout(Duration::from_secs(3), service.shutdown()).await?;
        assert!(service.inner.workers.is_empty());
        assert_eq!(
            service.read(&accepted.turn_id)?.status,
            TurnStatus::RecoveryRequired
        );
        assert!(service.read(&accepted.turn_id)?.unknown_effect);
        assert!(!workspace.path().join("leaked").exists());
        Ok(())
    }

    #[tokio::test]
    async fn configured_verification_records_exit_status_and_controls_outcome()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let service = ThreadService::new(
            app(vec![final_turn(), final_turn()])?,
            &[workspace.path().to_path_buf()],
        )
        .map_err(std::io::Error::other)?;
        let mut read_only = request(&workspace);
        read_only.config = read_only.config.read_only();
        read_only.verification_command = Some("echo forbidden".into());
        assert!(service.submit_fixture(read_only).await.is_err());
        let mut passing = request(&workspace);
        passing.verification_command = Some("echo verified".into());
        let accepted = service
            .submit_fixture(passing)
            .await
            .map_err(std::io::Error::other)?;
        let waiting = wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput).await?;
        assert_eq!(
            service.inner.tool_workers.available_permits(),
            service.inner.limits.global_tools
        );
        service
            .answer_input(
                &accepted.turn_id,
                waiting
                    .pending_input_id
                    .as_deref()
                    .ok_or("verification approval missing")?,
                true,
            )
            .await?;
        let passed = wait_for(&service, &accepted.turn_id, TurnStatus::Completed)
            .await
            .map_err(std::io::Error::other)?;
        assert_eq!(passed.status, TurnStatus::Completed);
        assert_eq!(passed.verification, VerificationStatus::Passed);
        let stored = service
            .inner
            .store
            .load(&accepted.thread_id)
            .await?
            .ok_or("execution records missing")?;
        assert!(stored.records.iter().any(|record| matches!(turn_fact(record), ExecutionRecord::ToolIntent { call, .. } if call.origin == CallOrigin::Verification)));
        assert!(stored.records.iter().any(|record| matches!(turn_fact(record), ExecutionRecord::VerificationResult { call, effect: EffectStatus::Completed, evidence, .. } if call.origin == CallOrigin::Verification && evidence.exit_status == Some(0))));
        assert_eq!(
            passed
                .verification_evidence
                .as_ref()
                .and_then(|e| e.exit_status),
            Some(0)
        );
        let mut failing = request(&workspace);
        failing.verification_command = Some("exit 7".into());
        let accepted = service
            .submit_fixture(failing)
            .await
            .map_err(std::io::Error::other)?;
        let waiting = wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput).await?;
        service
            .answer_input(
                &accepted.turn_id,
                waiting
                    .pending_input_id
                    .as_deref()
                    .ok_or("verification approval missing")?,
                true,
            )
            .await?;
        let failed = wait_for(&service, &accepted.turn_id, TurnStatus::Failed)
            .await
            .map_err(std::io::Error::other)?;
        assert_eq!(failed.status, TurnStatus::Failed);
        assert_eq!(failed.verification, VerificationStatus::Failed);
        assert_eq!(
            failed
                .verification_evidence
                .as_ref()
                .and_then(|e| e.exit_status),
            Some(7)
        );
        Ok(())
    }

    #[tokio::test]
    async fn verification_denial_and_permit_cancellation_never_launch_a_command()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let service = ThreadService::new(
            app(vec![final_turn(), final_turn()])?,
            &[workspace.path().to_path_buf()],
        )?;
        let permits = Arc::clone(&service.inner.tool_workers)
            .acquire_many_owned(16)
            .await?;
        for approved in [false, true] {
            let mut submitted = request(&workspace);
            submitted.verification_command = Some("echo blocked > created".into());
            let accepted = service.submit_fixture(submitted).await?;
            let waiting =
                wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput).await?;
            let input = waiting
                .pending_input
                .as_ref()
                .ok_or("verification approval missing")?;
            assert!(input.arguments.contains("echo blocked > created"));
            service
                .answer_input(&accepted.turn_id, &input.request_id, approved)
                .await?;
            if approved {
                service.cancel(&accepted.turn_id).await?;
            }
            let finished = wait_for(
                &service,
                &accepted.turn_id,
                if approved {
                    TurnStatus::Cancelled
                } else {
                    TurnStatus::Failed
                },
            )
            .await?;
            assert_eq!(
                finished.verification,
                if approved {
                    VerificationStatus::Unavailable
                } else {
                    VerificationStatus::Denied
                }
            );
            assert!(!finished.unknown_effect);
            assert!(!workspace.path().join("created").exists());
            let stored = service
                .inner
                .store
                .load(&accepted.thread_id)
                .await?
                .ok_or("execution missing")?;
            assert!(!stored.records.iter().any(|record| matches!(turn_fact(record), ExecutionRecord::ToolIntent { call, .. } if call.origin == CallOrigin::Verification)));
            assert!(stored.records.iter().any(|record| matches!(
                turn_fact(record),
                ExecutionRecord::VerificationResult {
                    effect: EffectStatus::NotExecuted,
                    ..
                }
            )));
        }
        drop(permits);
        service.shutdown().await;
        Ok(())
    }

    struct FailingStore {
        memory: MemoryExecutionStore,
        failure: &'static str,
    }

    #[async_trait::async_trait]
    impl ExecutionStore for FailingStore {
        async fn read_index(
            &self,
            after: u64,
            cutoff: Option<u64>,
            limit: usize,
            max_bytes: usize,
        ) -> Result<crate::store::ExecutionIndexPage, String> {
            self.memory
                .read_index(after, cutoff, limit, max_bytes)
                .await
        }

        async fn claim_owner(&self, id: &str) -> Result<crate::store::OwnerClaim, String> {
            self.memory.claim_owner(id).await
        }
        async fn read_owner(
            &self,
            id: &str,
        ) -> Result<Option<crate::store::ExecutionOwner>, String> {
            self.memory.read_owner(id).await
        }
        async fn stop_owner(
            &self,
            owner: &crate::store::ExecutionOwner,
        ) -> Result<crate::store::ExecutionOwner, String> {
            self.memory.stop_owner(owner).await
        }

        async fn commit(
            &self,
            id: &str,
            version: u64,
            records: &[ExecutionRecord],
        ) -> Result<u64, String> {
            self.memory.commit(id, version, records).await
        }

        async fn read_records(
            &self,
            id: &str,
            after: u64,
            cutoff: Option<u64>,
            limit: usize,
            max_bytes: usize,
        ) -> Result<Option<crate::store::ExecutionPage>, String> {
            self.memory
                .read_records(id, after, cutoff, limit, max_bytes)
                .await
        }

        async fn thread_history(
            &self,
            id: &str,
            after: u64,
            cutoff: u64,
            limit: usize,
            max_bytes: usize,
        ) -> Result<crate::store::ThreadHistoryChunk, String> {
            self.memory
                .thread_history(id, after, cutoff, limit, max_bytes)
                .await
        }

        async fn commit_owned(
            &self,
            owner: &crate::store::ExecutionOwner,
            id: &str,
            version: u64,
            records: &[ExecutionRecord],
        ) -> Result<u64, String> {
            let fails = records.iter().any(|record| match turn_fact(record) {
                ExecutionRecord::ThreadCreated { .. } => self.failure == "accepted",
                ExecutionRecord::ModelResponse { .. } => self.failure == "response",
                ExecutionRecord::ToolIntent { .. } => self.failure == "intent",
                ExecutionRecord::ToolResult { .. } => self.failure == "result",
                ExecutionRecord::WorkspaceReleasePrepared { .. } => self.failure == "release",
                _ => false,
            });
            if fails {
                return Err("injected execution commit failure".into());
            }
            self.memory.commit_owned(owner, id, version, records).await
        }
        async fn load(&self, id: &str) -> Result<Option<crate::store::StoredExecution>, String> {
            self.memory.load(id).await
        }
        async fn find_key(
            &self,
            scope: &str,
            key: &str,
        ) -> Result<Option<crate::store::AcceptedKey>, String> {
            self.memory.find_key(scope, key).await
        }
    }

    #[tokio::test]
    async fn commit_failures_block_effects_and_preserve_uncertain_execution()
    -> Result<(), Box<dyn std::error::Error>> {
        for failure in ["accepted", "response", "intent", "result"] {
            let workspace = TempDir::new()?;
            let store = Arc::new(FailingStore {
                memory: MemoryExecutionStore::default(),
                failure,
            });
            let service = ThreadService::with_store(
                app(vec![
                    turn(vec![
                        tool_call(
                            "first",
                            "write",
                            serde_json::json!({"path":"first.txt","content":"one"}),
                        ),
                        tool_call(
                            "second",
                            "write",
                            serde_json::json!({"path":"second.txt","content":"two"}),
                        ),
                    ]),
                    final_turn(),
                ])?,
                &[workspace.path().to_path_buf()],
                store.clone(),
            )?;
            let accepted = service.submit_fixture(request(&workspace)).await;
            if failure == "accepted" {
                assert_eq!(
                    accepted.err().map(|error| error.code),
                    Some(ErrorCode::StorageUnavailable)
                );
                assert!(!workspace.path().join("first.txt").exists());
                service.shutdown().await;
                assert!(
                    store
                        .memory
                        .read_owner(&service.inner.instance_id)
                        .await?
                        .ok_or("owner missing")?
                        .stopped_at_ms
                        .is_none()
                );
                continue;
            }
            let accepted = accepted?;
            if failure != "response" {
                let approval =
                    wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput).await?;
                let request_id = approval
                    .pending_input_id
                    .ok_or("approval identity missing")?;
                service
                    .answer_input(&accepted.turn_id, &request_id, true)
                    .await?;
            }
            let blocked =
                wait_for(&service, &accepted.turn_id, TurnStatus::RecoveryRequired).await?;
            assert!(blocked.unknown_effect);
            service.shutdown().await;
            assert!(
                store
                    .memory
                    .read_owner(&service.inner.instance_id)
                    .await?
                    .ok_or("owner missing")?
                    .stopped_at_ms
                    .is_none()
            );
            assert_eq!(
                workspace.path().join("first.txt").exists(),
                failure == "result"
            );
            assert!(!workspace.path().join("second.txt").exists());
            let saved = store
                .load(&accepted.thread_id)
                .await?
                .ok_or("execution missing")?;
            assert!(
                !saved
                    .records
                    .iter()
                    .any(|record| matches!(turn_fact(record), ExecutionRecord::ToolResult { .. }))
            );
            if failure == "result" {
                assert!(
                    saved.records.iter().any(|record| matches!(
                        turn_fact(record),
                        ExecutionRecord::ToolIntent { .. }
                    ))
                );
            }
            assert!(
                service
                    .lock_state()
                    .active_workspaces
                    .contains_key(&blocked.workspace)
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn release_preparation_failure_retains_exclusion_without_publishing_completion()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let store = Arc::new(FailingStore {
            memory: MemoryExecutionStore::default(),
            failure: "release",
        });
        let service = ThreadService::with_store(
            app(vec![
                turn(vec![tool_call(
                    "write",
                    "write",
                    serde_json::json!({"path":"known.txt", "content":"one"}),
                )]),
                final_turn(),
            ])?,
            &[workspace.path().to_path_buf()],
            store.clone(),
        )?;
        let accepted = service.submit_fixture(request(&workspace)).await?;
        let waiting = wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput).await?;
        service
            .answer_input(
                &accepted.turn_id,
                waiting
                    .pending_input_id
                    .as_deref()
                    .ok_or("approval missing")?,
                true,
            )
            .await?;
        assert_eq!(
            wait_for(&service, &accepted.turn_id, TurnStatus::RecoveryRequired)
                .await?
                .status,
            TurnStatus::RecoveryRequired
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("known.txt"))?,
            "one"
        );
        let saved = store
            .load(&accepted.thread_id)
            .await?
            .ok_or("execution missing")?;
        assert!(saved.records.iter().any(|r| matches!(
            turn_fact(r),
            ExecutionRecord::ToolResult {
                effect: EffectStatus::Completed,
                ..
            }
        )));
        assert!(!saved.records.iter().any(|r| matches!(
            turn_fact(r),
            ExecutionRecord::TurnLifecycle {
                lifecycle: crate::thread::TurnLifecycle::Finished { .. },
                ..
            }
        )));
        service.shutdown().await;
        assert!(
            store
                .read_owner(&service.inner.instance_id)
                .await?
                .ok_or("owner missing")?
                .stopped_at_ms
                .is_none()
        );
        drop(service);
        let peer = ThreadService::new(app(vec![final_turn()])?, &[workspace.path().to_path_buf()])?;
        assert_eq!(
            peer.submit_fixture(request(&workspace))
                .await
                .err()
                .ok_or("unknown release was bypassed")?
                .code,
            ErrorCode::RecoveryRequired
        );
        peer.shutdown().await;
        Ok(())
    }
}
