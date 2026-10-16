//! Native delegated work uses the existing Thread admission and Agent runner.
use std::sync::Arc;

use bitrouter_sdk::caller::CallerContext;

use super::{ErrorCode, ServiceError, ThreadService};
use crate::control::WorkControl;
use crate::thread::{GoalWork, ThreadTarget};
use crate::turn::{TurnReceipt, TurnRequest};

pub fn thread_key(source: &GoalWork) -> Result<String, ServiceError> {
    super::admission::fingerprint(source).map(|hash| format!("goal-worker-{hash}"))
}

#[derive(Clone)]
pub(super) enum Managed {
    Generation(Box<super::generation::Job>),
    Native {
        source: GoalWork,
        control: Arc<dyn WorkControl>,
    },
}

impl Managed {
    pub(super) fn operation(&self) -> &'static str {
        match self {
            Self::Generation(_) => "generation",
            Self::Native { .. } => "goal_work",
        }
    }

    pub(super) fn fingerprint(&self, prompt: &str) -> Result<String, ServiceError> {
        match self {
            Self::Generation(job) => {
                super::admission::fingerprint(&(&job.request.origin, &job.request.prompt))
            }
            Self::Native { source, .. } => super::admission::fingerprint(&(source, prompt)),
        }
    }

    pub(super) fn control(&self) -> &Arc<dyn WorkControl> {
        match self {
            Self::Generation(job) => &job.control,
            Self::Native { control, .. } => control,
        }
    }
}

impl ThreadService {
    /// Reconcile a saved Goal action without authorizing or starting work.
    pub async fn find_goal_worker_turn(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        key: &str,
    ) -> Result<Option<TurnReceipt>, ServiceError> {
        let view = self.read_stored_thread_view(target, caller).await?;
        if view.thread.goal_work.is_none() {
            return Err("Thread is not a delegated Goal worker".into());
        }
        let scope = super::admission::key_scope(caller, Some(&target.thread_id), "goal_work", key)?;
        let Some(entry) = self
            .inner
            .store
            .find_key(&scope, key)
            .await
            .map_err(ServiceError::storage)?
        else {
            return Ok(None);
        };
        self.existing_turn_receipt(&entry).await.map(Some)
    }

    /// Only the trusted Goal host can attach the live source/accounting gate.
    /// Generic native RPCs cannot start work on an associated worker Thread.
    pub async fn start_goal_worker_turn(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        source: GoalWork,
        request: TurnRequest,
        control: Arc<dyn WorkControl>,
    ) -> Result<TurnReceipt, ServiceError> {
        control
            .authorize()
            .await
            .map_err(|error| ServiceError::new(ErrorCode::Unauthorized, error.to_string()))?;
        let view = self.load_thread(target, caller).await?;
        if self.read_thread(target, caller)?.goal_work.as_ref() != Some(&source) {
            return Err(ServiceError::new(
                ErrorCode::Unauthorized,
                "worker source does not match this Thread",
            ));
        }
        if let Some(recovery) = view.recovery {
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
            Some(Managed::Native { source, control }),
        )
        .await
    }
}
