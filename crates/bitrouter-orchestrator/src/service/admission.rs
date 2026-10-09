//! Accept keyed inputs and commit identities before starting execution.

use std::path::Path;
use std::sync::Arc;

use bitrouter_sdk::caller::CallerContext;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use super::state::{QueuedTurn, State, ThreadRecord, TurnRecord};
use super::threads::unknown_thread;
use super::{ErrorCode, ServiceError, ThreadService};
use crate::agent::Agent;
use crate::store::{AcceptedKey, ExecutionRecord};
use crate::thread::{ThreadSnapshot, ThreadStatus, ThreadTarget};
use crate::turn::{
    TurnEventPayload, TurnReceipt, TurnRequest, TurnSnapshot, TurnStatus, VerificationStatus,
};

pub(super) fn fingerprint<T: Serialize>(value: &T) -> Result<String, ServiceError> {
    let encoded = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    Ok(Sha256::digest(encoded)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub(super) fn key_scope(
    caller: &CallerContext,
    thread_id: Option<&str>,
    operation: &str,
    key: &str,
) -> Result<String, ServiceError> {
    if key.is_empty() || key.len() > 128 || caller.is_anonymous() {
        return Err("authenticated caller and a 1-128 byte acceptance key are required".into());
    }
    let scope =
        serde_json::to_string(&(caller.api_key_id(), caller.user_id(), operation, thread_id))
            .map_err(|error| error.to_string())?;
    if scope.len() > 512 {
        return Err("caller scope is too large".into());
    }
    Ok(scope)
}

impl ThreadService {
    pub(super) async fn accepted_key(
        &self,
        scope: &str,
        key: &str,
        fingerprint: &str,
    ) -> Result<Option<AcceptedKey>, ServiceError> {
        let entry = self
            .inner
            .store
            .find_key(scope, key)
            .await
            .map_err(ServiceError::storage)?;
        if entry
            .as_ref()
            .is_some_and(|entry| entry.fingerprint != fingerprint)
        {
            return Err(ServiceError::new(
                ErrorCode::Conflict,
                "acceptance key belongs to a different request",
            ));
        }
        Ok(entry)
    }

    /// Resolve only receipt metadata; never materialize the full journal or
    /// hold the admission mutex while paging historical execution facts.
    pub(super) async fn scan_receipt(
        &self,
        thread_id: &str,
        mut consume: impl FnMut(ExecutionRecord),
    ) -> Result<(ThreadSnapshot, u64), ServiceError> {
        let _reader = self.inner.recovery_readers.try_acquire().map_err(|_| {
            ServiceError::new(ErrorCode::Overloaded, "recovery reader capacity is full")
        })?;
        let mut after = 0_u64;
        let mut cutoff = None;
        let mut snapshot = None;
        loop {
            let limit = if after == 0 {
                1
            } else {
                self.inner.limits.recovery_page_records
            };
            let page = self
                .inner
                .store
                .read_records(
                    thread_id,
                    after,
                    cutoff,
                    limit,
                    self.inner.limits.recovery_page_bytes,
                )
                .await
                .map_err(ServiceError::storage)?
                .ok_or_else(unknown_thread)?;
            if page.cutoff == 0 || page.cutoff > self.inner.limits.recovery_records_per_thread {
                return Err(ServiceError::new(
                    ErrorCode::Overloaded,
                    "receipt record scan bound exceeded",
                ));
            }
            if cutoff.is_some_and(|cutoff| cutoff != page.cutoff)
                || page.records.is_empty()
                || page.records.len() > limit
            {
                return Err(ServiceError::new(
                    ErrorCode::StorageUnavailable,
                    "invalid receipt record page",
                ));
            }
            cutoff = Some(page.cutoff);
            after = after
                .checked_add(page.records.len() as u64)
                .ok_or("receipt cursor exhausted")?;
            if after > page.cutoff || page.next_after != (after < page.cutoff).then_some(after) {
                return Err(ServiceError::new(
                    ErrorCode::StorageUnavailable,
                    "invalid receipt page cursor",
                ));
            }
            for record in page.records {
                if let ExecutionRecord::ThreadCreated {
                    snapshot: current, ..
                }
                | ExecutionRecord::ThreadCheckpoint {
                    snapshot: current, ..
                } = &record
                {
                    if current.thread_id != thread_id {
                        return Err("receipt Thread identity mismatch".into());
                    }
                    self.check_snapshot_grant(current)?;
                    snapshot = Some(current.clone());
                }
                consume(record);
            }
            if after == page.cutoff {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok((snapshot.ok_or_else(unknown_thread)?, after))
    }

    pub(super) async fn existing_thread_receipt(
        &self,
        entry: &AcceptedKey,
    ) -> Result<ThreadSnapshot, ServiceError> {
        {
            let state = self.lock_state();
            if let Some(thread) = state.threads.get(&entry.thread_id) {
                self.check_thread_grant(&state, thread)?;
                return Ok(thread.snapshot.clone());
            }
        }
        let (mut snapshot, version) = self.scan_receipt(&entry.thread_id, |_| {}).await?;
        snapshot.server_instance_id = self.inner.instance_id.clone();
        snapshot.cursor = version;
        snapshot.status = ThreadStatus::RecoveryRequired;
        snapshot.pause_reason =
            Some("stored Thread awaits execution ownership and effect recovery".into());
        Ok(snapshot)
    }

    pub(super) async fn existing_turn_receipt(
        &self,
        entry: &AcceptedKey,
    ) -> Result<TurnReceipt, ServiceError> {
        let turn_id = entry
            .turn_id
            .as_ref()
            .ok_or("accepted turn identity missing")?;
        let mut receipt = None;
        self.scan_receipt(&entry.thread_id, |record| match record {
            ExecutionRecord::TurnQueued {
                turn_id: id,
                queue_order,
                ..
            } if &id == turn_id => {
                receipt = Some(TurnReceipt {
                    thread_id: entry.thread_id.clone(),
                    turn_id: id,
                    queue_order,
                    status: TurnStatus::Queued,
                })
            }
            ExecutionRecord::TurnActivated { turn_id: id, .. } if &id == turn_id => {
                if let Some(receipt) = &mut receipt {
                    receipt.status = TurnStatus::RecoveryRequired;
                }
            }
            ExecutionRecord::TurnRecord { turn_id: id, fact } if &id == turn_id => {
                if let ExecutionRecord::TurnLifecycle { lifecycle, .. } = *fact
                    && let crate::turn::TurnLifecycle::Finished { status, .. } = lifecycle
                    && let Some(receipt) = &mut receipt
                {
                    receipt.status = status;
                }
            }
            _ => {}
        })
        .await?;
        let mut receipt = receipt.ok_or("accepted Turn admission missing")?;
        if let Some(task) = self.lock_state().turns.get(turn_id) {
            receipt.status = task.snapshot.status;
        }
        Ok(receipt)
    }

    pub async fn start_turn(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        request: TurnRequest,
    ) -> Result<TurnReceipt, ServiceError> {
        self.load_thread(target, caller).await?;
        self.admit_turn(target, caller, request, true).await
    }

    pub async fn enqueue_turn(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        request: TurnRequest,
    ) -> Result<TurnReceipt, ServiceError> {
        self.load_thread(target, caller).await?;
        self.admit_turn(target, caller, request, false).await
    }

    pub(super) async fn admit_turn(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        request: TurnRequest,
        start: bool,
    ) -> Result<TurnReceipt, ServiceError> {
        self.ensure_instance(Some(&target.server_instance_id))?;
        let admission = self.inner.admission.lock().await;
        let scope = key_scope(
            caller,
            Some(&target.thread_id),
            if start { "start" } else { "enqueue" },
            &request.idempotency_key,
        )?;
        let hash = fingerprint(&request.prompt)?;
        if let Some(entry) = self
            .accepted_key(&scope, &request.idempotency_key, &hash)
            .await?
        {
            drop(admission);
            return self.existing_turn_receipt(&entry).await;
        }
        if request.prompt.trim().is_empty()
            || request.prompt.len() > self.inner.limits.request_bytes
        {
            return Err("nonempty input within the request bound is required".into());
        }
        let gate = self.thread_gate(&target.thread_id)?;
        let guard = gate.lock().await;
        let (queued, config, workspace, verification, context_version, previous_messages, profile) = {
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
                ThreadStatus::RecoveryRequired | ThreadStatus::Closing
            ) {
                return Err(ServiceError::new(
                    ErrorCode::RecoveryRequired,
                    "Thread cannot admit input until recovery or shutdown completes",
                ));
            }
            if start
                && (thread.snapshot.status != ThreadStatus::Idle
                    || !thread.queued.is_empty()
                    || thread.snapshot.active_turn_id.is_some())
            {
                return Err(ServiceError::new(
                    ErrorCode::Conflict,
                    "start requires an idle Thread without accepted queued work",
                ));
            }
            if thread.queued.len() >= self.inner.limits.queued_turns_per_thread {
                return Err(ServiceError::new(
                    ErrorCode::Overloaded,
                    "Thread queue is full",
                ));
            }
            let extra = request.prompt.len().saturating_mul(2);
            if thread.bytes().saturating_add(extra) > self.inner.limits.context_bytes_per_thread
                || state
                    .threads
                    .values()
                    .map(ThreadRecord::bytes)
                    .sum::<usize>()
                    .saturating_add(extra)
                    > self.inner.limits.hot_context_bytes
            {
                return Err(ServiceError::new(
                    ErrorCode::Overloaded,
                    "Thread context/input capacity is full",
                ));
            }
            if start {
                self.check_turn_capacity(&state, &thread.snapshot.workspace)?;
            }
            let queued = QueuedTurn {
                turn_id: uuid::Uuid::new_v4().to_string(),
                user_item_id: uuid::Uuid::new_v4().to_string(),
                prompt: request.prompt,
                order: thread
                    .next_order
                    .checked_add(1)
                    .ok_or("queue order exhausted")?,
            };
            (
                queued,
                thread.config.clone(),
                thread.snapshot.workspace.clone(),
                thread.verification_command.clone(),
                thread.snapshot.context_version,
                thread.messages.clone(),
                thread.snapshot.permission_profile,
            )
        };
        let agent = if start {
            Some(
                Agent::new(
                    Arc::clone(&self.inner.app),
                    caller.clone(),
                    &workspace,
                    config.clone(),
                )?
                .with_tool_workers(
                    Arc::clone(&self.inner.tool_workers),
                    self.inner.limits.tools_per_turn,
                ),
            )
        } else {
            None
        };
        if start {
            self.reserve_workspace(&workspace, &queued.turn_id).await?;
        }
        let payload = if start {
            TurnEventPayload::Accepted {
                user_item_id: queued.user_item_id.clone(),
                prompt: queued.prompt.clone(),
                workspace: workspace.clone(),
                model: config.model.clone(),
                tool_mode: config.tool_mode(),
                idempotency_key: Some(request.idempotency_key.clone()),
                request_fingerprint: Some(hash.clone()),
            }
        } else {
            TurnEventPayload::TurnQueued {
                user_item_id: queued.user_item_id.clone(),
                prompt: queued.prompt.clone(),
                queue_order: queued.order,
            }
        };
        let key = AcceptedKey {
            scope,
            key: request.idempotency_key,
            fingerprint: hash,
            thread_id: target.thread_id.clone(),
            turn_id: Some(queued.turn_id.clone()),
        };
        let mut facts = vec![
            ExecutionRecord::TurnQueued {
                turn_id: queued.turn_id.clone(),
                user_item_id: queued.user_item_id.clone(),
                prompt: queued.prompt.clone(),
                queue_order: queued.order,
            },
            ExecutionRecord::AcceptedKey { entry: key.clone() },
        ];
        if start {
            facts.push(ExecutionRecord::TurnActivated {
                turn_id: queued.turn_id.clone(),
                context_version: context_version.saturating_add(1),
            });
        }
        if let Err(error) = self
            .commit_thread_serialized(&target.thread_id, &facts)
            .await
        {
            if let Some(entry) = self
                .accepted_key(&key.scope, &key.key, &key.fingerprint)
                .await?
            {
                drop(guard);
                drop(admission);
                return self.existing_turn_receipt(&entry).await;
            }
            return Err(error);
        }
        let cancel = CancellationToken::new();
        let receipt = TurnReceipt {
            thread_id: target.thread_id.clone(),
            turn_id: queued.turn_id.clone(),
            queue_order: queued.order,
            status: if start {
                TurnStatus::Accepted
            } else {
                TurnStatus::Queued
            },
        };
        {
            let mut state = self.lock_state();
            let thread = state
                .threads
                .get_mut(&target.thread_id)
                .ok_or_else(unknown_thread)?;
            thread.next_order = queued.order;
            if start {
                thread.snapshot.status = ThreadStatus::Busy;
                thread.snapshot.active_turn_id = Some(queued.turn_id.clone());
                thread.snapshot.context_version = context_version.saturating_add(1);
            } else {
                thread.queued.push_back(queued.clone());
                thread.snapshot.queued.push(receipt.clone());
            }
            let snapshot = TurnSnapshot {
                resources: None,
                steering: Vec::new(),
                thread_id: target.thread_id.clone(),
                server_instance_id: self.inner.instance_id.clone(),
                model: config.model.clone(),
                turn_id: queued.turn_id.clone(),
                status: receipt.status,
                cursor: 0,
                workspace: workspace.clone(),
                tool_mode: config.tool_mode(),
                final_answer: None,
                detail: None,
                unknown_effect: false,
                verification: VerificationStatus::Unavailable,
                verification_evidence: None,
                pending_input_id: None,
                pending_input: None,
                live: None,
            };
            state.turns.insert(
                queued.turn_id.clone(),
                TurnRecord {
                    fence: Arc::new(crate::control::LaunchFence::default()),
                    steering: Vec::new(),
                    verification_budget: None,
                    thread_id: target.thread_id.clone(),
                    permission_profile: profile,
                    settled: None,
                    snapshot,
                    terminal_at: None,
                    cancel: cancel.clone(),
                    pending: None,
                    storage_error: None,
                },
            );
            self.append_locked(&mut state, &queued.turn_id, payload)?;
            if start {
                state
                    .active_workspaces
                    .insert(workspace.clone(), queued.turn_id.clone());
            } else if !state.ready_threads.contains(&target.thread_id) {
                state.ready_threads.push_back(target.thread_id.clone());
            }
        }
        if let Some(agent) = agent {
            self.spawn_thread_turn(
                queued,
                agent,
                (previous_messages, context_version),
                verification,
                cancel,
            );
        }
        drop(guard);
        drop(admission);
        if !start {
            self.drive_queues().await;
        }
        Ok(receipt)
    }

    pub(super) fn check_turn_capacity(
        &self,
        state: &State,
        workspace: &Path,
    ) -> Result<(), ServiceError> {
        if let Some(error) = self.workspace_owner_error(state, workspace) {
            return Err(error);
        }
        if state.active_workspaces.len() >= self.inner.limits.active_turns {
            return Err(ServiceError::new(
                ErrorCode::Overloaded,
                "active Turn limit reached",
            ));
        }
        Ok(())
    }
}
