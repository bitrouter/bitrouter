//! Wait only for known admission contention, without starting a second scheduler.
use std::sync::Arc;
use std::time::Duration;

use bitrouter_sdk::caller::CallerContext;

use super::generation::GenerationRequest;
use super::worker::Managed;
use crate::control::WorkControl;
use crate::service::{ErrorCode, ServiceError, ThreadService, admission, state::ThreadRecord};
use crate::thread::{GoalWork, ThreadStatus, ThreadTarget};
use crate::turn::{TurnReceipt, TurnRequest};

impl ThreadService {
    /// The recorded Thread duration also bounds pre-admission capacity waiting.
    /// Replays inspect the original receipt; only a new acceptance needs live
    /// authorization. No waiting state grants execution or consumes a Turn slot.
    pub async fn wait_goal_generation(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        request: GenerationRequest,
        control: Arc<dyn WorkControl>,
    ) -> Result<TurnReceipt, ServiceError> {
        let input = request.input()?;
        Box::pin(self.wait_managed_turn(
            target,
            caller,
            input,
            Managed::Generation(Box::new(super::generation::Job { request, control })),
        ))
        .await
    }

    /// Delegated work waits on the same runtime capacity as coordination. A
    /// lost lease, revoked source or unresolved workspace is never contention.
    pub async fn wait_goal_worker_turn(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        source: GoalWork,
        request: TurnRequest,
        control: Arc<dyn WorkControl>,
    ) -> Result<TurnReceipt, ServiceError> {
        Box::pin(self.wait_managed_turn(
            target,
            caller,
            request,
            Managed::Native { source, control },
        ))
        .await
    }

    async fn wait_managed_turn(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        request: TurnRequest,
        managed: Managed,
    ) -> Result<TurnReceipt, ServiceError> {
        let view = self.read_stored_thread_view(target, caller).await?;
        let deadline = tokio::time::Instant::now() + view.config.max_duration;
        let key = &request.idempotency_key;
        let scope =
            admission::key_scope(caller, Some(&target.thread_id), managed.operation(), key)?;
        let hash = managed.fingerprint(&request.prompt)?;
        let extra = match &managed {
            Managed::Generation(job) => serde_json::to_vec(&job.request.prompt)
                .map_err(|error| error.to_string())?
                .len(),
            Managed::Native { .. } => request.prompt.len().saturating_mul(2),
        };
        let mut refusal: Option<ServiceError> = None;
        loop {
            let changed = self.inner.runtime_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(receipt) = self.accepted_key(&scope, key, &hash).await? {
                return self.existing_turn_receipt(&receipt).await;
            }
            if let Some(error) = &refusal
                && tokio::time::Instant::now() >= deadline
            {
                return Err(ServiceError::new(
                    ErrorCode::Overloaded,
                    format!(
                        "Goal admission wait expired for {key}; no confirmed receipt at the last lookup; last refusal: {error}"
                    ),
                ));
            }
            managed
                .control()
                .authorize()
                .await
                .map_err(|error| ServiceError::new(ErrorCode::Unauthorized, error.to_string()))?;
            if refusal.is_some() && tokio::time::Instant::now() >= deadline {
                continue;
            }
            // The two drivers compose SDK, SQLite and native recovery futures.
            // Keep that state out of the calling coordinator's worker stack.
            let result = match managed.clone() {
                Managed::Generation(job) => {
                    Box::pin(self.start_goal_generation(target, caller, job.request, job.control))
                        .await
                }
                Managed::Native { source, control } => {
                    Box::pin(self.start_goal_worker_turn(
                        target,
                        caller,
                        source,
                        request.clone(),
                        control,
                    ))
                    .await
                }
            };
            match result {
                Ok(receipt) => return Ok(receipt),
                Err(error) => {
                    // Recheck the exact body/key before classifying contention.
                    // A racing acceptance can neither be lost nor converted
                    // into permission to accept another Turn.
                    if let Some(receipt) = self.accepted_key(&scope, key, &hash).await? {
                        return self.existing_turn_receipt(&receipt).await;
                    }
                    if !self.managed_capacity_wait(target, caller, &managed, extra, error.code)? {
                        return Err(error);
                    }
                    refusal = Some(error);
                }
            }
            tokio::select! {
                () = &mut changed => {},
                () = tokio::time::sleep_until(deadline.min(
                    tokio::time::Instant::now() + Duration::from_millis(100)
                )) => {},
            }
        }
    }

    fn managed_capacity_wait(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        managed: &Managed,
        extra: usize,
        code: ErrorCode,
    ) -> Result<bool, ServiceError> {
        let state = self.lock_state();
        let thread = state
            .threads
            .get(&target.thread_id)
            .ok_or_else(super::threads::unknown_thread)?;
        thread.authorize(caller)?;
        self.check_thread_grant(&state, thread)?;
        if state.closing {
            return Err(ServiceError::new(
                ErrorCode::ShuttingDown,
                "runtime is shutting down",
            ));
        }
        let source_matches = match managed {
            Managed::Generation(_) => thread.snapshot.is_goal_coordination(),
            Managed::Native { source, .. } => {
                !thread.snapshot.is_goal_coordination()
                    && thread.snapshot.goal_work.as_ref() == Some(source)
            }
        };
        if !source_matches
            || thread.storage_error.is_some()
            || matches!(
                thread.snapshot.status,
                ThreadStatus::RecoveryRequired | ThreadStatus::Closing
            )
            || !thread.queued.is_empty()
            || (matches!(managed, Managed::Generation(_))
                && extra > thread.config.max_context_bytes)
            || thread.context_bytes().saturating_add(extra)
                > self.inner.limits.context_bytes_per_thread
            || state
                .threads
                .values()
                .map(ThreadRecord::bytes)
                .sum::<usize>()
                .saturating_add(extra)
                > self.inner.limits.hot_context_bytes
        {
            return Ok(false);
        }
        let workspace_owner = if matches!(managed, Managed::Native { .. }) {
            self.workspace_owner_error(&state, &thread.snapshot.workspace)
        } else {
            None
        };
        if workspace_owner
            .as_ref()
            .is_some_and(|error| error.code != ErrorCode::Conflict)
        {
            return Ok(false);
        }
        Ok(match code {
            ErrorCode::Conflict => {
                (thread.snapshot.status == ThreadStatus::Busy
                    && thread.snapshot.active_turn_id.is_some())
                    || workspace_owner.is_some()
            }
            ErrorCode::Overloaded => self.check_active_turn_limit(&state).is_err(),
            _ => false,
        })
    }
}
