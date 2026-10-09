//! Server-owned agent runtime with transactional execution facts. State, execution and observation share one
//! authority; disconnecting an observer never cancels its task.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bitrouter_sdk::App;
use serde::{Deserialize, Serialize};
use tokio_util::task::TaskTracker;

use crate::service::state::{Inner, State};
use crate::store::{ExecutionStore, MemoryExecutionStore};
use crate::thread::PermissionProfile;

mod admission;
mod approval;
mod commit;
pub mod observation;
mod ownership;
mod queue;
mod recovery;
mod runner;
pub mod startup;
mod state;
mod steering;
mod threads;
mod verification;
mod workspace;

const MAX_EVENT_PAGE: usize = 1000;

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

#[derive(Clone)]
pub struct ThreadService {
    inner: Arc<Inner>,
}

impl ThreadService {
    /// Configure local extension resources before cloning or starting the service.
    pub fn with_resources(
        mut self,
        config: crate::harness::HarnessConfig,
    ) -> Result<Self, ServiceError> {
        if config.servers.len() > 32 || config.skill_roots.len() > 31 {
            return Err("harness configuration exceeds resource limits".into());
        }
        for server in &config.servers {
            server.validate().map_err(|error| error.to_string())?;
        }
        config.instructions.validate()?;
        let inner = Arc::get_mut(&mut self.inner)
            .ok_or("configure harness resources before sharing the runtime")?;
        inner.resources = Arc::new(config);
        Ok(self)
    }

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
                resources: Arc::new(crate::harness::HarnessConfig::default()),
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
                    instruction_roots: HashMap::new(),
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
        let instruction_root = crate::harness::instructions::project_root(&workspace, None)
            .unwrap_or_else(|| workspace.clone());
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
        state
            .instruction_roots
            .insert(workspace.clone(), instruction_root);
        Ok(workspace)
    }

    // Test fixture: exercise the same two native admission operations as CLI.

    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        match self.inner.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
