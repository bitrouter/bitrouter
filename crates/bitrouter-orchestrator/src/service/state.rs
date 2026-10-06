//! Shared live state. ThreadService remains its only execution and commit owner.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bitrouter_sdk::App;
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::Message;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::{ErrorCode, RuntimeLimits, ServiceError, startup, steering, workspace};
use crate::agent::AgentConfig;
use crate::store::ExecutionStore;
use crate::thread::{PermissionProfile, ThreadSnapshot};
use crate::turn::TurnSnapshot;

pub(super) struct PendingInput {
    pub(super) response: oneshot::Sender<bool>,
}

pub(super) struct VerificationBudget {
    pub(super) duration: Duration,
    pub(super) active_duration_ms: u64,
    pub(super) calls: u32,
    pub(super) max_calls: u32,
}

pub(super) struct TurnRecord {
    pub(super) fence: Arc<crate::control::LaunchFence>,
    pub(super) steering: Vec<steering::SteeringInput>,
    pub(super) verification_budget: Option<(u64, u32)>,
    pub(super) thread_id: String,
    pub(super) permission_profile: PermissionProfile,
    pub(super) settled: Option<(Vec<Message>, u64)>,
    pub(super) snapshot: TurnSnapshot,
    pub(super) terminal_at: Option<Instant>,
    pub(super) cancel: CancellationToken,
    pub(super) pending: Option<PendingInput>,
    pub(super) storage_error: Option<String>,
}

pub(super) struct State {
    pub(super) startup_discovery: Option<startup::StartupDiscovery>,
    pub(super) cold_executions: HashMap<String, startup::ColdExecution>,
    pub(super) workspace_fences: HashMap<PathBuf, Arc<workspace::WorkspaceFence>>,
    pub(super) execution_ownership: Option<crate::store::OwnerClaim>,
    pub(super) threads: HashMap<String, ThreadRecord>,
    pub(super) ready_threads: VecDeque<String>,
    pub(super) running_turns: HashMap<String, String>,
    pub(super) workspace_profiles: HashMap<PathBuf, Vec<PermissionProfile>>,
    pub(super) turns: HashMap<String, TurnRecord>,
    pub(super) active_workspaces: HashMap<PathBuf, String>,
    pub(super) allowed_workspaces: Vec<PathBuf>,
    pub(super) instruction_roots: HashMap<PathBuf, PathBuf>,
    pub(super) closing: bool,
}

pub(super) struct Inner {
    pub(super) queue_waker_started: std::sync::atomic::AtomicBool,
    pub(super) ownership_init: tokio::sync::Mutex<()>,
    pub(super) cleanup_unconfirmed: std::sync::atomic::AtomicBool,
    pub(super) app: Arc<App>,
    pub(super) resources: Arc<crate::harness::HarnessConfig>,
    pub(super) instance_id: String,
    pub(super) limits: RuntimeLimits,
    pub(super) workers: TaskTracker,
    pub(super) runtime_changed: tokio::sync::Notify,
    pub(super) state: Mutex<State>,
    pub(super) store: Arc<dyn ExecutionStore>,
    pub(super) admission: tokio::sync::Mutex<()>,
    pub(super) tool_workers: Arc<tokio::sync::Semaphore>,
    pub(super) recovery_readers: tokio::sync::Semaphore,
}

#[derive(Clone)]
pub(super) struct QueuedTurn {
    pub(super) turn_id: String,
    pub(super) user_item_id: String,
    pub(super) prompt: String,
    pub(super) order: u64,
}

pub(super) struct ThreadRecord {
    pub(super) servers: Option<Vec<bitrouter_sdk::mcp::transport::McpServerConfig>>,
    pub(super) presentation: super::observation::Presentation,
    pub(super) snapshot: ThreadSnapshot,
    pub(super) caller: CallerContext,
    pub(super) config: AgentConfig,
    pub(super) verification_command: Option<String>,
    pub(super) messages: Vec<Message>,
    pub(super) instructions: Option<crate::harness::instructions::InstructionSnapshot>,
    pub(super) instructions_epoch: Option<String>,
    pub(super) queued: VecDeque<QueuedTurn>,
    pub(super) next_order: u64,
    pub(super) commit_lock: Arc<tokio::sync::Mutex<()>>,
    pub(super) store_version: u64,
    pub(super) storage_error: Option<String>,
    pub(super) last_used: Instant,
}

impl ThreadRecord {
    pub(super) fn authorize(&self, caller: &CallerContext) -> Result<(), ServiceError> {
        if caller.api_key_id() != self.caller.api_key_id()
            || caller.user_id() != self.caller.user_id()
        {
            return Err(ServiceError::new(
                ErrorCode::Unauthorized,
                "caller does not own this Thread",
            ));
        }
        Ok(())
    }

    pub(super) fn bytes(&self) -> usize {
        serde_json::to_vec(&self.messages)
            .map_or(usize::MAX, |value| value.len().saturating_mul(2))
            .saturating_add(
                serde_json::to_vec(&self.instructions).map_or(usize::MAX, |value| value.len()),
            )
            .saturating_add(
                serde_json::to_vec(&self.servers).map_or(usize::MAX, |value| value.len()),
            )
            .saturating_add(
                self.queued
                    .iter()
                    .map(|entry| entry.prompt.len().saturating_mul(2))
                    .sum::<usize>(),
            )
    }
}
