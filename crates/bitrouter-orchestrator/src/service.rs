//! Server-owned agent runtime with transactional execution facts. State, execution and observation share one
//! authority; disconnecting an observer never cancels its task.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bitrouter_sdk::App;
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::{Message, ToolResultOutput, Usage};
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
mod workspace;

const MAX_EVENT_PAGE: usize = 1000;
const MAX_LIVE_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
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

impl TaskStatus {
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
pub enum TaskEventPayload {
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
    TaskStarted,
    AssistantStarted {
        step_id: String,
        item_id: String,
    },
    ModelTurn {
        #[serde(default)]
        step_id: String,
        #[serde(default)]
        item_id: String,
        request_id: String,
        requested_model: String,
        usage: Option<Usage>,
    },
    AssistantMessage {
        #[serde(default)]
        item_id: String,
        message: Message,
    },
    AssistantInterrupted {
        item_id: String,
        partial: Message,
        detail: String,
    },
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
    ToolStarted {
        #[serde(default)]
        origin: crate::store::CallOrigin,
        id: String,
        name: String,
    },
    ToolFinished {
        #[serde(default)]
        origin: crate::store::CallOrigin,
        id: String,
        name: String,
        output: ToolResultOutput,
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
    TaskFinished {
        status: TaskStatus,
        detail: String,
        final_answer: Option<String>,
        verification: VerificationStatus,
        verification_evidence: Option<VerificationEvidence>,
        unknown_effect: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEvent {
    #[serde(default)]
    pub thread_id: Option<String>,
    pub server_instance_id: String,
    pub task_id: String,
    pub seq: u64,
    pub timestamp_ms: u64,
    pub payload: TaskEventPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSnapshot {
    #[serde(default)]
    pub steering: Vec<crate::thread::SteeringReceipt>,
    #[serde(default)]
    pub thread_id: Option<String>,
    pub server_instance_id: String,
    pub model: String,
    pub task_id: String,
    pub status: TaskStatus,
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
    pub tools_per_task: usize,
    pub global_tools: usize,
    pub active_tasks: usize,
    pub retained_tasks: usize,
    pub retained_bytes: usize,
    pub retention_seconds: u64,
    pub events_per_task: usize,
    pub event_bytes_per_task: usize,
    pub subscribers_per_task: usize,
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
            tools_per_task: 4,
            global_tools: 16,
            active_tasks: 8,
            retained_tasks: 32,
            retained_bytes: 64 * 1024 * 1024,
            retention_seconds: 1800,
            events_per_task: 256,
            event_bytes_per_task: 2 * 1024 * 1024,
            subscribers_per_task: 8,
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
    UnknownTask,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Observation {
    Snapshot {
        snapshot: Box<TaskSnapshot>,
        resynchronized: bool,
        catchup: Vec<TaskEvent>,
    },
    Event {
        event: Box<TaskEvent>,
    },
}

/// A subscription holds no execution authority. Dropping it only detaches.
pub struct TaskSubscription {
    service: TaskService,
    task_id: String,
    receiver: broadcast::Receiver<Observation>,
    initial: Option<Observation>,
    finished: bool,
}

impl TaskSubscription {
    pub async fn next(&mut self) -> Result<Option<Observation>, ServiceError> {
        if let Some(initial) = self.initial.take() {
            return Ok(Some(initial));
        }
        if self.finished {
            return Ok(None);
        }
        match self.receiver.recv().await {
            Ok(observation) => {
                self.finished = match &observation {
                    Observation::Event { event } => {
                        matches!(event.payload, TaskEventPayload::TaskFinished { .. })
                    }
                    Observation::Snapshot { snapshot, .. } => {
                        snapshot.status.terminal()
                            || snapshot.status == TaskStatus::RecoveryRequired
                    }
                };
                Ok(Some(observation))
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                let mut state = self.service.lock_state();
                let record = state
                    .tasks
                    .get_mut(&self.task_id)
                    .ok_or_else(unknown_task)?;
                // Registration and snapshot cutoff are atomic with publication.
                self.receiver = record.publisher.subscribe();
                self.finished = record.snapshot.status.terminal()
                    || record.snapshot.status == TaskStatus::RecoveryRequired;
                Ok(Some(Observation::Snapshot {
                    snapshot: Box::new(record.snapshot.clone()),
                    resynchronized: true,
                    catchup: Vec::new(),
                }))
            }
            Err(broadcast::error::RecvError::Closed) => Ok(None),
        }
    }
}

fn unknown_task() -> ServiceError {
    ServiceError::new(
        ErrorCode::UnknownTask,
        "task is unknown or expired in this server instance",
    )
}

pub struct TaskRequest {
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

struct TaskRecord {
    fence: Arc<crate::control::LaunchFence>,
    steering: Vec<steering::SteeringInput>,
    verification_budget: Option<(u64, u32)>,
    thread_id: Option<String>,
    permission_profile: PermissionProfile,
    settled: Option<(Vec<Message>, u64)>,
    snapshot: TaskSnapshot,
    events: VecDeque<TaskEvent>,
    event_bytes: usize,
    publisher: broadcast::Sender<Observation>,
    terminal_at: Option<Instant>,
    cancel: CancellationToken,
    pending: Option<PendingInput>,
    commit_lock: Arc<tokio::sync::Mutex<()>>,
    store_version: u64,
    storage_error: Option<String>,
}

struct State {
    startup_discovery: Option<startup::StartupDiscovery>,
    cold_executions: HashMap<String, startup::ColdExecution>,
    workspace_fences: HashMap<PathBuf, Arc<workspace::WorkspaceFence>>,
    execution_ownership: Option<crate::store::OwnerClaim>,
    threads: HashMap<String, threads::ThreadRecord>,
    ready_threads: VecDeque<String>,
    workspace_profiles: HashMap<PathBuf, Vec<PermissionProfile>>,
    tasks: HashMap<String, TaskRecord>,
    active_workspaces: HashMap<PathBuf, String>,
    allowed_workspaces: Vec<PathBuf>,
    idempotency: HashMap<(String, String, String), (String, String)>,
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
pub struct TaskService {
    inner: Arc<Inner>,
}

impl TaskService {
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
            || limits.active_tasks == 0
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
            || limits.tools_per_task == 0
            || limits.global_tools == 0
            || limits.retained_tasks == 0
            || limits.events_per_task == 0
            || limits.subscriber_queue == 0
            || limits.subscribers_per_task == 0
            || limits.event_bytes_per_task < limits.request_bytes
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
                    tasks: HashMap::new(),
                    active_workspaces: HashMap::new(),
                    allowed_workspaces,
                    idempotency: HashMap::new(),
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
            for record in state.tasks.values() {
                record.cancel.cancel();
            }
            // Spawn and close are serialized under the same state lock.
            self.inner.workers.close();
        }
        self.inner.workers.wait().await;
        self.pause_queues_after_shutdown().await;
        self.stop_execution_owner().await;
    }

    pub fn observe(
        &self,
        task_id: &str,
        after: Option<u64>,
    ) -> Result<TaskSubscription, ServiceError> {
        let mut state = self.lock_state();
        self.prune(&mut state);
        let record = state.tasks.get(task_id).ok_or_else(unknown_task)?;
        if record.publisher.receiver_count() >= self.inner.limits.subscribers_per_task {
            return Err(ServiceError::new(
                ErrorCode::Overloaded,
                "too many task observers",
            ));
        }
        if after.is_some_and(|cursor| cursor > record.snapshot.cursor) {
            return Err("cursor is ahead of task".into());
        }
        let resynchronized = after.is_some_and(|cursor| {
            record
                .events
                .front()
                .map_or(cursor < record.snapshot.cursor, |event| {
                    cursor < event.seq.saturating_sub(1)
                })
        });
        let catchup = match after {
            Some(cursor) if !resynchronized => record
                .events
                .iter()
                .filter(|event| event.seq > cursor)
                .cloned()
                .collect(),
            _ => Vec::new(),
        };
        Ok(TaskSubscription {
            service: self.clone(),
            task_id: task_id.into(),
            receiver: record.publisher.subscribe(),
            initial: Some(Observation::Snapshot {
                snapshot: Box::new(record.snapshot.clone()),
                resynchronized,
                catchup,
            }),
            finished: record.snapshot.status.terminal()
                || record.snapshot.status == TaskStatus::RecoveryRequired,
        })
    }

    fn prune(&self, state: &mut State) {
        let mut terminal: Vec<_> = state
            .tasks
            .iter()
            .filter_map(|(id, record)| {
                record.terminal_at.map(|at| {
                    let bytes = serde_json::to_vec(&record.snapshot)
                        .map_or(usize::MAX, |encoded| encoded.len());
                    (id.clone(), at, record.event_bytes.saturating_add(bytes))
                })
            })
            .collect();
        terminal.sort_by_key(|(_, at, _)| *at);
        let excess = terminal
            .len()
            .saturating_sub(self.inner.limits.retained_tasks);
        let mut retained_bytes = terminal
            .iter()
            .fold(0_usize, |total, (_, _, bytes)| total.saturating_add(*bytes));
        for (index, (id, at, bytes)) in terminal.into_iter().enumerate() {
            if index < excess
                || retained_bytes > self.inner.limits.retained_bytes
                || at.elapsed() >= Duration::from_secs(self.inner.limits.retention_seconds)
            {
                state.tasks.remove(&id);
                state.idempotency.retain(|_, (_, task)| task != &id);
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

    pub async fn submit(&self, request: TaskRequest) -> Result<TaskSnapshot, ServiceError> {
        let _admission = self.inner.admission.lock().await;
        if request.prompt.len()
            + request.config.instructions.len()
            + request.config.model.len()
            + request.verification_command.as_ref().map_or(0, String::len)
            > self.inner.limits.request_bytes
        {
            return Err("task request is too large".into());
        }
        if request.config.max_steps > 256
            || request.config.max_tool_calls > 1024
            || request.config.max_context_bytes > 2 * 1024 * 1024
            || request.config.max_duration > Duration::from_secs(86400)
        {
            return Err("agent bounds exceed runtime limits".into());
        }
        let key = request.idempotency_key.as_ref().map(|key| {
            (
                request.caller.api_key_id().to_string(),
                request.caller.user_id().to_string(),
                key.clone(),
            )
        });
        if key
            .as_ref()
            .is_some_and(|(_, _, key)| key.is_empty() || key.len() > 128)
        {
            return Err("invalid idempotency key".into());
        }
        let tool_mode = request.config.tool_mode();
        if tool_mode == ToolMode::ReadOnly && request.verification_command.is_some() {
            return Err("read-only tasks cannot run a verification command".into());
        }
        let workspace = request
            .workspace
            .canonicalize()
            .map_err(|error| error.to_string())?;
        if !self.lock_state().allowed_workspaces.contains(&workspace) {
            return Err("workspace is not registered with this server".into());
        }
        let fingerprint = serde_json::to_string(&(
            &request.prompt,
            &workspace,
            &request.config.model,
            &request.config.effort,
            tool_mode,
            &request.verification_command,
            &request.config.instructions,
            request.config.max_steps,
            request.config.max_tool_calls,
            request.config.max_duration.as_millis(),
            request.config.max_context_bytes,
            request.config.max_spend_microusd,
            request
                .config
                .estimate_rates
                .as_ref()
                .map(|rates| (rates.prompt, rates.completion)),
        ))
        .map_err(|error| error.to_string())?;
        let owner_key_id = request.caller.api_key_id().to_string();
        let owner_user_id = request.caller.user_id().to_string();
        let stored_config = request.config.clone();
        let agent = Agent::new(
            Arc::clone(&self.inner.app),
            request.caller,
            &workspace,
            request.config,
        )?
        .with_tool_workers(
            Arc::clone(&self.inner.tool_workers),
            self.inner.limits.tools_per_task,
        );
        let task_id = uuid::Uuid::new_v4().to_string();
        let cancel = CancellationToken::new();
        {
            let mut state = self.lock_state();
            self.prune(&mut state);
            if state.closing {
                return Err(ServiceError::new(
                    ErrorCode::ShuttingDown,
                    "runtime is shutting down",
                ));
            }
            if let Some(key) = key.as_ref()
                && let Some((existing_fingerprint, existing_id)) = state.idempotency.get(key)
            {
                if existing_fingerprint != &fingerprint {
                    return Err(ServiceError::new(
                        ErrorCode::Conflict,
                        "idempotency key already belongs to a different task",
                    ));
                }
                return state
                    .tasks
                    .get(existing_id)
                    .map(|record| record.snapshot.clone())
                    .ok_or_else(|| "idempotent task disappeared".into());
            }
            if state.active_workspaces.len() >= self.inner.limits.active_tasks {
                return Err(ServiceError::new(
                    ErrorCode::Overloaded,
                    "active task limit reached",
                ));
            }
            if let Some(error) = self.workspace_owner_error(&state, &workspace) {
                return Err(error);
            }
        }
        let user_item_id = uuid::Uuid::new_v4().to_string();
        self.initialize_execution().await?;
        self.reserve_workspace(&workspace, &task_id).await?;
        let event = TaskEvent {
            thread_id: None,
            server_instance_id: self.inner.instance_id.clone(),
            task_id: task_id.clone(),
            seq: 1,
            timestamp_ms: now_ms(),
            payload: TaskEventPayload::Accepted {
                user_item_id: user_item_id.clone(),
                prompt: request.prompt.clone(),
                workspace: workspace.clone(),
                model: agent.model().into(),
                tool_mode,
                idempotency_key: request.idempotency_key.clone(),
                request_fingerprint: request
                    .idempotency_key
                    .as_ref()
                    .map(|_| fingerprint.clone()),
            },
        };
        let store_version = self
            .commit_fenced(
                &task_id,
                0,
                &[ExecutionRecord::Accepted {
                    owner_key_id,
                    owner_user_id,
                    fingerprint: fingerprint.clone(),
                    config: Box::new(stored_config),
                    verification_command: request.verification_command.clone(),
                    event,
                }],
            )
            .await
            .map_err(|error| ServiceError::new(ErrorCode::StorageUnavailable, error))?;
        let accepted = {
            let mut state = self.lock_state();
            let snapshot = TaskSnapshot {
                steering: Vec::new(),
                thread_id: None,
                server_instance_id: self.inner.instance_id.clone(),
                model: agent.model().to_string(),
                task_id: task_id.clone(),
                status: TaskStatus::Accepted,
                cursor: 0,
                workspace: workspace.clone(),
                tool_mode,
                final_answer: None,
                detail: None,
                unknown_effect: false,
                verification: VerificationStatus::Unavailable,
                verification_evidence: None,
                pending_input_id: None,
                pending_input: None,
                live: None,
            };
            state.tasks.insert(
                task_id.clone(),
                TaskRecord {
                    fence: Arc::new(crate::control::LaunchFence::default()),
                    steering: Vec::new(),
                    verification_budget: None,
                    thread_id: None,
                    permission_profile: if tool_mode == ToolMode::ReadOnly {
                        PermissionProfile::ReadOnly
                    } else {
                        PermissionProfile::Ask
                    },
                    settled: None,
                    snapshot,
                    events: VecDeque::new(),
                    event_bytes: 0,
                    publisher: broadcast::channel(self.inner.limits.subscriber_queue).0,
                    terminal_at: None,
                    cancel: cancel.clone(),
                    pending: None,
                    commit_lock: Arc::new(tokio::sync::Mutex::new(())),
                    store_version,
                    storage_error: None,
                },
            );
            if let Err(error) = self.append_locked(
                &mut state,
                &task_id,
                TaskEventPayload::Accepted {
                    user_item_id: user_item_id.clone(),
                    prompt: request.prompt.clone(),
                    workspace: workspace.clone(),
                    model: agent.model().to_string(),
                    tool_mode,
                    idempotency_key: request.idempotency_key.clone(),
                    request_fingerprint: request
                        .idempotency_key
                        .as_ref()
                        .map(|_| fingerprint.clone()),
                },
            ) {
                state.tasks.remove(&task_id);
                return Err(error);
            }
            state.active_workspaces.insert(workspace, task_id.clone());
            if let Some(key) = key.as_ref() {
                state
                    .idempotency
                    .insert(key.clone(), (fingerprint, task_id.clone()));
            }
            state
                .tasks
                .get(&task_id)
                .map(|record| record.snapshot.clone())
                .ok_or_else(|| "accepted task disappeared".to_string())?
        };
        let service = self.clone();
        let prompt = RunInput {
            prompt: request.prompt,
            user_item_id,
            messages: Vec::new(),
            context_version: 0,
            checkpoint: None,
            complete_checkpoint: false,
            restored_verification: None,
        };
        let verification_command = request.verification_command;
        let workspace_for_worker = accepted.workspace.clone();
        let worker_id = task_id.clone();
        self.inner.workers.spawn(async move {
            service
                .run_task(
                    worker_id,
                    agent,
                    prompt,
                    verification_command,
                    workspace_for_worker,
                    cancel,
                )
                .await;
        });
        Ok(accepted)
    }

    pub fn read(&self, task_id: &str) -> Result<TaskSnapshot, ServiceError> {
        let mut state = self.lock_state();
        self.prune(&mut state);
        state
            .tasks
            .get(task_id)
            .map(|record| record.snapshot.clone())
            .ok_or_else(unknown_task)
    }

    pub fn events_after(&self, task_id: &str, after: u64) -> Result<Vec<TaskEvent>, ServiceError> {
        let mut state = self.lock_state();
        self.prune(&mut state);
        let record = state.tasks.get(task_id).ok_or_else(unknown_task)?;
        if after > record.snapshot.cursor {
            return Err("cursor is ahead of task".into());
        }
        if record
            .events
            .front()
            .map_or(after < record.snapshot.cursor, |event| {
                after < event.seq.saturating_sub(1)
            })
        {
            return Err(ServiceError::new(
                ErrorCode::ResyncRequired,
                "event cursor expired; observe a fresh snapshot",
            ));
        }
        Ok(record
            .events
            .iter()
            .filter(|event| event.seq > after)
            .take(MAX_EVENT_PAGE)
            .cloned()
            .collect())
    }

    pub async fn answer_input(
        &self,
        task_id: &str,
        request_id: &str,
        approved: bool,
    ) -> Result<(), ServiceError> {
        let gate = self.commit_gate(task_id)?;
        let _guard = gate.lock().await;
        self.answer_input_serialized(task_id, request_id, approved, &[])
            .await
    }

    async fn answer_input_serialized(
        &self,
        task_id: &str,
        request_id: &str,
        approved: bool,
        extra_facts: &[ExecutionRecord],
    ) -> Result<(), ServiceError> {
        {
            let state = self.lock_state();
            let record = state.tasks.get(task_id).ok_or_else(unknown_task)?;
            if record.snapshot.status != TaskStatus::WaitingForInput
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
            task_id,
            TaskEventPayload::InputResolved {
                request_id: request_id.into(),
                approved,
            },
            extra_facts,
        )
        .await?;
        let sender = self
            .lock_state()
            .tasks
            .get_mut(task_id)
            .and_then(|record| record.pending.take())
            .ok_or_else(|| "pending input channel is unavailable".to_string())?
            .response;
        sender.send(approved).map_err(|_| {
            ServiceError::new(ErrorCode::Conflict, "task stopped before accepting input")
        })
    }

    pub async fn cancel(&self, task_id: &str) -> Result<(), ServiceError> {
        let gate = self.commit_gate(task_id)?;
        let _guard = gate.lock().await;
        let token = {
            let state = self.lock_state();
            let record = state.tasks.get(task_id).ok_or_else(unknown_task)?;
            if record.snapshot.status.terminal() || record.cancel.is_cancelled() {
                return Ok(());
            }
            if record.snapshot.status == TaskStatus::Queued {
                return Err(ServiceError::new(
                    ErrorCode::Conflict,
                    "use targeted queued withdrawal for an unstarted Turn",
                ));
            }
            record.cancel.clone()
        };
        self.append_serialized(task_id, TaskEventPayload::CancelRequested)
            .await?;
        token.cancel();
        let pending = self
            .lock_state()
            .tasks
            .get(task_id)
            .and_then(|record| record.snapshot.pending_input_id.clone());
        if let Some(request_id) = pending {
            self.append_serialized(
                task_id,
                TaskEventPayload::InputResolved {
                    request_id,
                    approved: false,
                },
            )
            .await?;
            if let Some(pending) = self
                .lock_state()
                .tasks
                .get_mut(task_id)
                .and_then(|record| record.pending.take())
            {
                let _ = pending.response.send(false);
            }
        }
        Ok(())
    }

    fn run_task(
        &self,
        task_id: String,
        agent: Agent,
        prompt: RunInput,
        verification_command: Option<String>,
        workspace: PathBuf,
        cancel: CancellationToken,
    ) -> futures::future::BoxFuture<'static, ()> {
        let service = self.clone();
        Box::pin(async move {
            service
                .run_task_inner(
                    task_id,
                    agent,
                    prompt,
                    verification_command,
                    workspace,
                    cancel,
                )
                .await
        })
    }

    async fn run_task_inner(
        &self,
        task_id: String,
        agent: Agent,
        prompt: RunInput,
        verification_command: Option<String>,
        workspace: PathBuf,
        cancel: CancellationToken,
    ) {
        if self
            .append(&task_id, TaskEventPayload::TaskStarted)
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
            let controls = self.lock_state().tasks.get(&task_id).and_then(|task| {
                task.thread_id
                    .as_ref()
                    .map(|_| crate::control::TurnControl {
                        fence: Arc::clone(&task.fence),
                        models: model_tx,
                    })
            });
            let run_cancel = cancel.clone();
            let profile = self
                .lock_state()
                .tasks
                .get(&task_id)
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
                        let result = self.prepare_model(&task_id, &mut request).await;
                        let _ = request.response.send(result);
                    }
                    Some(request) = commit_rx.recv() => {
                        let result = self.commit_records(&task_id, &request.records).await.map_err(|error| error.to_string());
                        let failed = result.is_err();
                        let _ = request.response.send(result);
                        if failed { cancel.cancel(); }
                    }
                    Some(event) = event_rx.recv() => {
                        if self.append_agent_event(&task_id, event).await.is_err() {
                            break None;
                        }
                    }
                    Some(request) = approval_rx.recv() => {
                        // Agent control events precede its approval handoff. Drain
                        // that finite prefix before publishing the input request.
                        while let Ok(event) = event_rx.try_recv() {
                            if self.append_agent_event(&task_id, event).await.is_err() {
                                cancel.cancel();
                            }
                        }
                        if self.request_approval(&task_id, request).await.is_err() {
                            break None;
                        }
                    }
                    result = &mut run => match result {
                        Ok(report) => break Some(report),
                        Err(error) => {
                            cancel.cancel();
                            let _ = self.append(&task_id, TaskEventPayload::TaskFinished {
                                status: TaskStatus::Interrupted, detail: format!("agent execution lost: {error}; effects may have occurred"),
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
                        &task_id,
                        TaskEventPayload::TaskFinished {
                            status: TaskStatus::Interrupted,
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
                    if self.append_agent_event(&task_id, event).await.is_err() {
                        return;
                    }
                }
                let mut status = match report.status {
                    RunStatus::Completed => TaskStatus::Completed,
                    RunStatus::Cancelled => TaskStatus::Cancelled,
                    RunStatus::Failed | RunStatus::BoundExceeded => TaskStatus::Failed,
                };
                if cancel.is_cancelled() {
                    status = TaskStatus::Cancelled;
                }
                if report.unknown_effect {
                    status = TaskStatus::RecoveryRequired;
                }
                let mut unknown_effect = report.unknown_effect;
                let (verification, evidence) = if status == TaskStatus::Completed {
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
                                .run_verification(&task_id, &workspace, command, &cancel, budget)
                                .await
                            {
                                Ok(result) => result,
                                Err(_) => return,
                            };
                            unknown_effect |= uncertain;
                            if verification != VerificationStatus::Passed {
                                status = if uncertain {
                                    TaskStatus::RecoveryRequired
                                } else if cancel.is_cancelled() {
                                    TaskStatus::Cancelled
                                } else {
                                    TaskStatus::Failed
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
                if status == TaskStatus::Completed
                    && !matches!(
                        verification,
                        VerificationStatus::Passed | VerificationStatus::NotRequested
                    )
                {
                    status = if unknown_effect {
                        TaskStatus::RecoveryRequired
                    } else {
                        TaskStatus::Failed
                    };
                }
                let gate = match self.commit_gate(&task_id) {
                    Ok(gate) => gate,
                    Err(_) => return,
                };
                let guard = gate.lock().await;
                let pending = self
                    .lock_state()
                    .tasks
                    .get(&task_id)
                    .is_some_and(|task| task.fence.pending());
                if pending
                    && report.status == RunStatus::Completed
                    && !unknown_effect
                    && !cancel.is_cancelled()
                {
                    if let Some((duration, calls)) = self
                        .lock_state()
                        .tasks
                        .get(&task_id)
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
                        &task_id,
                        TaskEventPayload::TaskFinished {
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
        task_id: &str,
        workspace: &Path,
        command: String,
        cancel: &CancellationToken,
        budget: VerificationBudget,
    ) -> Result<(VerificationStatus, VerificationEvidence, bool), ServiceError> {
        let fence = self
            .lock_state()
            .tasks
            .get(task_id)
            .map(|task| Arc::clone(&task.fence))
            .ok_or_else(unknown_task)?;
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
                    self.lock_state().tasks.get(task_id).is_some_and(|task| {
                        task.permission_profile == PermissionProfile::AllowEffects
                    });
                let approved = if allow_effects {
                    true
                } else {
                    let (response, receiver) = oneshot::channel();
                    let wait_started = Instant::now();
                    self.request_approval(
                        task_id,
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
                                task_id,
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
                            if let Err(error) = self
                                .append(
                                    task_id,
                                    TaskEventPayload::ToolStarted {
                                        id: call.item_id.clone(),
                                        name: call.name.clone(),
                                        origin: CallOrigin::Verification,
                                    },
                                )
                                .await
                            {
                                drop(start);
                                let _ = run.await;
                                return Err(error);
                            }
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
                                    Some(event) = receiver.recv() => if storage_error.is_none() && let Err(error) = self.append_agent_event(task_id, event).await { storage_error = Some(error); worker_cancel.cancel(); },
                                }
                            };
                            while let Ok(event) = receiver.try_recv() {
                                if storage_error.is_none()
                                    && let Err(error) =
                                        self.append_agent_event(task_id, event).await
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
            task_id,
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
        self.append(
            task_id,
            TaskEventPayload::ToolFinished {
                id: call.item_id,
                name: call.name,
                output,
                origin: CallOrigin::Verification,
            },
        )
        .await?;
        Ok((verification, evidence, effect == EffectStatus::Unknown))
    }

    async fn request_approval(
        &self,
        task_id: &str,
        request: ApprovalRequest,
    ) -> Result<(), ServiceError> {
        let gate = self.commit_gate(task_id)?;
        let _guard = gate.lock().await;
        {
            let mut state = self.lock_state();
            let record = state.tasks.get_mut(task_id).ok_or_else(unknown_task)?;
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
            task_id,
            TaskEventPayload::InputRequested {
                request_id: request.id,
                tool_id: request.tool_id,
                tool_name: request.tool_name,
                arguments: request.arguments,
            },
        )
        .await
    }

    async fn append_agent_event(&self, task_id: &str, event: RunEvent) -> Result<(), ServiceError> {
        let payload = match event {
            RunEvent::UserMessage { .. } | RunEvent::Finished { .. } => return Ok(()),
            RunEvent::AssistantStarted { step_id, item_id } => {
                TaskEventPayload::AssistantStarted { step_id, item_id }
            }
            RunEvent::AssistantDelta { item_id, text } => {
                TaskEventPayload::AssistantDelta { item_id, text }
            }
            RunEvent::AssistantInterrupted {
                item_id,
                partial,
                detail,
            } => TaskEventPayload::AssistantInterrupted {
                item_id,
                partial,
                detail,
            },
            RunEvent::ToolOutputDelta { id, source, text } => {
                TaskEventPayload::ToolOutputDelta { id, source, text }
            }
            RunEvent::ModelTurn {
                step_id,
                item_id,
                request_id,
                requested_model,
                usage,
            } => TaskEventPayload::ModelTurn {
                step_id,
                item_id,
                request_id,
                requested_model,
                usage,
            },
            RunEvent::AssistantMessage { item_id, message } => {
                TaskEventPayload::AssistantMessage { item_id, message }
            }
            RunEvent::ToolStarted { id, name } => TaskEventPayload::ToolStarted {
                id,
                name,
                origin: crate::store::CallOrigin::Model,
            },
            RunEvent::ToolFinished { id, name, output } => TaskEventPayload::ToolFinished {
                id,
                name,
                output,
                origin: crate::store::CallOrigin::Model,
            },
        };
        self.append(task_id, payload).await
    }

    fn commit_gate(&self, task_id: &str) -> Result<Arc<tokio::sync::Mutex<()>>, ServiceError> {
        let state = self.lock_state();
        let record = state.tasks.get(task_id).ok_or_else(unknown_task)?;
        match &record.thread_id {
            Some(thread_id) => state
                .threads
                .get(thread_id)
                .map(|thread| Arc::clone(&thread.commit_lock))
                .ok_or_else(unknown_task),
            None => Ok(Arc::clone(&record.commit_lock)),
        }
    }

    async fn commit_records(
        &self,
        task_id: &str,
        records: &[ExecutionRecord],
    ) -> Result<(), ServiceError> {
        let gate = self.commit_gate(task_id)?;
        let _guard = gate.lock().await;
        self.commit_serialized(task_id, records).await?;
        for record in records {
            if let ExecutionRecord::VerificationResult {
                active_duration_ms,
                tool_calls,
                ..
            } = record
                && let Some(task) = self.lock_state().tasks.get_mut(task_id)
            {
                task.verification_budget = Some((*active_duration_ms, *tool_calls));
            }
        }
        Ok(())
    }

    async fn commit_serialized(
        &self,
        task_id: &str,
        records: &[ExecutionRecord],
    ) -> Result<(), ServiceError> {
        let thread_id = self
            .lock_state()
            .tasks
            .get(task_id)
            .and_then(|record| record.thread_id.clone());
        if let Some(thread_id) = thread_id {
            return self
                .commit_turn_serialized(&thread_id, task_id, records)
                .await;
        }
        let version = {
            let state = self.lock_state();
            let record = state.tasks.get(task_id).ok_or_else(unknown_task)?;
            if let Some(error) = &record.storage_error {
                return Err(ServiceError::new(
                    ErrorCode::StorageUnavailable,
                    error.clone(),
                ));
            }
            record.store_version
        };
        match self.commit_fenced(task_id, version, records).await {
            Ok(version) => {
                self.lock_state()
                    .tasks
                    .get_mut(task_id)
                    .ok_or_else(unknown_task)?
                    .store_version = version;
                Ok(())
            }
            Err(error) => {
                if let Some(record) = self.lock_state().tasks.get_mut(task_id) {
                    record.cancel.cancel();
                    record.storage_error = Some(error.clone());
                    record.snapshot.status = TaskStatus::RecoveryRequired;
                    record.snapshot.unknown_effect = true;
                    record.snapshot.detail = Some(format!(
                        "execution storage failed; recovery required: {error}"
                    ));
                    // Storage failure cannot be represented by a committed
                    // event. Resynchronize observers to current blocked state
                    // without inventing a durable sequence or terminal result.
                    let _ = record.publisher.send(Observation::Snapshot {
                        snapshot: Box::new(record.snapshot.clone()),
                        resynchronized: true,
                        catchup: Vec::new(),
                    });
                }
                Err(ServiceError::new(ErrorCode::StorageUnavailable, error))
            }
        }
    }

    async fn append(&self, task_id: &str, payload: TaskEventPayload) -> Result<(), ServiceError> {
        let gate = self.commit_gate(task_id)?;
        let _guard = gate.lock().await;
        self.append_serialized(task_id, payload).await
    }

    async fn append_serialized(
        &self,
        task_id: &str,
        payload: TaskEventPayload,
    ) -> Result<(), ServiceError> {
        self.append_facts_serialized(task_id, payload, &[]).await
    }

    async fn append_facts_serialized(
        &self,
        task_id: &str,
        mut payload: TaskEventPayload,
        extra_facts: &[ExecutionRecord],
    ) -> Result<(), ServiceError> {
        if let TaskEventPayload::TaskFinished {
            status,
            unknown_effect: true,
            ..
        } = &mut payload
        {
            *status = TaskStatus::RecoveryRequired;
        }
        if matches!(
            payload,
            TaskEventPayload::AssistantDelta { .. } | TaskEventPayload::ToolOutputDelta { .. }
        ) {
            return self.append_locked(&mut self.lock_state(), task_id, payload);
        }
        if matches!(payload, TaskEventPayload::TaskFinished { status, .. } if status.terminal())
            && let Err(error) = self.prepare_workspace_finish(task_id).await
        {
            self.workspace_finish_failed(task_id, &error);
            return Err(error);
        }
        let events = {
            let state = self.lock_state();
            let record = state.tasks.get(task_id).ok_or_else(unknown_task)?;
            let mut payloads = Vec::new();
            if matches!(
                payload,
                TaskEventPayload::TaskFinished { .. }
                    | TaskEventPayload::CancelRequested
                    | TaskEventPayload::SteeringUpdated { .. }
            ) && let Some(request_id) = record.snapshot.pending_input_id.clone()
            {
                payloads.push(TaskEventPayload::InputResolved {
                    request_id,
                    approved: false,
                });
            }
            if let TaskEventPayload::TaskFinished { detail, .. } = &payload {
                for entry in &record.steering {
                    if entry.receipt.status == crate::thread::SteeringStatus::Received {
                        let mut receipt = entry.receipt.clone();
                        receipt.status = crate::thread::SteeringStatus::NotApplied;
                        receipt.reason = Some(detail.clone());
                        payloads.push(TaskEventPayload::SteeringUpdated {
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
                .map(|(offset, payload)| TaskEvent {
                    thread_id: record.thread_id.clone(),
                    server_instance_id: self.inner.instance_id.clone(),
                    task_id: task_id.into(),
                    seq: record.snapshot.cursor + offset as u64 + 1,
                    timestamp_ms: now_ms(),
                    payload,
                })
                .collect::<Vec<_>>()
        };
        let mut facts = events
            .iter()
            .cloned()
            .map(|event| ExecutionRecord::Event { event })
            .collect::<Vec<_>>();
        facts.extend_from_slice(extra_facts);
        for event in &events {
            if let TaskEventPayload::SteeringUpdated {
                receipt,
                text: None,
            } = &event.payload
            {
                facts.push(ExecutionRecord::SteeringResolved {
                    receipt: receipt.clone(),
                });
            }
        }

        if let Some(fact) = self.terminal_thread_checkpoint(task_id, &events, facts.len())? {
            facts.push(fact);
        }
        self.commit_serialized(task_id, &facts).await?;
        let mut state = self.lock_state();
        for fact in &facts {
            if let ExecutionRecord::SteeringResolved { receipt } = fact
                && let Some(task) = state.tasks.get_mut(task_id)
                && let Some(entry) = task
                    .steering
                    .iter_mut()
                    .find(|entry| entry.receipt.input_id == receipt.input_id)
            {
                entry.receipt = receipt.clone();
            }
        }
        for event in events {
            self.append_locked(&mut state, task_id, event.payload)?;
        }
        Ok(())
    }

    fn resolve_pending(&self, state: &mut State, task_id: &str) -> Result<(), ServiceError> {
        if let Some(id) = state
            .tasks
            .get(task_id)
            .and_then(|record| record.snapshot.pending_input_id.clone())
        {
            self.append_locked(
                state,
                task_id,
                TaskEventPayload::InputResolved {
                    request_id: id,
                    approved: false,
                },
            )?;
            if let Some(pending) = state
                .tasks
                .get_mut(task_id)
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
        task_id: &str,
        mut payload: TaskEventPayload,
    ) -> Result<(), ServiceError> {
        if let TaskEventPayload::TaskFinished { detail, .. } = &mut payload
            && detail.len() > MAX_LIVE_BYTES
        {
            let mut end = MAX_LIVE_BYTES;
            while !detail.is_char_boundary(end) {
                end -= 1;
            }
            detail.truncate(end);
            detail.push_str(" (detail truncated)");
        }
        if matches!(payload, TaskEventPayload::TaskFinished { .. }) {
            self.resolve_pending(state, task_id)?;
        }
        let record = state.tasks.get(task_id).ok_or_else(unknown_task)?;
        let event = TaskEvent {
            thread_id: record.thread_id.clone(),
            server_instance_id: self.inner.instance_id.clone(),
            task_id: task_id.into(),
            seq: record.snapshot.cursor + 1,
            timestamp_ms: now_ms(),
            payload,
        };
        let encoded_bytes = serde_json::to_vec(&event)
            .map_err(|error| error.to_string())?
            .len();
        let record = state.tasks.get_mut(task_id).ok_or_else(unknown_task)?;
        record.snapshot.apply(&event);
        if record.snapshot.status.terminal() {
            if state
                .active_workspaces
                .get(&record.snapshot.workspace)
                .is_some_and(|owner| owner == task_id)
            {
                state.active_workspaces.remove(&record.snapshot.workspace);
                state.workspace_fences.remove(&record.snapshot.workspace);
            }
            record.pending.take();
            record.terminal_at = Some(Instant::now());
        }
        record.event_bytes += encoded_bytes;
        record.events.push_back(event.clone());
        while record.events.len() > self.inner.limits.events_per_task
            || record.event_bytes > self.inner.limits.event_bytes_per_task
        {
            if let Some(old) = record.events.pop_front() {
                record.event_bytes = record.event_bytes.saturating_sub(
                    serde_json::to_vec(&old)
                        .map_err(|error| error.to_string())?
                        .len(),
                );
            } else {
                break;
            }
        }
        let _ = record.publisher.send(Observation::Event {
            event: Box::new(event.clone()),
        });
        if matches!(
            event.payload,
            TaskEventPayload::AssistantDelta { .. } | TaskEventPayload::ToolOutputDelta { .. }
        ) && let Some(thread_id) = &event.thread_id
            && let Some(thread) = state.threads.get_mut(thread_id)
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

impl TaskSnapshot {
    pub fn apply(&mut self, event: &TaskEvent) {
        if event.server_instance_id != self.server_instance_id
            || event.task_id != self.task_id
            || event.seq <= self.cursor
        {
            return;
        }
        self.cursor = event.seq;
        match &event.payload {
            TaskEventPayload::TurnQueued { .. } => self.status = TaskStatus::Queued,
            TaskEventPayload::SteeringUpdated { receipt, .. } => {
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
            TaskEventPayload::Accepted { .. } => self.status = TaskStatus::Accepted,
            TaskEventPayload::TaskStarted | TaskEventPayload::InputResolved { .. } => {
                self.status = TaskStatus::Running;
                self.pending_input_id = None;
                self.pending_input = None;
            }
            TaskEventPayload::InputRequested {
                request_id,
                tool_id,
                tool_name,
                arguments,
            } => {
                self.status = TaskStatus::WaitingForInput;
                self.pending_input_id = Some(request_id.clone());
                self.pending_input = Some(InputRequest {
                    request_id: request_id.clone(),
                    tool_id: tool_id.clone(),
                    tool_name: tool_name.clone(),
                    arguments: arguments.clone(),
                });
            }
            TaskEventPayload::TaskFinished {
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
            TaskEventPayload::AssistantMessage { .. }
            | TaskEventPayload::AssistantInterrupted { .. }
            | TaskEventPayload::ToolFinished { .. } => {
                self.live = None;
            }
            TaskEventPayload::AssistantStarted { item_id, .. } => {
                self.live = None;
                self.live(Some(item_id), "assistant", "");
            }
            TaskEventPayload::AssistantDelta { item_id, text } => {
                self.live(Some(item_id), "assistant", text)
            }
            TaskEventPayload::ToolOutputDelta { id, source, text } => self.live(
                Some(id),
                &format!("shell {id}"),
                &format!("[{source}] {text}"),
            ),
            TaskEventPayload::ModelTurn { .. }
            | TaskEventPayload::ToolStarted { .. }
            | TaskEventPayload::CancelRequested => {}
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

    fn request(workspace: &TempDir) -> TaskRequest {
        TaskRequest {
            prompt: "change the file".into(),
            workspace: workspace.path().to_path_buf(),
            caller: CallerContext::local(),
            config: AgentConfig::fixed("fixture-model", None),
            verification_command: None,
            idempotency_key: None,
        }
    }

    pub(super) async fn wait_for(
        service: &TaskService,
        task_id: &str,
        status: TaskStatus,
    ) -> Result<TaskSnapshot, String> {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let snapshot = service.read(task_id).map_err(|error| error.to_string())?;
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
    async fn task_observation_is_ordered_and_instance_local()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        std::fs::write(workspace.path().join("note.txt"), "hello")?;
        let app = app(vec![
            turn(vec![tool_call(
                "read",
                "read",
                serde_json::json!({"path":"note.txt"}),
            )]),
            final_turn(),
        ])?;
        let service = TaskService::new(Arc::clone(&app), &[workspace.path().to_path_buf()])
            .map_err(std::io::Error::other)?;
        let mut submitted = request(&workspace);
        submitted.idempotency_key = Some("same-task".into());
        let accepted = service
            .submit(submitted)
            .await
            .map_err(std::io::Error::other)?;
        let mut duplicate = request(&workspace);
        duplicate.idempotency_key = Some("same-task".into());
        assert_eq!(
            service
                .submit(duplicate)
                .await
                .map_err(std::io::Error::other)?
                .task_id,
            accepted.task_id
        );
        assert_eq!(accepted.status, TaskStatus::Accepted);
        assert_eq!(accepted.cursor, 1);
        let completed = wait_for(&service, &accepted.task_id, TaskStatus::Completed)
            .await
            .map_err(std::io::Error::other)?;
        assert_eq!(completed.status, TaskStatus::Completed);
        assert_eq!(completed.final_answer.as_deref(), Some("done"));
        assert_eq!(completed.verification, VerificationStatus::NotRequested);
        let all = service
            .events_after(&accepted.task_id, 0)
            .map_err(std::io::Error::other)?;
        assert_eq!(all.len() as u64, completed.cursor);
        assert!(
            all.iter()
                .enumerate()
                .all(|(index, event)| event.seq == index as u64 + 1)
        );
        let tail = service
            .events_after(&accepted.task_id, 3)
            .map_err(std::io::Error::other)?;
        assert_eq!(tail.first().map(|event| event.seq), Some(4));
        let old_instance = service.capabilities().server_instance_id;
        service.shutdown().await;
        let reopened = TaskService::new(app, &[workspace.path().to_path_buf()])?;
        assert_ne!(old_instance, reopened.capabilities().server_instance_id);
        assert!(reopened.ensure_instance(Some(&old_instance)).is_err());
        assert!(reopened.read(&accepted.task_id).is_err());
        Ok(())
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
        let service = TaskService::new(app, &[workspace.path().to_path_buf()])
            .map_err(std::io::Error::other)?;
        assert!(service.submit(request(&other)).await.is_err());
        let accepted = service
            .submit(request(&workspace))
            .await
            .map_err(std::io::Error::other)?;
        assert!(service.submit(request(&workspace)).await.is_err());
        let waiting = wait_for(&service, &accepted.task_id, TaskStatus::WaitingForInput)
            .await
            .map_err(std::io::Error::other)?;
        assert_eq!(waiting.status, TaskStatus::WaitingForInput);
        let input_id = waiting
            .pending_input_id
            .ok_or_else(|| std::io::Error::other("missing pending input"))?;
        assert!(
            service
                .answer_input(&accepted.task_id, "wrong", true)
                .await
                .is_err()
        );
        assert!(!workspace.path().join("created.txt").exists());
        service
            .answer_input(&accepted.task_id, &input_id, true)
            .await
            .map_err(std::io::Error::other)?;
        assert!(
            service
                .answer_input(&accepted.task_id, &input_id, true)
                .await
                .is_err()
        );
        let completed = wait_for(&service, &accepted.task_id, TaskStatus::Completed)
            .await
            .map_err(std::io::Error::other)?;
        assert_eq!(completed.status, TaskStatus::Completed);
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
        let service = TaskService::new(
            app(vec![turn(vec![tool_call(
                "write",
                "write",
                serde_json::json!({"path":"created.txt", "content":"created"}),
            )])])?,
            &[workspace.path().to_path_buf()],
        )
        .map_err(std::io::Error::other)?;
        let accepted = service
            .submit(request(&workspace))
            .await
            .map_err(std::io::Error::other)?;
        let waiting = wait_for(&service, &accepted.task_id, TaskStatus::WaitingForInput)
            .await
            .map_err(std::io::Error::other)?;
        let input_id = waiting
            .pending_input_id
            .ok_or_else(|| std::io::Error::other("missing pending input"))?;
        service
            .cancel(&accepted.task_id)
            .await
            .map_err(std::io::Error::other)?;
        assert!(
            service
                .answer_input(&accepted.task_id, &input_id, true)
                .await
                .is_err()
        );
        let cancelled = wait_for(&service, &accepted.task_id, TaskStatus::Cancelled)
            .await
            .map_err(std::io::Error::other)?;
        assert_eq!(cancelled.status, TaskStatus::Cancelled);
        assert!(!workspace.path().join("created.txt").exists());
        Ok(())
    }

    #[tokio::test]
    async fn detach_and_lag_preserve_approval_and_snapshot_cutoff()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let limits = RuntimeLimits {
            events_per_task: 2,
            event_bytes_per_task: 64 * 1024,
            subscriber_queue: 2,
            subscribers_per_task: 1,
            ..RuntimeLimits::default()
        };
        let service = TaskService::with_limits(
            app(vec![turn(vec![tool_call(
                "write",
                "write",
                serde_json::json!({"path":"created.txt", "content":"created"}),
            )])])?,
            &[workspace.path().to_path_buf()],
            limits,
        )?;
        let accepted = service.submit(request(&workspace)).await?;
        let waiting = wait_for(&service, &accepted.task_id, TaskStatus::WaitingForInput).await?;
        let mut observer = service.observe(&accepted.task_id, Some(0))?;
        assert!(matches!(
            observer.next().await?,
            Some(Observation::Snapshot {
                resynchronized: true,
                ..
            })
        ));
        assert_eq!(
            service
                .observe(&accepted.task_id, None)
                .err()
                .map(|error| error.code),
            Some(ErrorCode::Overloaded)
        );
        drop(observer);
        assert_eq!(
            service.read(&accepted.task_id)?.status,
            TaskStatus::WaitingForInput
        );
        let mut observer = service.observe(&accepted.task_id, None)?;
        let cutoff = match observer.next().await? {
            Some(Observation::Snapshot { snapshot, .. }) => snapshot.cursor,
            _ => return Err("missing initial snapshot".into()),
        };
        for _ in 0..10 {
            service
                .append(
                    &accepted.task_id,
                    TaskEventPayload::AssistantDelta {
                        item_id: "assistant-test".into(),
                        text: "live".repeat(16_384),
                    },
                )
                .await?;
        }
        let refreshed = match observer.next().await? {
            Some(Observation::Snapshot {
                snapshot,
                resynchronized: true,
                ..
            }) => snapshot,
            _ => return Err("slow observer did not resynchronize".into()),
        };
        assert!(refreshed.cursor > cutoff);
        assert_eq!(
            refreshed
                .pending_input
                .as_ref()
                .map(|input| input.tool_name.as_str()),
            Some("write")
        );
        assert!(
            refreshed
                .live
                .as_ref()
                .is_some_and(|live| live.text.len() <= MAX_LIVE_BYTES)
        );
        {
            let state = service.lock_state();
            let record = state
                .tasks
                .get(&accepted.task_id)
                .ok_or("task disappeared")?;
            assert!(record.events.len() <= 2);
            assert!(record.event_bytes <= service.inner.limits.event_bytes_per_task);
        }
        service.cancel(&accepted.task_id).await?;
        let resolved = observer.next().await?.ok_or("missing resolution")?;
        assert!(
            matches!(resolved, Observation::Event { event } if event.seq == refreshed.cursor + 1 && matches!(event.payload, TaskEventPayload::InputResolved { approved: false, .. }))
        );
        assert!(
            service
                .answer_input(
                    &accepted.task_id,
                    waiting.pending_input_id.as_deref().ok_or("missing input")?,
                    true
                )
                .await
                .is_err()
        );
        service.shutdown().await;
        assert!(!workspace.path().join("created.txt").exists());
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_seals_admission_and_joins_waiting_execution()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let other = TempDir::new()?;
        let limits = RuntimeLimits {
            active_tasks: 1,
            ..RuntimeLimits::default()
        };
        let service = TaskService::with_limits(
            app(vec![turn(vec![tool_call(
                "write",
                "write",
                serde_json::json!({"path":"created.txt", "content":"created"}),
            )])])?,
            &[workspace.path().to_path_buf(), other.path().to_path_buf()],
            limits,
        )?;
        let accepted = service.submit(request(&workspace)).await?;
        wait_for(&service, &accepted.task_id, TaskStatus::WaitingForInput).await?;
        assert_eq!(
            service
                .submit(request(&other))
                .await
                .err()
                .map(|error| error.code),
            Some(ErrorCode::Overloaded)
        );
        tokio::time::timeout(Duration::from_secs(2), service.shutdown()).await?;
        assert_eq!(
            service.read(&accepted.task_id)?.status,
            TaskStatus::Cancelled
        );
        assert!(service.read(&accepted.task_id)?.pending_input.is_none());
        assert!(service.inner.workers.is_empty());
        assert!(service.lock_state().active_workspaces.is_empty());
        assert_eq!(
            service
                .submit(request(&workspace))
                .await
                .err()
                .map(|error| error.code),
            Some(ErrorCode::ShuttingDown)
        );
        assert!(!workspace.path().join("created.txt").exists());
        Ok(())
    }

    #[tokio::test]
    async fn terminal_retention_evicts_tasks_and_their_idempotency_keys()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let service = TaskService::with_limits(
            app(vec![final_turn(), final_turn(), final_turn()])?,
            &[workspace.path().to_path_buf()],
            RuntimeLimits {
                retained_tasks: 1,
                ..RuntimeLimits::default()
            },
        )?;
        let mut first = request(&workspace);
        first.idempotency_key = Some("first".into());
        let first = service.submit(first).await?;
        wait_for(&service, &first.task_id, TaskStatus::Completed).await?;
        let second = service.submit(request(&workspace)).await?;
        wait_for(&service, &second.task_id, TaskStatus::Completed).await?;
        assert_eq!(
            service.read(&first.task_id).err().map(|error| error.code),
            Some(ErrorCode::UnknownTask)
        );
        let mut reused = request(&workspace);
        reused.idempotency_key = Some("first".into());
        assert_ne!(service.submit(reused).await?.task_id, first.task_id);
        service.shutdown().await;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_waits_for_verification_process_cleanup()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let service =
            TaskService::new(app(vec![final_turn()])?, &[workspace.path().to_path_buf()])?;
        let mut submitted = request(&workspace);
        submitted.verification_command = Some("touch started; sleep 30; touch leaked".into());
        let accepted = service.submit(submitted).await?;
        let waiting = wait_for(&service, &accepted.task_id, TaskStatus::WaitingForInput).await?;
        service
            .answer_input(
                &accepted.task_id,
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
            service.read(&accepted.task_id)?.status,
            TaskStatus::RecoveryRequired
        );
        assert!(service.read(&accepted.task_id)?.unknown_effect);
        assert!(!workspace.path().join("leaked").exists());
        Ok(())
    }

    #[tokio::test]
    async fn configured_verification_records_exit_status_and_controls_outcome()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let service = TaskService::new(
            app(vec![final_turn(), final_turn()])?,
            &[workspace.path().to_path_buf()],
        )
        .map_err(std::io::Error::other)?;
        let mut read_only = request(&workspace);
        read_only.config = read_only.config.read_only();
        read_only.verification_command = Some("echo forbidden".into());
        assert!(service.submit(read_only).await.is_err());
        let mut passing = request(&workspace);
        passing.verification_command = Some("echo verified".into());
        let accepted = service
            .submit(passing)
            .await
            .map_err(std::io::Error::other)?;
        let waiting = wait_for(&service, &accepted.task_id, TaskStatus::WaitingForInput).await?;
        assert_eq!(
            service.inner.tool_workers.available_permits(),
            service.inner.limits.global_tools
        );
        service
            .answer_input(
                &accepted.task_id,
                waiting
                    .pending_input_id
                    .as_deref()
                    .ok_or("verification approval missing")?,
                true,
            )
            .await?;
        let passed = wait_for(&service, &accepted.task_id, TaskStatus::Completed)
            .await
            .map_err(std::io::Error::other)?;
        assert_eq!(passed.status, TaskStatus::Completed);
        assert_eq!(passed.verification, VerificationStatus::Passed);
        let stored = service
            .inner
            .store
            .load(&accepted.task_id)
            .await?
            .ok_or("execution records missing")?;
        assert!(stored.records.iter().any(|record| matches!(record, ExecutionRecord::ToolIntent { call, .. } if call.origin == CallOrigin::Verification)));
        assert!(stored.records.iter().any(|record| matches!(record, ExecutionRecord::VerificationResult { call, effect: EffectStatus::Completed, evidence, .. } if call.origin == CallOrigin::Verification && evidence.exit_status == Some(0))));
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
            .submit(failing)
            .await
            .map_err(std::io::Error::other)?;
        let waiting = wait_for(&service, &accepted.task_id, TaskStatus::WaitingForInput).await?;
        service
            .answer_input(
                &accepted.task_id,
                waiting
                    .pending_input_id
                    .as_deref()
                    .ok_or("verification approval missing")?,
                true,
            )
            .await?;
        let failed = wait_for(&service, &accepted.task_id, TaskStatus::Failed)
            .await
            .map_err(std::io::Error::other)?;
        assert_eq!(failed.status, TaskStatus::Failed);
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
        let service = TaskService::new(
            app(vec![final_turn(), final_turn()])?,
            &[workspace.path().to_path_buf()],
        )?;
        let permits = Arc::clone(&service.inner.tool_workers)
            .acquire_many_owned(16)
            .await?;
        for approved in [false, true] {
            let mut submitted = request(&workspace);
            submitted.verification_command = Some("echo blocked > created".into());
            let accepted = service.submit(submitted).await?;
            let waiting =
                wait_for(&service, &accepted.task_id, TaskStatus::WaitingForInput).await?;
            let input = waiting
                .pending_input
                .as_ref()
                .ok_or("verification approval missing")?;
            assert!(input.arguments.contains("echo blocked > created"));
            service
                .answer_input(&accepted.task_id, &input.request_id, approved)
                .await?;
            if approved {
                service.cancel(&accepted.task_id).await?;
            }
            let finished = wait_for(
                &service,
                &accepted.task_id,
                if approved {
                    TaskStatus::Cancelled
                } else {
                    TaskStatus::Failed
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
                .load(&accepted.task_id)
                .await?
                .ok_or("execution missing")?;
            assert!(!stored.records.iter().any(|record| matches!(record, ExecutionRecord::ToolIntent { call, .. } if call.origin == CallOrigin::Verification)));
            assert!(stored.records.iter().any(|record| matches!(
                record,
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
            let fails = records.iter().any(|record| match record {
                ExecutionRecord::Accepted { .. } => self.failure == "accepted",
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
            let service = TaskService::with_store(
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
            let accepted = service.submit(request(&workspace)).await;
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
                    wait_for(&service, &accepted.task_id, TaskStatus::WaitingForInput).await?;
                let request_id = approval
                    .pending_input_id
                    .ok_or("approval identity missing")?;
                service
                    .answer_input(&accepted.task_id, &request_id, true)
                    .await?;
            }
            let blocked =
                wait_for(&service, &accepted.task_id, TaskStatus::RecoveryRequired).await?;
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
                .load(&accepted.task_id)
                .await?
                .ok_or("execution missing")?;
            assert!(
                !saved
                    .records
                    .iter()
                    .any(|record| matches!(record, ExecutionRecord::ToolResult { .. }))
            );
            if failure == "result" {
                assert!(
                    saved
                        .records
                        .iter()
                        .any(|record| matches!(record, ExecutionRecord::ToolIntent { .. }))
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
        let service = TaskService::with_store(
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
        let accepted = service.submit(request(&workspace)).await?;
        let waiting = wait_for(&service, &accepted.task_id, TaskStatus::WaitingForInput).await?;
        service
            .answer_input(
                &accepted.task_id,
                waiting
                    .pending_input_id
                    .as_deref()
                    .ok_or("approval missing")?,
                true,
            )
            .await?;
        assert_eq!(
            wait_for(&service, &accepted.task_id, TaskStatus::RecoveryRequired)
                .await?
                .status,
            TaskStatus::RecoveryRequired
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("known.txt"))?,
            "one"
        );
        let saved = store
            .load(&accepted.task_id)
            .await?
            .ok_or("execution missing")?;
        assert!(saved.records.iter().any(|r| matches!(
            r,
            ExecutionRecord::ToolResult {
                effect: EffectStatus::Completed,
                ..
            }
        )));
        assert!(
            !saved
                .records
                .iter()
                .any(|r| matches!(r, ExecutionRecord::Event { event }
            if matches!(event.payload, TaskEventPayload::TaskFinished { .. })))
        );
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
        let peer = TaskService::new(app(vec![final_turn()])?, &[workspace.path().to_path_buf()])?;
        assert_eq!(
            peer.submit(request(&workspace))
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
