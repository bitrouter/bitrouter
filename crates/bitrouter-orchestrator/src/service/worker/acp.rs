//! Managed external sessions are transport adapters of the shared Turn owner.
//! Public ACP observations are retained without manufacturing native state.
use std::sync::Arc;

use bitrouter_sdk::caller::CallerContext;
use serde::{Deserialize, Serialize};

use crate::control::WorkControl;
use crate::service::{ErrorCode, ServiceError, ThreadService};
use crate::thread::{GoalWork, ThreadTarget};
use crate::turn::{TurnReceipt, TurnRequest};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub server_instance_id: String,
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Binding {
    pub run_id: String,
    pub session_id: String,
    pub agent_id: String,
    pub confirmed_model_route: Option<String>,
    pub capabilities: Option<serde_json::Value>,
    /// Previous owned run of this same private session; never a new session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored_from: Option<String>,
}

/// Derived only from a complete accepted journal before the first prompt intent.
/// Loading this description does not confirm private session continuity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Continuation {
    pub owner: Owner,
    pub binding: Binding,
    pub prompt: String,
    pub accepted_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Permission {
    pub request_id: String,
    /// Original wire material stays readable without the optional ACP runtime.
    /// The adapter decodes protocol types; this owner checks the offered IDs.
    pub tool_call: serde_json::Value,
    pub options: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub cursor: u64,
    pub snapshot_cursor: u64,
    pub history_complete: bool,
    pub events: Vec<serde_json::Value>,
    pub answer_delta: String,
    /// A protocol end-of-prompt result, independent of process quiescence.
    pub outcome: Option<Outcome>,
    pub permissions: Vec<Permission>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Outcome {
    Completed,
    Unknown(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stop {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub confirmed: bool,
    pub session_never_started: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Start,
    Prompt,
    Stop,
}

impl Action {
    pub fn key(self, turn_id: &str) -> String {
        let action = match self {
            Self::Start => "start",
            Self::Prompt => "prompt",
            Self::Stop => "stop",
        };
        format!("acp-{turn_id}-{action}")
    }
}

#[async_trait::async_trait]
pub trait Driver: Send + Sync {
    /// Start/select a session without submitting its first prompt.
    async fn start(&self, owner: &Owner, action_id: &str) -> Result<Binding, String>;
    /// Load the exact stopped session, without submitting any prompt. Drivers
    /// that cannot prove this capability refuse; no fallback to session/new.
    async fn restore(&self, _: &Owner, _: &str, _: &Continuation) -> Result<Binding, String> {
        Err("ACP same-session restoration is unsupported".into())
    }
    async fn prompt(
        &self,
        owner: &Owner,
        binding: &Binding,
        action_id: &str,
        text: &str,
    ) -> Result<(), String>;
    async fn observe(
        &self,
        owner: &Owner,
        binding: &Binding,
        after: u64,
    ) -> Result<Observation, String>;
    async fn permission(
        &self,
        owner: &Owner,
        binding: &Binding,
        action_id: &str,
        request_id: &str,
        option_id: &str,
    ) -> Result<(), String>;
    /// Reconcile this original owned session; missing evidence is not release.
    async fn stop(
        &self,
        owner: &Owner,
        binding: Option<&Binding>,
        action_id: &str,
    ) -> Result<Stop, String>;
}

#[derive(Clone)]
pub(in crate::service) struct Job {
    pub(in crate::service) source: GoalWork,
    pub(in crate::service) control: Arc<dyn WorkControl>,
    pub(in crate::service) driver: Arc<dyn Driver>,
    pub(in crate::service) continuation: Option<Continuation>,
}

pub struct Approval {
    pub turn_id: String,
    pub request_id: String,
    pub option_id: String,
    pub idempotency_key: String,
}

pub(in crate::service) struct Choice {
    pub(in crate::service) request_id: String,
    pub(in crate::service) option_id: String,
    pub(in crate::service) action_id: String,
}

impl ThreadService {
    /// Recheck the shared owner after an adapter waits for transport/process
    /// preparation. This is a fenced dispatch check, not evidence of delivery.
    pub async fn authorize_acp_dispatch(
        &self,
        owner: &Owner,
        action_id: &str,
        control: &dyn WorkControl,
    ) -> Result<(), ServiceError> {
        control
            .authorize()
            .await
            .map_err(|error| ServiceError::new(ErrorCode::Unauthorized, error.to_string()))?;
        self.check_acp_owner(owner)?;
        self.commit_records(
            &owner.turn_id,
            &[crate::store::ExecutionRecord::AcpDispatchGate {
                owner: owner.clone(),
                action_id: action_id.into(),
            }],
        )
        .await?;
        control
            .authorize()
            .await
            .map_err(|error| ServiceError::new(ErrorCode::Unauthorized, error.to_string()))?;
        self.check_acp_owner(owner)
    }

    fn check_acp_owner(&self, owner: &Owner) -> Result<(), ServiceError> {
        self.ensure_instance(Some(&owner.server_instance_id))?;
        let state = self.lock_state();
        let turn = state
            .turns
            .get(&owner.turn_id)
            .filter(|turn| turn.thread_id == owner.thread_id)
            .ok_or_else(crate::service::unknown_turn)?;
        let thread = state
            .threads
            .get(&owner.thread_id)
            .ok_or("ACP Thread disappeared")?;
        self.check_thread_grant(&state, thread)?;
        if !matches!(thread.config, crate::thread::executor::Config::Acp { .. })
            || thread.snapshot.active_turn_id.as_ref() != Some(&owner.turn_id)
            || state.closing
            || turn.cancel.is_cancelled()
            || turn.fence.pending()
            || turn.storage_error.is_some()
            || turn.snapshot.status.terminal()
            || turn.snapshot.status == crate::turn::TurnStatus::RecoveryRequired
        {
            return Err("ACP Turn owner is closed for dispatch".into());
        }
        Ok(())
    }

    pub async fn start_goal_acp_turn(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        source: GoalWork,
        request: TurnRequest,
        control: Arc<dyn WorkControl>,
        driver: Arc<dyn Driver>,
    ) -> Result<TurnReceipt, ServiceError> {
        control
            .authorize()
            .await
            .map_err(|error| ServiceError::new(ErrorCode::Unauthorized, error.to_string()))?;
        let view = self.load_thread(target, caller).await?;
        if view.thread.goal_work.as_ref() != Some(&source)
            || !matches!(&view.config, crate::thread::executor::Config::Acp { .. })
        {
            return Err(ServiceError::new(
                ErrorCode::Unauthorized,
                "ACP source does not match this Thread",
            ));
        }
        if let Some(recovery) = view.recovery {
            if view.latest_turn.is_some() || !recovery.terminal_checkpoint {
                return Err(ServiceError::new(
                    ErrorCode::RecoveryRequired,
                    "ACP continuation requires the original session and stopped-owner evidence",
                ));
            }
            // Reuse idle recovery only for metadata that never admitted a
            // Turn. This cannot import private ACP state or resubmit a prompt.
            self.recover_thread_controlled(
                target,
                caller,
                crate::thread::ThreadRecoveryRequest {
                    source_server_instance_id: recovery.source_server_instance_id,
                    source_cursor: recovery.source_cursor,
                    idempotency_key: format!(
                        "{}-restore-{}",
                        request.idempotency_key, recovery.source_cursor
                    ),
                },
                Some(control.clone()),
            )
            .await?;
        }
        self.admit_turn(
            target,
            caller,
            request,
            true,
            true,
            Some(super::Managed::Acp(Box::new(Job {
                source,
                control,
                driver,
                continuation: None,
            }))),
        )
        .await
    }
    /// Continue only an already accepted, inspected pre-prompt session. The
    /// original durable input/key remain authoritative; no new Turn is accepted.
    pub async fn resume_goal_acp_turn(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        source: GoalWork,
        key: &str,
        control: Arc<dyn WorkControl>,
        driver: Arc<dyn Driver>,
    ) -> Result<TurnReceipt, ServiceError> {
        control
            .authorize()
            .await
            .map_err(|error| ServiceError::new(ErrorCode::Unauthorized, error.to_string()))?;
        let view = self.load_thread(target, caller).await?;
        if view.thread.goal_work.as_ref() != Some(&source)
            || !matches!(view.config, crate::thread::executor::Config::Acp { .. })
        {
            return Err(ServiceError::new(
                ErrorCode::Unauthorized,
                "ACP continuation source mismatch",
            ));
        }
        let original = self
            .find_goal_worker_turn(target, caller, key)
            .await?
            .ok_or_else(|| {
                ServiceError::new(ErrorCode::UnknownTurn, "original ACP action missing")
            })?;
        if let Some(report) = view.recovery {
            if report
                .turn
                .as_ref()
                .is_none_or(|turn| turn.turn_id != original.turn_id)
            {
                return Err(ServiceError::new(
                    ErrorCode::RecoveryRequired,
                    "original ACP checkpoint missing",
                ));
            }
            self.recover_thread_with_acp(
                target,
                caller,
                crate::thread::ThreadRecoveryRequest {
                    source_server_instance_id: report.source_server_instance_id,
                    source_cursor: report.source_cursor,
                    idempotency_key: format!("{key}-restore-{}", report.source_cursor),
                },
                Some(control.clone()),
                Some(Job {
                    source,
                    control,
                    driver,
                    continuation: None,
                }),
            )
            .await?;
        }
        self.find_goal_worker_turn(target, caller, key)
            .await?
            .ok_or_else(|| {
                ServiceError::new(
                    ErrorCode::StorageUnavailable,
                    "original ACP receipt disappeared",
                )
            })
    }
}

mod execution;
mod permission;
