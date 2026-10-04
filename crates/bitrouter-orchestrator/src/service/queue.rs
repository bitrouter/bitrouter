//! Advance accepted FIFO work without bypassing the active Turn or safe checkpoints.

use std::sync::Arc;
use std::time::Duration;

use bitrouter_sdk::caller::CallerContext;

use super::admission::{fingerprint, key_scope};
use super::commit::lifecycle_fact;
use super::threads::unknown_thread;
use super::{ErrorCode, ServiceError, ThreadService, now_ms, unknown_turn};
use crate::agent::Agent;
use crate::store::{AcceptedKey, ExecutionRecord};
use crate::thread::{ThreadSnapshot, ThreadStatus, ThreadTarget};
use crate::turn::{TurnEvent, TurnEventPayload, TurnReceipt, TurnStatus, VerificationStatus};

impl ThreadService {
    pub async fn cancel_queued_turn(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        turn_id: &str,
        idempotency_key: String,
    ) -> Result<TurnReceipt, ServiceError> {
        self.ensure_instance(Some(&target.server_instance_id))?;
        let admission = self.inner.admission.lock().await;
        let scope = key_scope(
            caller,
            Some(&target.thread_id),
            "cancel_queued",
            &idempotency_key,
        )?;
        let hash = fingerprint(&turn_id)?;
        if let Some(entry) = self.accepted_key(&scope, &idempotency_key, &hash).await? {
            drop(admission);
            return self.existing_turn_receipt(&entry).await;
        }
        let gate = self.thread_gate(&target.thread_id)?;
        let _guard = gate.lock().await;
        let (entry, mut snapshot, messages, cursor) = {
            let state = self.lock_state();
            let thread = state
                .threads
                .get(&target.thread_id)
                .ok_or_else(unknown_thread)?;
            thread.authorize(caller)?;
            if state.closing {
                return Err(ServiceError::new(
                    ErrorCode::ShuttingDown,
                    "runtime is shutting down",
                ));
            }
            if thread.snapshot.status == ThreadStatus::RecoveryRequired {
                return Err(ServiceError::new(
                    ErrorCode::RecoveryRequired,
                    "Thread controls require recovery",
                ));
            }
            let entry = thread
                .queued
                .iter()
                .find(|entry| entry.turn_id == turn_id)
                .cloned()
                .ok_or_else(|| {
                    ServiceError::new(ErrorCode::Conflict, "Turn is no longer queued")
                })?;
            let task = state.turns.get(turn_id).ok_or_else(unknown_turn)?;
            (
                entry,
                thread.snapshot.clone(),
                thread.messages.clone(),
                task.snapshot.cursor,
            )
        };
        snapshot.queued.retain(|entry| entry.turn_id != turn_id);
        if snapshot.queued.is_empty() {
            snapshot.waiting_for_capacity = false;
        }
        snapshot.cursor = snapshot.cursor.saturating_add(4);
        let payload = TurnEventPayload::Finished {
            status: TurnStatus::Cancelled,
            detail: "queued Turn withdrawn before activation".into(),
            final_answer: None,
            verification: VerificationStatus::Unavailable,
            verification_evidence: None,
            unknown_effect: false,
        };
        let facts = [
            ExecutionRecord::QueuedTurnCancelled {
                turn_id: turn_id.into(),
            },
            ExecutionRecord::AcceptedKey {
                entry: AcceptedKey {
                    scope,
                    key: idempotency_key,
                    fingerprint: hash,
                    thread_id: target.thread_id.clone(),
                    turn_id: Some(turn_id.into()),
                },
            },
            ExecutionRecord::TurnRecord {
                turn_id: turn_id.into(),
                fact: Box::new(
                    lifecycle_fact(&TurnEvent {
                        thread_id: target.thread_id.clone(),
                        server_instance_id: self.inner.instance_id.clone(),
                        turn_id: turn_id.into(),
                        seq: cursor + 1,
                        timestamp_ms: now_ms(),
                        payload: payload.clone(),
                    })
                    .ok_or("missing cancellation lifecycle")?,
                ),
            },
            ExecutionRecord::ThreadCheckpoint {
                snapshot: snapshot.clone(),
                messages,
            },
        ];
        self.commit_thread_serialized(&target.thread_id, &facts)
            .await?;
        let mut state = self.lock_state();
        let thread = state
            .threads
            .get_mut(&target.thread_id)
            .ok_or_else(unknown_thread)?;
        snapshot.cursor = thread.store_version;
        thread.snapshot = snapshot;
        thread.queued.retain(|entry| entry.turn_id != turn_id);
        self.append_locked(&mut state, turn_id, payload)?;
        Ok(TurnReceipt {
            thread_id: target.thread_id.clone(),
            turn_id: turn_id.into(),
            queue_order: entry.order,
            status: TurnStatus::Cancelled,
        })
    }

    pub async fn resume_queue(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        idempotency_key: String,
    ) -> Result<ThreadSnapshot, ServiceError> {
        self.ensure_instance(Some(&target.server_instance_id))?;
        let admission = self.inner.admission.lock().await;
        let scope = key_scope(caller, Some(&target.thread_id), "resume", &idempotency_key)?;
        let hash = fingerprint(&target.thread_id)?;
        if let Some(entry) = self.accepted_key(&scope, &idempotency_key, &hash).await? {
            drop(admission);
            return self.existing_thread_receipt(&entry).await;
        }
        let gate = self.thread_gate(&target.thread_id)?;
        let guard = gate.lock().await;
        let (mut snapshot, messages) = {
            let state = self.lock_state();
            let thread = state
                .threads
                .get(&target.thread_id)
                .ok_or_else(unknown_thread)?;
            thread.authorize(caller)?;
            if state.closing {
                return Err(ServiceError::new(
                    ErrorCode::ShuttingDown,
                    "runtime is shutting down",
                ));
            }
            if thread.snapshot.status == ThreadStatus::RecoveryRequired
                || thread.snapshot.active_turn_id.is_some()
            {
                return Err(ServiceError::new(
                    ErrorCode::RecoveryRequired,
                    "an active or uncertain Turn blocks resume",
                ));
            }
            if thread.snapshot.status != ThreadStatus::Paused {
                return Err(ServiceError::new(
                    ErrorCode::Conflict,
                    "only a paused queue can resume",
                ));
            }
            self.check_thread_grant(&state, thread)?;
            Agent::new(
                Arc::clone(&self.inner.app),
                caller.clone(),
                &thread.snapshot.workspace,
                thread.config.clone(),
            )?;
            let mut snapshot = thread.snapshot.clone();
            snapshot.waiting_for_capacity = !thread.queued.is_empty()
                && self
                    .check_turn_capacity(&state, &thread.snapshot.workspace)
                    .is_err();
            (snapshot, thread.messages.clone())
        };
        snapshot.status = ThreadStatus::Idle;
        snapshot.pause_reason = None;
        snapshot.cursor = snapshot.cursor.saturating_add(3);
        self.commit_thread_serialized(
            &target.thread_id,
            &[
                ExecutionRecord::QueueResumed,
                ExecutionRecord::AcceptedKey {
                    entry: AcceptedKey {
                        scope,
                        key: idempotency_key,
                        fingerprint: hash,
                        thread_id: target.thread_id.clone(),
                        turn_id: None,
                    },
                },
                ExecutionRecord::ThreadCheckpoint {
                    snapshot: snapshot.clone(),
                    messages,
                },
            ],
        )
        .await?;
        {
            let mut state = self.lock_state();
            let thread = state
                .threads
                .get_mut(&target.thread_id)
                .ok_or_else(unknown_thread)?;
            snapshot.cursor = thread.store_version;
            thread.snapshot = snapshot.clone();
            if !thread.queued.is_empty() && !state.ready_threads.contains(&target.thread_id) {
                state.ready_threads.push_back(target.thread_id.clone());
            }
        }
        drop(guard);
        drop(admission);
        self.drive_queues().await;
        self.read_thread(target, caller)
    }

    pub(super) async fn drive_queues(&self) {
        let _admission = self.inner.admission.lock().await;
        let candidates = {
            let state = self.lock_state();
            if state.closing {
                return;
            }
            state.ready_threads.len()
        };
        for _ in 0..candidates {
            let Some(thread_id) = self.lock_state().ready_threads.pop_front() else {
                break;
            };
            let Ok(gate) = self.thread_gate(&thread_id) else {
                continue;
            };
            let _guard = gate.lock().await;
            let grant_error = {
                let state = self.lock_state();
                state.threads.get(&thread_id).and_then(|thread| {
                    self.check_thread_grant(&state, thread)
                        .and_then(|()| {
                            if thread.snapshot.status == ThreadStatus::Idle
                                && thread.snapshot.active_turn_id.is_none()
                                && !thread.queued.is_empty()
                            {
                                self.check_turn_capacity(&state, &thread.snapshot.workspace)
                            } else {
                                Ok(())
                            }
                        })
                        .err()
                })
            };
            if let Some(error) = grant_error {
                if matches!(error.code, ErrorCode::Conflict | ErrorCode::Overloaded) {
                    let mut state = self.lock_state();
                    if let Some(thread) = state.threads.get_mut(&thread_id) {
                        thread.snapshot.waiting_for_capacity = true;
                        thread.presentation.waiting_for_capacity();
                    }
                    state.ready_threads.push_back(thread_id.clone());
                } else {
                    let status = if error.code == ErrorCode::RecoveryRequired {
                        ThreadStatus::RecoveryRequired
                    } else {
                        ThreadStatus::Paused
                    };
                    let _ = self
                        .checkpoint_queue_serialized(&thread_id, error.to_string(), status)
                        .await;
                }
                continue;
            }
            let activation = {
                let mut state = self.lock_state();
                let Some(thread) = state.threads.get(&thread_id) else {
                    continue;
                };
                if thread.snapshot.status != ThreadStatus::Idle
                    || thread.snapshot.active_turn_id.is_some()
                    || thread.queued.is_empty()
                {
                    continue;
                }
                if self
                    .check_turn_capacity(&state, &thread.snapshot.workspace)
                    .is_err()
                {
                    if let Some(thread) = state.threads.get_mut(&thread_id) {
                        thread.snapshot.waiting_for_capacity = true;
                        thread.presentation.waiting_for_capacity();
                    }
                    state.ready_threads.push_back(thread_id.clone());
                    continue;
                }
                let Some(entry) = thread.queued.front().cloned() else {
                    continue;
                };
                (
                    entry,
                    thread.caller.clone(),
                    thread.config.clone(),
                    thread.snapshot.workspace.clone(),
                    thread.messages.clone(),
                    thread.snapshot.context_version,
                    thread.verification_command.clone(),
                )
            };
            let (entry, caller, config, workspace, messages, version, verification) = activation;
            let agent = match Agent::new(
                Arc::clone(&self.inner.app),
                caller,
                &workspace,
                config.clone(),
            ) {
                Ok(agent) => agent.with_tool_workers(
                    Arc::clone(&self.inner.tool_workers),
                    self.inner.limits.tools_per_turn,
                ),
                Err(error) => {
                    let _ = self.pause_thread_serialized(&thread_id, error).await;
                    continue;
                }
            };
            if let Err(error) = self.reserve_workspace(&workspace, &entry.turn_id).await {
                if error.code == ErrorCode::Conflict {
                    let mut state = self.lock_state();
                    if let Some(thread) = state.threads.get_mut(&thread_id) {
                        thread.snapshot.waiting_for_capacity = true;
                        thread.presentation.waiting_for_capacity();
                    }
                    state.ready_threads.push_back(thread_id.clone());
                } else {
                    let status = if error.code == ErrorCode::RecoveryRequired {
                        ThreadStatus::RecoveryRequired
                    } else {
                        ThreadStatus::Paused
                    };
                    let _ = self
                        .checkpoint_queue_serialized(&thread_id, error.to_string(), status)
                        .await;
                }
                continue;
            }
            let payload = TurnEventPayload::Accepted {
                user_item_id: entry.user_item_id.clone(),
                prompt: entry.prompt.clone(),
                workspace: workspace.clone(),
                model: config.model.clone(),
                tool_mode: config.tool_mode(),
                idempotency_key: None,
                request_fingerprint: None,
            };
            if self
                .commit_thread_serialized(
                    &thread_id,
                    &[ExecutionRecord::TurnActivated {
                        turn_id: entry.turn_id.clone(),
                        context_version: version.saturating_add(1),
                    }],
                )
                .await
                .is_err()
            {
                continue;
            }
            let cancel = {
                let mut state = self.lock_state();
                let Some(thread) = state.threads.get_mut(&thread_id) else {
                    continue;
                };
                thread.queued.pop_front();
                thread
                    .snapshot
                    .queued
                    .retain(|receipt| receipt.turn_id != entry.turn_id);
                thread.snapshot.status = ThreadStatus::Busy;
                thread.snapshot.active_turn_id = Some(entry.turn_id.clone());
                thread.snapshot.context_version = version.saturating_add(1);
                thread.snapshot.waiting_for_capacity = false;
                state
                    .active_workspaces
                    .insert(workspace.clone(), entry.turn_id.clone());
                let Some(cancel) = state
                    .turns
                    .get(&entry.turn_id)
                    .map(|task| task.cancel.clone())
                else {
                    continue;
                };
                if self
                    .append_locked(&mut state, &entry.turn_id, payload)
                    .is_err()
                {
                    cancel.cancel();
                    continue;
                }
                cancel
            };
            self.spawn_thread_turn(entry, agent, (messages, version), verification, cancel);
        }
        self.schedule_queue_retry();
    }

    pub(super) fn schedule_queue_retry(&self) {
        use std::sync::atomic::Ordering;
        let state = self.lock_state();
        if state.closing
            || state.ready_threads.is_empty()
            || self.inner.queue_waker_started.swap(true, Ordering::AcqRel)
        {
            return;
        }
        // One owned waiter observes external releases without requiring another
        // client request. It stops when there is no eligible FIFO or on shutdown.
        self.inner.workers.spawn(self.queue_retry());
    }

    pub(super) fn queue_retry(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
        let service = self.clone();
        Box::pin(async move {
            loop {
                {
                    let state = service.lock_state();
                    if state.closing || state.ready_threads.is_empty() {
                        service
                            .inner
                            .queue_waker_started
                            .store(false, std::sync::atomic::Ordering::Release);
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                service.drive_queues().await;
            }
        })
    }

    pub(super) async fn pause_thread_serialized(
        &self,
        thread_id: &str,
        reason: String,
    ) -> Result<(), ServiceError> {
        self.checkpoint_queue_serialized(thread_id, reason, ThreadStatus::Paused)
            .await
    }

    pub(super) async fn checkpoint_queue_serialized(
        &self,
        thread_id: &str,
        reason: String,
        status: ThreadStatus,
    ) -> Result<(), ServiceError> {
        let (mut snapshot, messages) = {
            let state = self.lock_state();
            let thread = state.threads.get(thread_id).ok_or_else(unknown_thread)?;
            if thread.snapshot.status == ThreadStatus::RecoveryRequired
                || thread.snapshot.active_turn_id.is_some()
            {
                return Err(ServiceError::new(
                    ErrorCode::RecoveryRequired,
                    "active or uncertain Turn blocks a queue checkpoint",
                ));
            }
            (thread.snapshot.clone(), thread.messages.clone())
        };
        snapshot.status = status;
        snapshot.pause_reason = Some(reason);
        snapshot.waiting_for_capacity = false;
        snapshot.cursor = snapshot.cursor.saturating_add(1);
        self.commit_thread_serialized(
            thread_id,
            &[ExecutionRecord::ThreadCheckpoint {
                snapshot: snapshot.clone(),
                messages,
            }],
        )
        .await?;
        let mut state = self.lock_state();
        let thread = state
            .threads
            .get_mut(thread_id)
            .ok_or_else(unknown_thread)?;
        snapshot.cursor = thread.store_version;
        thread.snapshot = snapshot;
        state.ready_threads.retain(|id| id != thread_id);
        Ok(())
    }

    pub(super) async fn pause_queues_after_shutdown(&self) {
        let ids = self
            .lock_state()
            .threads
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for id in ids {
            let Ok(gate) = self.thread_gate(&id) else {
                continue;
            };
            let _guard = gate.lock().await;
            if self
                .lock_state()
                .threads
                .get(&id)
                .is_some_and(|thread| thread.snapshot.status == ThreadStatus::Idle)
            {
                let _ = self
                    .pause_thread_serialized(
                        &id,
                        "runtime shut down; explicit recovery and resume required".into(),
                    )
                    .await;
            }
        }
    }
}
