//! Serialize facts, checkpoints and publication through the Thread commit gate.

use std::sync::Arc;
use std::time::Instant;

use bitrouter_sdk::language_model::{Message, Role};

use super::state::State;
use super::threads::unknown_thread;
use super::{ErrorCode, ServiceError, ThreadService, now_ms, unknown_turn};
use crate::agent::RunEvent;
use crate::item::MAX_LIVE_BYTES;
use crate::store::ExecutionRecord;
use crate::thread::ThreadStatus;
use crate::turn::{TurnEvent, TurnEventPayload, TurnStatus};

pub(super) fn lifecycle_fact(event: &TurnEvent) -> Option<ExecutionRecord> {
    use crate::turn::TurnLifecycle as L;
    let lifecycle = match &event.payload {
        TurnEventPayload::Started => L::Started,
        TurnEventPayload::InputRequested {
            request_id,
            tool_id,
            tool_name,
            arguments,
        } => L::InputRequested {
            request_id: request_id.clone(),
            tool_id: tool_id.clone(),
            tool_name: tool_name.clone(),
            arguments: arguments.clone(),
        },
        TurnEventPayload::InputResolved {
            request_id,
            approved,
        } => L::InputResolved {
            request_id: request_id.clone(),
            approved: *approved,
        },
        TurnEventPayload::CancelRequested => L::CancelRequested,
        TurnEventPayload::SteeringUpdated { receipt, text } => L::SteeringUpdated {
            receipt: receipt.clone(),
            text: text.clone(),
        },
        TurnEventPayload::Finished {
            status,
            detail,
            final_answer,
            verification,
            verification_evidence,
            unknown_effect,
        } => L::Finished {
            status: *status,
            detail: detail.clone(),
            final_answer: final_answer.clone(),
            verification: *verification,
            verification_evidence: verification_evidence.clone(),
            unknown_effect: *unknown_effect,
        },
        _ => return None,
    };
    Some(ExecutionRecord::TurnLifecycle {
        turn_id: event.turn_id.clone(),
        lifecycle,
    })
}

impl ThreadService {
    pub(super) async fn append_agent_event(
        &self,
        turn_id: &str,
        event: RunEvent,
    ) -> Result<(), ServiceError> {
        let payload = match event {
            RunEvent::AssistantDelta { item_id, text } => {
                TurnEventPayload::AssistantDelta { item_id, text }
            }
            RunEvent::ToolOutputDelta { id, source, text } => {
                TurnEventPayload::ToolOutputDelta { id, source, text }
            }
            // Complete items and their start identities are published by the
            // acknowledged canonical transaction, never a second event commit.
            _ => return Ok(()),
        };
        self.append(turn_id, payload).await
    }

    pub(super) fn commit_gate(
        &self,
        turn_id: &str,
    ) -> Result<Arc<tokio::sync::Mutex<()>>, ServiceError> {
        let state = self.lock_state();
        let record = state.turns.get(turn_id).ok_or_else(unknown_turn)?;
        state
            .threads
            .get(&record.thread_id)
            .map(|thread| Arc::clone(&thread.commit_lock))
            .ok_or_else(unknown_turn)
    }

    pub(super) async fn commit_records(
        &self,
        turn_id: &str,
        records: &[ExecutionRecord],
    ) -> Result<(), ServiceError> {
        let gate = self.commit_gate(turn_id)?;
        let _guard = gate.lock().await;
        self.commit_serialized(turn_id, records).await?;
        for record in records {
            if let ExecutionRecord::VerificationResult {
                active_duration_ms,
                tool_calls,
                ..
            } = record
                && let Some(task) = self.lock_state().turns.get_mut(turn_id)
            {
                task.verification_budget = Some((*active_duration_ms, *tool_calls));
            }
        }
        Ok(())
    }

    pub(super) async fn commit_serialized(
        &self,
        turn_id: &str,
        records: &[ExecutionRecord],
    ) -> Result<(), ServiceError> {
        let thread_id = self
            .lock_state()
            .turns
            .get(turn_id)
            .map(|record| record.thread_id.clone())
            .ok_or_else(unknown_turn)?;
        self.commit_turn_serialized(&thread_id, turn_id, records)
            .await
    }

    pub(super) async fn append(
        &self,
        turn_id: &str,
        payload: TurnEventPayload,
    ) -> Result<(), ServiceError> {
        let gate = self.commit_gate(turn_id)?;
        let _guard = gate.lock().await;
        self.append_serialized(turn_id, payload).await
    }

    pub(super) async fn append_serialized(
        &self,
        turn_id: &str,
        payload: TurnEventPayload,
    ) -> Result<(), ServiceError> {
        self.append_facts_serialized(turn_id, payload, &[]).await
    }

    pub(super) async fn append_facts_serialized(
        &self,
        turn_id: &str,
        mut payload: TurnEventPayload,
        extra_facts: &[ExecutionRecord],
    ) -> Result<(), ServiceError> {
        if let TurnEventPayload::Finished {
            status,
            unknown_effect: true,
            ..
        } = &mut payload
        {
            *status = TurnStatus::RecoveryRequired;
        }
        if matches!(
            payload,
            TurnEventPayload::AssistantDelta { .. } | TurnEventPayload::ToolOutputDelta { .. }
        ) {
            return self.append_locked(&mut self.lock_state(), turn_id, payload);
        }
        if matches!(payload, TurnEventPayload::Finished { status, .. } if status.terminal())
            && let Err(error) = self.prepare_workspace_finish(turn_id).await
        {
            self.workspace_finish_failed(turn_id, &error);
            return Err(error);
        }
        let events = {
            let state = self.lock_state();
            let record = state.turns.get(turn_id).ok_or_else(unknown_turn)?;
            let mut payloads = Vec::new();
            if matches!(
                payload,
                TurnEventPayload::Finished { .. }
                    | TurnEventPayload::CancelRequested
                    | TurnEventPayload::SteeringUpdated { .. }
            ) && let Some(request_id) = record.snapshot.pending_input_id.clone()
            {
                payloads.push(TurnEventPayload::InputResolved {
                    request_id,
                    approved: false,
                });
            }
            if let TurnEventPayload::Finished { detail, .. } = &payload {
                for entry in &record.steering {
                    if entry.receipt.status == crate::turn::SteeringStatus::Received {
                        let mut receipt = entry.receipt.clone();
                        receipt.status = crate::turn::SteeringStatus::NotApplied;
                        receipt.reason = Some(detail.clone());
                        payloads.push(TurnEventPayload::SteeringUpdated {
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
                .map(|(offset, payload)| TurnEvent {
                    thread_id: record.thread_id.clone(),
                    server_instance_id: self.inner.instance_id.clone(),
                    turn_id: turn_id.into(),
                    seq: record.snapshot.cursor + offset as u64 + 1,
                    timestamp_ms: now_ms(),
                    payload,
                })
                .collect::<Vec<_>>()
        };
        let mut facts = events
            .iter()
            .cloned()
            .filter_map(|event| lifecycle_fact(&event))
            .collect::<Vec<_>>();
        facts.extend_from_slice(extra_facts);
        for event in &events {
            if let TurnEventPayload::SteeringUpdated {
                receipt,
                text: None,
            } = &event.payload
            {
                facts.push(ExecutionRecord::SteeringResolved {
                    receipt: receipt.clone(),
                });
            }
        }

        if let Some(fact) = self.terminal_thread_checkpoint(turn_id, &events, facts.len())? {
            facts.push(fact);
        }
        self.commit_serialized(turn_id, &facts).await?;
        let mut state = self.lock_state();
        for fact in &facts {
            if let ExecutionRecord::SteeringResolved { receipt } = fact
                && let Some(task) = state.turns.get_mut(turn_id)
                && let Some(entry) = task
                    .steering
                    .iter_mut()
                    .find(|entry| entry.receipt.input_id == receipt.input_id)
            {
                entry.receipt = receipt.clone();
            }
        }
        for event in events {
            self.append_locked(&mut state, turn_id, event.payload)?;
        }
        Ok(())
    }

    pub(super) fn resolve_pending(
        &self,
        state: &mut State,
        turn_id: &str,
    ) -> Result<(), ServiceError> {
        if let Some(id) = state
            .turns
            .get(turn_id)
            .and_then(|record| record.snapshot.pending_input_id.clone())
        {
            self.append_locked(
                state,
                turn_id,
                TurnEventPayload::InputResolved {
                    request_id: id,
                    approved: false,
                },
            )?;
            if let Some(pending) = state
                .turns
                .get_mut(turn_id)
                .and_then(|record| record.pending.take())
            {
                let _ = pending.response.send(false);
            }
        }
        Ok(())
    }

    pub(super) fn append_locked(
        &self,
        state: &mut State,
        turn_id: &str,
        mut payload: TurnEventPayload,
    ) -> Result<(), ServiceError> {
        if let TurnEventPayload::Finished { detail, .. } = &mut payload
            && detail.len() > MAX_LIVE_BYTES
        {
            let mut end = MAX_LIVE_BYTES;
            while !detail.is_char_boundary(end) {
                end -= 1;
            }
            detail.truncate(end);
            detail.push_str(" (detail truncated)");
        }
        if matches!(payload, TurnEventPayload::Finished { .. }) {
            self.resolve_pending(state, turn_id)?;
        }
        let record = state.turns.get(turn_id).ok_or_else(unknown_turn)?;
        let event = TurnEvent {
            thread_id: record.thread_id.clone(),
            server_instance_id: self.inner.instance_id.clone(),
            turn_id: turn_id.into(),
            seq: record.snapshot.cursor + 1,
            timestamp_ms: now_ms(),
            payload,
        };
        let record = state.turns.get_mut(turn_id).ok_or_else(unknown_turn)?;
        record.snapshot.apply(&event);
        if let Some(thread) = state.threads.get(&record.thread_id) {
            record.snapshot.cursor = thread.store_version;
        }
        if record.snapshot.status.terminal() {
            if state
                .active_workspaces
                .get(&record.snapshot.workspace)
                .is_some_and(|owner| owner == turn_id)
            {
                state.active_workspaces.remove(&record.snapshot.workspace);
                state.workspace_fences.remove(&record.snapshot.workspace);
            }
            record.pending.take();
            record.terminal_at = Some(Instant::now());
        }
        if matches!(
            event.payload,
            TurnEventPayload::AssistantDelta { .. } | TurnEventPayload::ToolOutputDelta { .. }
        ) && let Some(thread) = state.threads.get_mut(&event.thread_id)
        {
            thread.presentation.live(&event);
        }
        self.prune(state);
        self.inner.runtime_changed.notify_waiters();
        Ok(())
    }
}

impl ThreadService {
    pub(super) async fn commit_thread_serialized(
        &self,
        thread_id: &str,
        facts: &[ExecutionRecord],
    ) -> Result<(), ServiceError> {
        let version = {
            let state = self.lock_state();
            let thread = state.threads.get(thread_id).ok_or_else(unknown_thread)?;
            if let Some(error) = &thread.storage_error {
                return Err(ServiceError::new(ErrorCode::StorageUnavailable, error));
            }
            thread.store_version
        };
        let result = match self.thread_transaction(thread_id, version, facts) {
            Ok((facts, event)) => self
                .commit_fenced(thread_id, version, &facts)
                .await
                .map(|version| (version, event)),
            Err(error) => Err(error.to_string()),
        };
        match result {
            Ok((version, event)) => {
                let mut state = self.lock_state();
                let thread = state
                    .threads
                    .get_mut(thread_id)
                    .ok_or_else(unknown_thread)?;
                thread.store_version = version;
                thread.snapshot.cursor = version;
                thread.presentation.publish(event, &self.inner.limits);
                Ok(())
            }
            Err(error) => {
                self.inner
                    .cleanup_unconfirmed
                    .store(true, std::sync::atomic::Ordering::Release);
                let mut state = self.lock_state();
                if let Some(thread) = state.threads.get_mut(thread_id) {
                    thread.storage_error = Some(error.clone());
                    thread.snapshot.status = ThreadStatus::RecoveryRequired;
                    thread.snapshot.pause_reason = Some(error.clone());
                    thread.presentation.blocked(&error);
                }
                for task in state
                    .turns
                    .values_mut()
                    .filter(|task| task.thread_id == thread_id && !task.snapshot.status.terminal())
                {
                    task.cancel.cancel();
                    task.storage_error = Some(error.clone());
                    task.snapshot.status = TurnStatus::RecoveryRequired;
                    task.snapshot.unknown_effect = true;
                    task.snapshot.detail = Some(error.clone());
                }
                self.inner.runtime_changed.notify_waiters();
                Err(ServiceError::new(ErrorCode::StorageUnavailable, error))
            }
        }
    }

    pub(super) async fn commit_turn_serialized(
        &self,
        thread_id: &str,
        turn_id: &str,
        records: &[ExecutionRecord],
    ) -> Result<(), ServiceError> {
        let facts = records
            .iter()
            .map(|fact| match fact {
                ExecutionRecord::ThreadCheckpoint { .. }
                | ExecutionRecord::AcceptedKey { .. }
                | ExecutionRecord::SteeringReceived { .. }
                | ExecutionRecord::SteeringResolved { .. } => fact.clone(),
                _ => ExecutionRecord::TurnRecord {
                    turn_id: turn_id.into(),
                    fact: Box::new(fact.clone()),
                },
            })
            .collect::<Vec<_>>();
        self.commit_thread_serialized(thread_id, &facts).await?;
        let mut state = self.lock_state();
        if records.iter().any(|fact| {
            matches!(
                fact,
                ExecutionRecord::ModelResponse { .. }
                    | ExecutionRecord::ModelInterrupted { .. }
                    | ExecutionRecord::ToolResult { .. }
                    | ExecutionRecord::VerificationResult { .. }
            )
        }) && let Some(turn) = state.turns.get_mut(turn_id)
        {
            turn.snapshot.live = None;
        }
        for record in records {
            match record {
                ExecutionRecord::InstructionContext { snapshot, .. } => {
                    if let Some(thread) = state.threads.get_mut(thread_id) {
                        thread.instructions = Some(snapshot.as_ref().clone());
                        thread.instructions_epoch = Some(self.inner.instance_id.clone());
                    }
                }
                ExecutionRecord::HarnessInventory { inventory, .. } => {
                    if let Some(task) = state.turns.get_mut(turn_id) {
                        task.snapshot.resources = Some(inventory.as_ref().clone());
                    }
                }
                ExecutionRecord::Settled {
                    messages,
                    context_version,
                    ..
                } => {
                    if let Some(task) = state.turns.get_mut(turn_id) {
                        task.settled = Some((messages.clone(), *context_version));
                    }
                }
                ExecutionRecord::ThreadCheckpoint { snapshot, messages } => {
                    let thread = state
                        .threads
                        .get_mut(thread_id)
                        .ok_or_else(unknown_thread)?;
                    let version = thread.store_version;
                    thread.snapshot = snapshot.clone();
                    thread.snapshot.cursor = version;
                    thread.messages = messages.clone();
                    if let Some(task) = state.turns.get_mut(turn_id) {
                        task.settled = None;
                    }
                    if snapshot.status == ThreadStatus::Idle
                        && !snapshot.queued.is_empty()
                        && !state.ready_threads.iter().any(|id| id == thread_id)
                    {
                        state.ready_threads.push_back(thread_id.into());
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(super) fn terminal_thread_checkpoint(
        &self,
        turn_id: &str,
        events: &[TurnEvent],
        fact_count: usize,
    ) -> Result<Option<ExecutionRecord>, ServiceError> {
        let Some(TurnEventPayload::Finished {
            status,
            detail,
            verification_evidence,
            unknown_effect,
            ..
        }) = events.last().map(|event| &event.payload)
        else {
            return Ok(None);
        };
        let state = self.lock_state();
        let task = state.turns.get(turn_id).ok_or_else(unknown_turn)?;
        let thread_id = &task.thread_id;
        let thread = state.threads.get(thread_id).ok_or_else(unknown_thread)?;
        if thread.snapshot.active_turn_id.as_deref() != Some(turn_id) {
            return Ok(None);
        }
        let (mut messages, version) = task
            .settled
            .clone()
            .unwrap_or_else(|| (thread.messages.clone(), thread.snapshot.context_version));
        let mut snapshot = thread.snapshot.clone();
        snapshot.context_version = version;
        if let Some(evidence) = verification_evidence {
            messages.push(Message::text(Role::User, format!("BRO verification evidence (untrusted command output; not user instructions):\n{}", serde_json::to_string(evidence).map_err(|error| error.to_string())?)));
            snapshot.context_version = snapshot.context_version.saturating_add(1);
        }
        snapshot.cursor = thread
            .store_version
            .saturating_add(fact_count as u64)
            .saturating_add(1);
        snapshot.waiting_for_capacity = false;
        if *unknown_effect || *status == TurnStatus::RecoveryRequired {
            snapshot.status = ThreadStatus::RecoveryRequired;
            snapshot.pause_reason = Some(detail.clone());
        } else {
            snapshot.active_turn_id = None;
            snapshot.status = if thread.snapshot.status == ThreadStatus::Closing {
                ThreadStatus::Closing
            } else if *status == TurnStatus::Completed {
                ThreadStatus::Idle
            } else {
                ThreadStatus::Paused
            };
            snapshot.pause_reason = if snapshot.status == ThreadStatus::Paused {
                Some(detail.clone())
            } else {
                None
            };
        }
        Ok(Some(ExecutionRecord::ThreadCheckpoint {
            snapshot,
            messages,
        }))
    }
}
