//! Native delegated work uses the existing Thread admission and Agent runner.
use std::sync::Arc;

use bitrouter_sdk::caller::CallerContext;

use super::{ErrorCode, ServiceError, ThreadService};
use crate::control::WorkControl;
use crate::thread::{GoalWork, ThreadTarget};
use crate::turn::{TurnReceipt, TurnRequest};

pub mod acp;

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
    Acp(Box<acp::Job>),
}

impl Managed {
    pub(super) fn operation(&self) -> &'static str {
        match self {
            Self::Generation(_) => "generation",
            Self::Native { .. } | Self::Acp(_) => "goal_work",
        }
    }

    pub(super) fn fingerprint(&self, prompt: &str) -> Result<String, ServiceError> {
        match self {
            Self::Generation(job) => {
                super::admission::fingerprint(&(&job.request.origin, &job.request.prompt))
            }
            Self::Native { source, .. } => super::admission::fingerprint(&(source, prompt)),
            Self::Acp(job) => super::admission::fingerprint(&(&job.source, prompt)),
        }
    }

    pub(super) fn control(&self) -> &Arc<dyn WorkControl> {
        match self {
            Self::Generation(job) => &job.control,
            Self::Native { control, .. } => control,
            Self::Acp(job) => &job.control,
        }
    }
}

impl ThreadService {
    /// Read the original Thread and Turn keys under the admission lock. The
    /// host must revoke this attempt's source before treating absence as a
    /// settled refusal. This never authorizes work or proves ambient effects
    /// outside the managed Turn absent.
    pub async fn find_goal_worker_admission(
        &self,
        server_instance_id: &str,
        caller: &CallerContext,
        source: &GoalWork,
        turn_key: &str,
    ) -> Result<Option<TurnReceipt>, ServiceError> {
        source.validate()?;
        self.ensure_instance(Some(server_instance_id))?;
        let _admission = self.inner.admission.lock().await;
        let key = thread_key(source)?;
        let scope = super::admission::key_scope(caller, None, "create_thread", &key)?;
        let Some(entry) = self
            .inner
            .store
            .find_key(&scope, &key)
            .await
            .map_err(ServiceError::storage)?
        else {
            return Ok(None);
        };
        let thread = self.existing_thread_receipt(&entry).await?;
        self.check_snapshot_grant(&thread)?;
        if thread.goal_work.as_ref() != Some(source) {
            return Err("original worker Thread belongs to another source".into());
        }
        let scope =
            super::admission::key_scope(caller, Some(&thread.thread_id), "goal_work", turn_key)?;
        let Some(entry) = self
            .inner
            .store
            .find_key(&scope, turn_key)
            .await
            .map_err(ServiceError::storage)?
        else {
            return Ok(None);
        };
        self.existing_turn_receipt(&entry).await.map(Some)
    }

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
        let managed = Managed::Native { source, control };
        if let Some(recovery) = view.recovery {
            // Reject a changed retry body before recovery can dispatch the
            // original continuation. Reconciliation is a separate entrypoint.
            let scope = super::admission::key_scope(
                caller,
                Some(&target.thread_id),
                managed.operation(),
                &request.idempotency_key,
            )?;
            self.accepted_key(
                &scope,
                &request.idempotency_key,
                &managed.fingerprint(&request.prompt)?,
            )
            .await?;
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
                Some(managed.control().clone()),
            )
            .await?;
        }
        self.admit_turn(target, caller, request, true, true, Some(managed))
            .await
    }

    /// Resume one already accepted native Turn from its inspected checkpoint.
    /// Its durable input is authoritative; no prompt is resubmitted and this
    /// entrypoint cannot create a Thread or accept another Turn.
    pub async fn resume_goal_worker_turn(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        source: &GoalWork,
        turn_id: &str,
        key: &str,
        control: Arc<dyn WorkControl>,
    ) -> Result<TurnReceipt, ServiceError> {
        control
            .authorize()
            .await
            .map_err(|error| ServiceError::new(ErrorCode::Unauthorized, error.to_string()))?;
        let view = self.load_thread(target, caller).await?;
        view.config.native()?;
        if view.thread.goal_work.as_ref() != Some(source) {
            return Err(ServiceError::new(
                ErrorCode::Unauthorized,
                "native continuation belongs to another Goal work source",
            ));
        }
        let receipt = self
            .find_goal_worker_turn(target, caller, key)
            .await?
            .ok_or_else(|| {
                ServiceError::new(
                    ErrorCode::UnknownTurn,
                    "original worker action was not accepted",
                )
            })?;
        if receipt.turn_id != turn_id {
            return Err(ServiceError::new(
                ErrorCode::Conflict,
                "original worker action belongs to another Turn",
            ));
        }
        if let Some(recovery) = view.recovery {
            if recovery
                .turn
                .as_ref()
                .is_some_and(|turn| turn.turn_id != turn_id)
            {
                return Err(ServiceError::new(
                    ErrorCode::Conflict,
                    "inspected continuation belongs to another Turn",
                ));
            }
            self.recover_thread_controlled(
                target,
                caller,
                crate::thread::ThreadRecoveryRequest {
                    source_server_instance_id: recovery.source_server_instance_id,
                    source_cursor: recovery.source_cursor,
                    idempotency_key: format!("{key}-restore-{}", recovery.source_cursor),
                },
                Some(control),
            )
            .await?;
        }
        self.find_goal_worker_turn(target, caller, key)
            .await?
            .ok_or_else(|| {
                ServiceError::new(
                    ErrorCode::StorageUnavailable,
                    "original worker receipt disappeared after recovery",
                )
            })
    }
}
