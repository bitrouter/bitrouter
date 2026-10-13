//! A durable, single-Thread close operation outlives its transport caller.

use bitrouter_sdk::caller::CallerContext;

use super::admission::{fingerprint, key_scope};
use super::commit::lifecycle_fact;
use super::threads::unknown_thread;
use super::{ErrorCode, ServiceError, ThreadService, now_ms};
use crate::store::{AcceptedKey, ExecutionRecord};
use crate::thread::{ThreadCloseReceipt, ThreadStatus, ThreadTarget};
use crate::turn::{TurnEvent, TurnEventPayload, TurnSnapshot, TurnStatus, VerificationStatus};

impl ThreadService {
    /// Cancel active work and every accepted unstarted input, retaining history.
    /// The owned operation continues if the requesting client drops this future.
    pub async fn close_thread(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        idempotency_key: String,
    ) -> Result<ThreadCloseReceipt, ServiceError> {
        let (send, receive) = tokio::sync::oneshot::channel();
        let service = self.clone();
        let target = target.clone();
        let caller = caller.clone();
        self.inner.workers.spawn(async move {
            let result = service.close_owned(&target, &caller, idempotency_key).await;
            let _ = send.send(result);
        });
        receive.await.map_err(|_| {
            ServiceError::new(
                ErrorCode::RecoveryRequired,
                "close owner stopped unexpectedly",
            )
        })?
    }

    async fn close_owned(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        idempotency_key: String,
    ) -> Result<ThreadCloseReceipt, ServiceError> {
        self.load_thread(target, caller).await?;
        let admission = self.inner.admission.lock().await;
        let scope = key_scope(caller, Some(&target.thread_id), "close", &idempotency_key)?;
        let hash = fingerprint(&target.thread_id)?;
        let accepted = self.accepted_key(&scope, &idempotency_key, &hash).await?;
        let gate = self.thread_gate(&target.thread_id)?;
        let guard = gate.lock().await;
        if accepted.is_some()
            && let Some(snapshot) = self
                .completed_close(&target.thread_id, &idempotency_key)
                .await?
        {
            return Ok(ThreadCloseReceipt {
                snapshot,
                replayed: true,
            });
        }
        if accepted.is_none() {
            let (mut snapshot, messages, queued, active) = {
                let state = self.lock_state();
                let thread = state
                    .threads
                    .get(&target.thread_id)
                    .ok_or_else(unknown_thread)?;
                thread.authorize(caller)?;
                self.check_thread_grant(&state, thread)?;
                if state.closing {
                    return Err(ServiceError::new(
                        ErrorCode::ShuttingDown,
                        "runtime is shutting down",
                    ));
                }
                if matches!(
                    thread.snapshot.status,
                    ThreadStatus::Closing | ThreadStatus::RecoveryRequired
                ) {
                    return Err(ServiceError::new(
                        ErrorCode::Conflict,
                        "Thread is closing or requires recovery",
                    ));
                }
                (
                    thread.snapshot.clone(),
                    thread.messages.clone(),
                    thread.queued.clone(),
                    thread.snapshot.active_turn_id.clone(),
                )
            };
            snapshot.status = ThreadStatus::Closing;
            snapshot.pause_reason = Some("explicit session close".into());
            snapshot.queued.clear();
            snapshot.waiting_for_capacity = false;
            let mut facts = vec![
                ExecutionRecord::ThreadCloseRequested {
                    idempotency_key: idempotency_key.clone(),
                },
                ExecutionRecord::AcceptedKey {
                    entry: AcceptedKey {
                        scope,
                        key: idempotency_key.clone(),
                        fingerprint: hash,
                        thread_id: target.thread_id.clone(),
                        turn_id: None,
                    },
                },
            ];
            let mut outcomes = Vec::new();
            for entry in &queued {
                let payload = TurnEventPayload::Finished {
                    status: TurnStatus::Cancelled,
                    detail: "queued Turn cancelled by explicit session close before activation"
                        .into(),
                    final_answer: None,
                    verification: VerificationStatus::Unavailable,
                    verification_evidence: None,
                    unknown_effect: false,
                };
                facts.push(ExecutionRecord::QueuedTurnCancelled {
                    turn_id: entry.turn_id.clone(),
                });
                facts.push(ExecutionRecord::TurnRecord {
                    turn_id: entry.turn_id.clone(),
                    fact: Box::new(
                        lifecycle_fact(&TurnEvent {
                            thread_id: target.thread_id.clone(),
                            server_instance_id: self.inner.instance_id.clone(),
                            turn_id: entry.turn_id.clone(),
                            seq: 1,
                            timestamp_ms: now_ms(),
                            payload: payload.clone(),
                        })
                        .ok_or("missing close cancellation lifecycle")?,
                    ),
                });
                outcomes.push((entry.turn_id.clone(), payload));
            }
            if let Some(turn_id) = &active {
                facts.push(ExecutionRecord::TurnRecord {
                    turn_id: turn_id.clone(),
                    fact: Box::new(ExecutionRecord::TurnLifecycle {
                        turn_id: turn_id.clone(),
                        lifecycle: crate::turn::TurnLifecycle::CancelRequested,
                    }),
                });
            }
            facts.push(ExecutionRecord::ThreadCheckpoint {
                snapshot: snapshot.clone(),
                messages,
            });
            self.commit_thread_serialized(&target.thread_id, &facts)
                .await?;
            let mut state = self.lock_state();
            let thread = state
                .threads
                .get_mut(&target.thread_id)
                .ok_or_else(unknown_thread)?;
            snapshot.cursor = thread.store_version;
            thread.snapshot = snapshot;
            thread.queued.clear();
            state.ready_threads.retain(|id| id != &target.thread_id);
            for (turn_id, payload) in outcomes {
                self.append_locked(&mut state, &turn_id, payload)?;
            }
            if let Some(turn_id) = active {
                self.append_locked(&mut state, &turn_id, TurnEventPayload::CancelRequested)?;
                if let Some(turn) = state.turns.get(&turn_id) {
                    turn.cancel.cancel();
                }
            }
        }
        drop(guard);
        drop(admission);
        self.wait_thread_workers(&target.thread_id).await;
        let guard = gate.lock().await;
        if let Some(snapshot) = self
            .completed_close(&target.thread_id, &idempotency_key)
            .await?
        {
            return Ok(ThreadCloseReceipt {
                snapshot,
                replayed: true,
            });
        }
        {
            let (mut snapshot, messages) = {
                let state = self.lock_state();
                let thread = state
                    .threads
                    .get(&target.thread_id)
                    .ok_or_else(unknown_thread)?;
                if thread.snapshot.status != ThreadStatus::Closing
                    || thread.snapshot.active_turn_id.is_some()
                    || !thread.queued.is_empty()
                    || thread.storage_error.is_some()
                {
                    return Err(ServiceError::new(
                        ErrorCode::RecoveryRequired,
                        "close cleanup or storage is unconfirmed",
                    ));
                }
                (thread.snapshot.clone(), thread.messages.clone())
            };
            snapshot.status = ThreadStatus::Paused;
            snapshot.pause_reason = Some("session closed; history retained".into());
            self.commit_thread_serialized(
                &target.thread_id,
                &[
                    ExecutionRecord::ThreadCloseCompleted {
                        idempotency_key,
                        snapshot: snapshot.clone(),
                    },
                    ExecutionRecord::ThreadCheckpoint {
                        snapshot: snapshot.clone(),
                        messages,
                    },
                ],
            )
            .await?;
            let mut state = self.lock_state();
            let thread = state
                .threads
                .get_mut(&target.thread_id)
                .ok_or_else(unknown_thread)?;
            snapshot.cursor = thread.store_version;
            thread.snapshot = snapshot;
        }
        drop(guard);
        Ok(ThreadCloseReceipt {
            snapshot: self.read_thread(target, caller)?,
            replayed: false,
        })
    }

    async fn completed_close(
        &self,
        thread_id: &str,
        key: &str,
    ) -> Result<Option<crate::thread::ThreadSnapshot>, ServiceError> {
        let mut completed = None;
        self.scan_receipt(thread_id, |record| {
            if let ExecutionRecord::ThreadCloseCompleted {
                idempotency_key,
                snapshot,
            } = record
                && idempotency_key == key
            {
                completed = Some(snapshot.clone());
            }
        })
        .await?;
        Ok(completed)
    }

    /// A retired ACP attachment can acknowledge only its already completed close.
    #[cfg(feature = "acp")]
    pub(crate) async fn replay_close(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        key: &str,
    ) -> Result<(), ServiceError> {
        self.load_thread(target, caller).await?;
        let scope = key_scope(caller, Some(&target.thread_id), "close", key)?;
        let hash = fingerprint(&target.thread_id)?;
        if self.accepted_key(&scope, key, &hash).await?.is_some()
            && self
                .completed_close(&target.thread_id, key)
                .await?
                .is_some()
        {
            return Ok(());
        }
        Err(ServiceError::new(
            ErrorCode::Conflict,
            "reopen the closed attachment before a new operation",
        ))
    }

    async fn wait_thread_workers(&self, thread_id: &str) {
        loop {
            let notified = self.inner.runtime_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self
                .lock_state()
                .running_turns
                .values()
                .any(|id| id == thread_id)
            {
                return;
            }
            notified.await;
        }
    }

    /// Completion includes the native worker's exit, not just its terminal fact.
    pub async fn wait_turn_settled(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        turn_id: &str,
    ) -> Result<TurnSnapshot, ServiceError> {
        loop {
            let notified = self.inner.runtime_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let snapshot = match self.read_turn(target, caller, turn_id) {
                Ok(snapshot) => snapshot,
                Err(error) if error.code == ErrorCode::UnknownTurn => {
                    self.read_stored_turn(target, caller, turn_id).await?
                }
                Err(error) => return Err(error),
            };
            if snapshot.status == TurnStatus::RecoveryRequired {
                return Err(ServiceError::new(
                    ErrorCode::RecoveryRequired,
                    snapshot
                        .detail
                        .unwrap_or_else(|| "Turn requires recovery".into()),
                ));
            }
            if snapshot.status.terminal() && !self.lock_state().running_turns.contains_key(turn_id)
            {
                return Ok(snapshot);
            }
            notified.await;
        }
    }
}
