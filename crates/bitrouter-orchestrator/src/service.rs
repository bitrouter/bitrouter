//! Process-local agent runtime. State, execution and observation share one
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

use crate::agent::{Agent, AgentConfig, ApprovalRequest, RunEvent, RunStatus, ToolMode};
use crate::tools::WorkspaceTools;

const MAX_EVENT_PAGE: usize = 1000;
const MAX_LIVE_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Accepted,
    Running,
    WaitingForInput,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
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
    Accepted {
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
    ModelTurn {
        request_id: String,
        requested_model: String,
        usage: Option<Usage>,
    },
    AssistantMessage {
        message: Message,
    },
    AssistantDelta {
        text: String,
    },
    ToolOutputDelta {
        id: String,
        source: String,
        text: String,
    },
    ToolStarted {
        id: String,
        name: String,
    },
    ToolFinished {
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
    pub server_instance_id: String,
    pub task_id: String,
    pub seq: u64,
    pub timestamp_ms: u64,
    pub payload: TaskEventPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSnapshot {
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
    pub server_instance_id: String,
    pub limits: RuntimeLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeLimits {
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
    InvalidRequest,
    UnknownTask,
    Conflict,
    Overloaded,
    ShuttingDown,
    InstanceChanged,
    ResyncRequired,
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
        event: TaskEvent,
    },
}

/// A subscription holds no execution authority. Dropping it only detaches.
pub struct TaskSubscription {
    service: TaskService,
    task_id: String,
    receiver: broadcast::Receiver<TaskEvent>,
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
            Ok(event) => {
                self.finished = matches!(event.payload, TaskEventPayload::TaskFinished { .. });
                Ok(Some(Observation::Event { event }))
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                let mut state = self.service.lock_state();
                let record = state
                    .tasks
                    .get_mut(&self.task_id)
                    .ok_or_else(unknown_task)?;
                // Registration and snapshot cutoff are atomic with publication.
                self.receiver = record.publisher.subscribe();
                self.finished = record.snapshot.status.terminal();
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

struct TaskRecord {
    snapshot: TaskSnapshot,
    events: VecDeque<TaskEvent>,
    event_bytes: usize,
    publisher: broadcast::Sender<TaskEvent>,
    terminal_at: Option<Instant>,
    cancel: CancellationToken,
    pending: Option<PendingInput>,
}

struct State {
    tasks: HashMap<String, TaskRecord>,
    active_workspaces: HashMap<PathBuf, String>,
    allowed_workspaces: Vec<PathBuf>,
    idempotency: HashMap<(String, String, String), (String, String)>,
    closing: bool,
}

struct Inner {
    app: Arc<App>,
    instance_id: String,
    limits: RuntimeLimits,
    workers: TaskTracker,
    state: Mutex<State>,
}

#[derive(Clone)]
pub struct TaskService {
    inner: Arc<Inner>,
}

impl TaskService {
    pub fn new(app: Arc<App>, allowed_workspaces: &[PathBuf]) -> Result<Self, ServiceError> {
        Self::with_limits(app, allowed_workspaces, RuntimeLimits::default())
    }

    fn with_limits(
        app: Arc<App>,
        allowed_workspaces: &[PathBuf],
        limits: RuntimeLimits,
    ) -> Result<Self, ServiceError> {
        if limits.active_tasks == 0
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
                app,
                instance_id: uuid::Uuid::new_v4().to_string(),
                limits,
                workers: TaskTracker::new(),
                state: Mutex::new(State {
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
        RuntimeCapabilities {
            server_instance_id: self.inner.instance_id.clone(),
            limits: self.inner.limits.clone(),
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
            let mut state = self.lock_state();
            state.closing = true;
            for record in state.tasks.values() {
                record.cancel.cancel();
            }
            // Spawn and close are serialized under the same state lock.
            self.inner.workers.close();
        }
        self.inner.workers.wait().await;
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
            finished: record.snapshot.status.terminal(),
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
            state.allowed_workspaces.push(workspace.clone());
        }
        Ok(workspace)
    }

    pub fn submit(&self, request: TaskRequest) -> Result<TaskSnapshot, ServiceError> {
        if request.prompt.len()
            + request.config.instructions.len()
            + request.config.model.len()
            + request.verification_command.as_ref().map_or(0, String::len)
            > self.inner.limits.request_bytes
        {
            return Err("task request is too large".into());
        }
        if request.config.max_steps > 256
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
        let agent = Agent::new(
            Arc::clone(&self.inner.app),
            request.caller,
            &workspace,
            request.config,
        )?;
        let task_id = uuid::Uuid::new_v4().to_string();
        let cancel = CancellationToken::new();
        let mut state = self.lock_state();
        self.prune(&mut state);
        if state.closing {
            return Err(ServiceError::new(
                ErrorCode::ShuttingDown,
                "runtime is shutting down",
            ));
        }
        let accepted = {
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
            if state.active_workspaces.contains_key(&workspace) {
                return Err(ServiceError::new(
                    ErrorCode::Conflict,
                    "another task already owns this workspace",
                ));
            }
            let snapshot = TaskSnapshot {
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
                    snapshot,
                    events: VecDeque::new(),
                    event_bytes: 0,
                    publisher: broadcast::channel(self.inner.limits.subscriber_queue).0,
                    terminal_at: None,
                    cancel: cancel.clone(),
                    pending: None,
                },
            );
            if let Err(error) = self.append_locked(
                &mut state,
                &task_id,
                TaskEventPayload::Accepted {
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
        let prompt = request.prompt;
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

    pub fn answer_input(
        &self,
        task_id: &str,
        request_id: &str,
        approved: bool,
    ) -> Result<(), ServiceError> {
        let sender = {
            let mut state = self.lock_state();
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
            self.append_locked(
                &mut state,
                task_id,
                TaskEventPayload::InputResolved {
                    request_id: request_id.into(),
                    approved,
                },
            )?;
            state
                .tasks
                .get_mut(task_id)
                .and_then(|record| record.pending.take())
                .ok_or_else(|| "pending input channel is unavailable".to_string())?
                .response
        };
        sender.send(approved).map_err(|_| {
            ServiceError::new(ErrorCode::Conflict, "task stopped before accepting input")
        })
    }

    pub fn cancel(&self, task_id: &str) -> Result<(), ServiceError> {
        let token = {
            let mut state = self.lock_state();
            let record = state.tasks.get(task_id).ok_or_else(unknown_task)?;
            if record.snapshot.status.terminal() {
                return Ok(());
            }
            let token = record.cancel.clone();
            if token.is_cancelled() {
                return Ok(());
            }
            token.cancel();
            self.resolve_pending(&mut state, task_id)?;
            self.append_locked(&mut state, task_id, TaskEventPayload::CancelRequested)?;
            token
        };
        token.cancel();
        Ok(())
    }

    async fn run_task(
        &self,
        task_id: String,
        agent: Agent,
        prompt: String,
        verification_command: Option<String>,
        workspace: PathBuf,
        cancel: CancellationToken,
    ) {
        if self
            .append(&task_id, TaskEventPayload::TaskStarted)
            .is_err()
        {
            cancel.cancel();
            return;
        }
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let (approval_tx, mut approval_rx) = mpsc::channel(1);
        let run_cancel = cancel.clone();
        let mut run = tokio::spawn(async move {
            agent
                .run_with_approvals(prompt, run_cancel, Some(event_tx), Some(approval_tx))
                .await
        });
        let report = loop {
            tokio::select! {
                Some(event) = event_rx.recv() => {
                    if self.append_agent_event(&task_id, event).is_err() {
                        break None;
                    }
                }
                Some(request) = approval_rx.recv() => {
                    // Agent control events precede its approval handoff. Drain
                    // that finite prefix before publishing the input request.
                    while let Ok(event) = event_rx.try_recv() {
                        if self.append_agent_event(&task_id, event).is_err() {
                            cancel.cancel();
                        }
                    }
                    if self.request_approval(&task_id, request).is_err() {
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
                        });
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
                }
            }
            let _ = self.append(
                &task_id,
                TaskEventPayload::TaskFinished {
                    status: TaskStatus::Interrupted,
                    detail: "agent execution stopped unexpectedly; effects may have occurred"
                        .into(),
                    final_answer: None,
                    verification: VerificationStatus::Unavailable,
                    verification_evidence: None,
                    unknown_effect: true,
                },
            );
            return;
        }
        if let Some(report) = report {
            while let Ok(event) = event_rx.try_recv() {
                if self.append_agent_event(&task_id, event).is_err() {
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
            let (verification, evidence) = if status == TaskStatus::Completed {
                match verification_command {
                    Some(command) => {
                        let evidence = verify(&workspace, command, &cancel).await;
                        let verification = if evidence.exit_status == Some(0)
                            && !evidence.timed_out
                            && evidence.error.is_none()
                        {
                            VerificationStatus::Passed
                        } else {
                            status = if cancel.is_cancelled() {
                                TaskStatus::Cancelled
                            } else {
                                TaskStatus::Failed
                            };
                            VerificationStatus::Failed
                        };
                        (verification, Some(evidence))
                    }
                    None => (VerificationStatus::Unavailable, None),
                }
            } else {
                (VerificationStatus::Unavailable, None)
            };
            let _ = self.append(
                &task_id,
                TaskEventPayload::TaskFinished {
                    status,
                    detail: if verification == VerificationStatus::Failed {
                        "configured verification check failed".into()
                    } else {
                        report.detail
                    },
                    final_answer: report.final_answer,
                    verification,
                    verification_evidence: evidence,
                    unknown_effect: false,
                },
            );
        }
    }

    fn request_approval(
        &self,
        task_id: &str,
        request: ApprovalRequest,
    ) -> Result<(), ServiceError> {
        let mut state = self.lock_state();
        let record = state.tasks.get(task_id).ok_or_else(unknown_task)?;
        if record.cancel.is_cancelled() {
            let _ = request.response.send(false);
            return Ok(());
        }
        if record.snapshot.status.terminal() || record.pending.is_some() {
            return Err("task cannot accept another pending input".into());
        }
        self.append_locked(
            &mut state,
            task_id,
            TaskEventPayload::InputRequested {
                request_id: request.id.clone(),
                tool_id: request.tool_id,
                tool_name: request.tool_name,
                arguments: request.arguments,
            },
        )?;
        if let Some(record) = state.tasks.get_mut(task_id) {
            record.pending = Some(PendingInput {
                response: request.response,
            });
        }
        Ok(())
    }

    fn append_agent_event(&self, task_id: &str, event: RunEvent) -> Result<(), ServiceError> {
        let payload = match event {
            RunEvent::UserMessage(_) | RunEvent::Finished { .. } => return Ok(()),
            RunEvent::AssistantDelta(text) => TaskEventPayload::AssistantDelta { text },
            RunEvent::ToolOutputDelta { id, source, text } => {
                TaskEventPayload::ToolOutputDelta { id, source, text }
            }
            RunEvent::ModelTurn {
                request_id,
                requested_model,
                usage,
            } => TaskEventPayload::ModelTurn {
                request_id,
                requested_model,
                usage,
            },
            RunEvent::AssistantMessage(message) => TaskEventPayload::AssistantMessage { message },
            RunEvent::ToolStarted { id, name } => TaskEventPayload::ToolStarted { id, name },
            RunEvent::ToolFinished { id, name, output } => {
                TaskEventPayload::ToolFinished { id, name, output }
            }
        };
        self.append(task_id, payload)
    }

    fn append(&self, task_id: &str, payload: TaskEventPayload) -> Result<(), ServiceError> {
        let mut state = self.lock_state();
        self.append_locked(&mut state, task_id, payload)
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
            state.active_workspaces.remove(&record.snapshot.workspace);
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
        let _ = record.publisher.send(event);
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
            TaskEventPayload::AssistantMessage { .. } | TaskEventPayload::ToolFinished { .. } => {
                self.live = None;
            }
            TaskEventPayload::AssistantDelta { text } => self.live("assistant", text),
            TaskEventPayload::ToolOutputDelta { id, source, text } => {
                self.live(&format!("shell {id}"), &format!("[{source}] {text}"))
            }
            TaskEventPayload::ModelTurn { .. }
            | TaskEventPayload::ToolStarted { .. }
            | TaskEventPayload::CancelRequested => {}
        }
    }
    fn live(&mut self, kind: &str, text: &str) {
        let live = self.live.get_or_insert_with(|| LiveActivity {
            kind: kind.into(),
            text: String::new(),
            truncated: false,
        });
        if live.kind != kind {
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

async fn verify(
    workspace: &Path,
    command: String,
    cancel: &CancellationToken,
) -> VerificationEvidence {
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
    let tools = match WorkspaceTools::new(workspace) {
        Ok(tools) => tools,
        Err(error) => {
            evidence.error = Some(error.to_string());
            return evidence;
        }
    };
    let arguments = serde_json::json!({"command": command, "timeout": 120}).to_string();
    let shell = if cfg!(windows) { "powershell" } else { "bash" };
    match tools
        .execute(shell, &arguments, cancel, "verification", None)
        .await
    {
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

    fn app(turns: Vec<GenerateResult>) -> std::io::Result<Arc<App>> {
        let table = StaticRoutingTable::new();
        table.insert("fixture-model", vec![target()]);
        App::builder()
            .language_model(|builder| {
                builder
                    .routing_table(Arc::new(table))
                    .executor(Arc::new(MockExecutor::new(
                        turns.into_iter().map(mock_stream).collect(),
                    )));
            })
            .build()
            .map(Arc::new)
            .map_err(std::io::Error::other)
    }

    fn turn(parts: Vec<Content>) -> GenerateResult {
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

    fn mock_stream(turn: GenerateResult) -> MockResponse {
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

    fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> Content {
        Content::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.to_string(),
            provider_executed: false,
            dynamic: false,
            provider_metadata: Default::default(),
        }
    }

    fn final_turn() -> GenerateResult {
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

    async fn wait_for(
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
        let accepted = service.submit(submitted).map_err(std::io::Error::other)?;
        let mut duplicate = request(&workspace);
        duplicate.idempotency_key = Some("same-task".into());
        assert_eq!(
            service
                .submit(duplicate)
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
        assert_eq!(completed.verification, VerificationStatus::Unavailable);
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
        assert!(service.submit(request(&other)).is_err());
        let accepted = service
            .submit(request(&workspace))
            .map_err(std::io::Error::other)?;
        assert!(service.submit(request(&workspace)).is_err());
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
                .is_err()
        );
        assert!(!workspace.path().join("created.txt").exists());
        service
            .answer_input(&accepted.task_id, &input_id, true)
            .map_err(std::io::Error::other)?;
        assert!(
            service
                .answer_input(&accepted.task_id, &input_id, true)
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
            .map_err(std::io::Error::other)?;
        let waiting = wait_for(&service, &accepted.task_id, TaskStatus::WaitingForInput)
            .await
            .map_err(std::io::Error::other)?;
        let input_id = waiting
            .pending_input_id
            .ok_or_else(|| std::io::Error::other("missing pending input"))?;
        service
            .cancel(&accepted.task_id)
            .map_err(std::io::Error::other)?;
        assert!(
            service
                .answer_input(&accepted.task_id, &input_id, true)
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
        let accepted = service.submit(request(&workspace))?;
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
            service.append(
                &accepted.task_id,
                TaskEventPayload::AssistantDelta {
                    text: "live".repeat(16_384),
                },
            )?;
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
        service.cancel(&accepted.task_id)?;
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
        let accepted = service.submit(request(&workspace))?;
        wait_for(&service, &accepted.task_id, TaskStatus::WaitingForInput).await?;
        assert_eq!(
            service
                .submit(request(&other))
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
        let first = service.submit(first)?;
        wait_for(&service, &first.task_id, TaskStatus::Completed).await?;
        let second = service.submit(request(&workspace))?;
        wait_for(&service, &second.task_id, TaskStatus::Completed).await?;
        assert_eq!(
            service.read(&first.task_id).err().map(|error| error.code),
            Some(ErrorCode::UnknownTask)
        );
        let mut reused = request(&workspace);
        reused.idempotency_key = Some("first".into());
        assert_ne!(service.submit(reused)?.task_id, first.task_id);
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
        let accepted = service.submit(submitted)?;
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
            TaskStatus::Cancelled
        );
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
        assert!(service.submit(read_only).is_err());
        let mut passing = request(&workspace);
        passing.verification_command = Some("echo verified".into());
        let accepted = service.submit(passing).map_err(std::io::Error::other)?;
        let passed = wait_for(&service, &accepted.task_id, TaskStatus::Completed)
            .await
            .map_err(std::io::Error::other)?;
        assert_eq!(passed.status, TaskStatus::Completed);
        assert_eq!(passed.verification, VerificationStatus::Passed);
        assert_eq!(
            passed
                .verification_evidence
                .as_ref()
                .and_then(|e| e.exit_status),
            Some(0)
        );
        let mut failing = request(&workspace);
        failing.verification_command = Some("exit 7".into());
        let accepted = service.submit(failing).map_err(std::io::Error::other)?;
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
}
