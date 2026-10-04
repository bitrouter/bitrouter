//! Thread state is owned by ThreadService, alongside its Turn projections. This
//! module does not introduce a second runner or a second commit authority.

use super::*;
use crate::store::AcceptedKey;
use crate::thread::{
    ApprovalAnswer, CancelTurnRequest, ThreadRequest, ThreadSnapshot, ThreadTarget, TurnReceipt,
    TurnRequest, WorkspaceGrant,
};
use bitrouter_sdk::language_model::Role;
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub(super) struct QueuedTurn {
    pub(super) turn_id: String,
    pub(super) user_item_id: String,
    pub(super) prompt: String,
    pub(super) order: u64,
}

pub(super) struct ThreadRecord {
    pub(super) presentation: super::observation::Presentation,
    pub(super) snapshot: ThreadSnapshot,
    pub(super) caller: CallerContext,
    pub(super) config: AgentConfig,
    pub(super) verification_command: Option<String>,
    pub(super) messages: Vec<Message>,
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
                self.queued
                    .iter()
                    .map(|entry| entry.prompt.len().saturating_mul(2))
                    .sum::<usize>(),
            )
    }
}

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

pub(super) fn unknown_thread() -> ServiceError {
    ServiceError::new(
        ErrorCode::UnknownThread,
        "Thread is not loaded; safe recovery is required before continuation",
    )
}

impl ThreadService {
    /// This constructor accepts trusted host grants, never client-supplied grants.
    pub fn with_workspace_grants(
        app: Arc<App>,
        grants: &[WorkspaceGrant],
        store: Arc<dyn ExecutionStore>,
    ) -> Result<Self, ServiceError> {
        let paths = grants
            .iter()
            .map(|grant| grant.workspace.clone())
            .collect::<Vec<_>>();
        let service = Self::with_store(app, &paths, store)?;
        {
            let mut state = service.lock_state();
            for grant in grants {
                if grant.permission_profiles.is_empty() {
                    return Err("workspace grant needs a permission profile".into());
                }
                let workspace = grant
                    .workspace
                    .canonicalize()
                    .map_err(|error| error.to_string())?;
                state
                    .workspace_profiles
                    .insert(workspace, grant.permission_profiles.clone());
            }
        }
        Ok(service)
    }

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

    async fn existing_thread_receipt(
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

    async fn existing_turn_receipt(
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
                    && let crate::thread::TurnLifecycle::Finished { status, .. } = lifecycle
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

    pub async fn create_thread(
        &self,
        server_instance_id: &str,
        request: ThreadRequest,
    ) -> Result<ThreadSnapshot, ServiceError> {
        self.ensure_instance(Some(server_instance_id))?;
        let admission = self.inner.admission.lock().await;
        let scope = key_scope(
            &request.caller,
            None,
            "create_thread",
            &request.idempotency_key,
        )?;
        let hash = fingerprint(&(
            &request.workspace,
            &request.config,
            request.permission_profile,
            &request.verification_command,
        ))?;
        if let Some(entry) = self
            .accepted_key(&scope, &request.idempotency_key, &hash)
            .await?
        {
            drop(admission);
            return self.existing_thread_receipt(&entry).await;
        }
        let workspace = request
            .workspace
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let profile = if request.config.tool_mode() == ToolMode::ReadOnly {
            PermissionProfile::ReadOnly
        } else {
            request.permission_profile
        };
        let config = if profile == PermissionProfile::ReadOnly {
            request.config.read_only()
        } else {
            request.config
        };
        if config.model.len()
            + config.instructions.len()
            + request.verification_command.as_ref().map_or(0, String::len)
            > self.inner.limits.request_bytes
            || config.max_steps > 256
            || config.max_tool_calls > 1024
            || config.max_context_bytes > self.inner.limits.context_bytes_per_thread
            || config.max_duration > Duration::from_secs(86400)
        {
            return Err("Thread settings exceed runtime bounds".into());
        }
        if profile == PermissionProfile::ReadOnly && request.verification_command.is_some() {
            return Err("read-only Threads cannot run verification".into());
        }
        Agent::new(
            Arc::clone(&self.inner.app),
            request.caller.clone(),
            &workspace,
            config.clone(),
        )?;
        self.reclaim_hot_capacity(0)?;
        {
            let state = self.lock_state();
            if state.closing {
                return Err(ServiceError::new(
                    ErrorCode::ShuttingDown,
                    "runtime is shutting down",
                ));
            }
            if state.threads.len() >= self.inner.limits.hot_threads {
                return Err(ServiceError::new(
                    ErrorCode::Overloaded,
                    "hot Thread limit reached",
                ));
            }
            if !state
                .workspace_profiles
                .get(&workspace)
                .is_some_and(|profiles| profiles.contains(&profile))
            {
                return Err(ServiceError::new(
                    ErrorCode::Unauthorized,
                    "workspace permission profile is not granted by the server",
                ));
            }
        }
        self.initialize_execution().await?;
        let thread_id = uuid::Uuid::new_v4().to_string();
        let mut snapshot = ThreadSnapshot {
            server_instance_id: self.inner.instance_id.clone(),
            thread_id: thread_id.clone(),
            status: ThreadStatus::Idle,
            workspace,
            model: config.model.clone(),
            permission_profile: profile,
            context_version: 0,
            cursor: 2,
            active_turn_id: None,
            queued: Vec::new(),
            pause_reason: None,
            waiting_for_capacity: false,
        };
        let key = AcceptedKey {
            scope,
            key: request.idempotency_key,
            fingerprint: hash,
            thread_id: thread_id.clone(),
            turn_id: None,
        };
        let facts = [
            ExecutionRecord::ThreadCreated {
                caller: request.caller.clone(),
                snapshot: snapshot.clone(),
                config: Box::new(config.clone()),
                verification_command: request.verification_command.clone(),
            },
            ExecutionRecord::AcceptedKey { entry: key.clone() },
        ];
        let (facts, event) = self.thread_transaction(&thread_id, 0, &facts)?;
        let version = match self.commit_fenced(&thread_id, 0, &facts).await {
            Ok(version) => version,
            Err(error) => {
                if let Some(entry) = self
                    .accepted_key(&key.scope, &key.key, &key.fingerprint)
                    .await?
                {
                    drop(admission);
                    return self.existing_thread_receipt(&entry).await;
                }
                return Err(ServiceError::new(ErrorCode::StorageUnavailable, error));
            }
        };
        snapshot.cursor = version;
        let mut presentation = super::observation::Presentation::new(
            crate::thread::ThreadView {
                recovery: None,
                thread: snapshot.clone(),
                config: config.clone(),
                verification_command: request.verification_command.clone(),
                latest_turn: None,
            },
            &self.inner.limits,
        );
        presentation.publish(event, &self.inner.limits);
        self.lock_state().threads.insert(
            thread_id,
            ThreadRecord {
                presentation,
                snapshot: snapshot.clone(),
                caller: request.caller,
                config,
                verification_command: request.verification_command,
                messages: Vec::new(),
                queued: VecDeque::new(),
                next_order: 0,
                commit_lock: Arc::new(tokio::sync::Mutex::new(())),
                store_version: version,
                storage_error: None,
                last_used: Instant::now(),
            },
        );
        Ok(snapshot)
    }

    pub fn read_thread(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
    ) -> Result<ThreadSnapshot, ServiceError> {
        self.ensure_instance(Some(&target.server_instance_id))?;
        let state = self.lock_state();
        let thread = state
            .threads
            .get(&target.thread_id)
            .ok_or_else(unknown_thread)?;
        thread.authorize(caller)?;
        self.check_thread_grant(&state, thread)?;
        Ok(thread.snapshot.clone())
    }

    pub fn read_turn(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        turn_id: &str,
    ) -> Result<TurnSnapshot, ServiceError> {
        self.read_thread_view(target, caller)?;
        let state = self.lock_state();
        let turn = state.turns.get(turn_id).ok_or_else(unknown_turn)?;
        if turn.thread_id != target.thread_id {
            return Err(unknown_turn());
        }
        Ok(turn.snapshot.clone())
    }

    pub async fn read_stored_turn(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        turn_id: &str,
    ) -> Result<TurnSnapshot, ServiceError> {
        if turn_id.is_empty() || turn_id.len() > 128 {
            return Err("invalid Turn identity".into());
        }
        let view = self.read_stored_thread_view(target, caller).await?;
        if let Some(turn) = self.lock_state().turns.get(turn_id)
            && turn.thread_id == target.thread_id
        {
            let mut snapshot = turn.snapshot.clone();
            snapshot.cursor = view.thread.cursor;
            return Ok(snapshot);
        }
        if let Some(turn) = &view.latest_turn
            && turn.turn_id == turn_id
        {
            return Ok(turn.clone());
        }
        let _reader = self.inner.recovery_readers.try_acquire().map_err(|_| {
            ServiceError::new(
                ErrorCode::Overloaded,
                "Turn history reader capacity is full",
            )
        })?;
        let cutoff = view.thread.cursor;
        let mut after = 0;
        let mut found: Option<TurnSnapshot> = None;
        loop {
            let page = self
                .inner
                .store
                .thread_history(
                    &target.thread_id,
                    after,
                    cutoff,
                    128,
                    self.inner.limits.history_page_bytes,
                )
                .await
                .map_err(ServiceError::storage)?;
            for event in &page.events {
                if event.thread_id != target.thread_id {
                    return Err("history Thread identity mismatch".into());
                }
                for change in &event.changes {
                    match change {
                        crate::thread::ThreadChange::TurnQueued { receipt, .. }
                            if receipt.turn_id == turn_id =>
                        {
                            let mut snapshot =
                                super::observation::empty_turn(&view.thread, &view.config, turn_id);
                            snapshot.status = TurnStatus::Queued;
                            found = Some(snapshot);
                        }
                        crate::thread::ThreadChange::TurnActivated { turn_id: id, .. }
                            if id == turn_id =>
                        {
                            if let Some(turn) = &mut found {
                                turn.status = TurnStatus::Accepted;
                            }
                        }
                        crate::thread::ThreadChange::QueuedTurnCancelled { turn_id: id }
                            if id == turn_id =>
                        {
                            if let Some(turn) = &mut found {
                                turn.status = TurnStatus::Cancelled;
                            }
                        }
                        crate::thread::ThreadChange::TurnLifecycle {
                            turn_id: id,
                            lifecycle,
                        } if id == turn_id => {
                            if let Some(turn) = &mut found {
                                turn.apply_payload(&lifecycle.payload());
                            }
                        }
                        _ => {}
                    }
                }
                after = event.seq;
                if let Some(turn) = &mut found {
                    turn.cursor = after;
                }
            }
            if !page.more {
                break;
            }
            if page.events.is_empty() {
                return Err("history made no progress".into());
            }
        }
        self.check_snapshot_grant(&view.thread)?;
        let mut turn = found.ok_or_else(unknown_turn)?;
        turn.cursor = cutoff;
        if view.recovery.is_some() && !turn.status.terminal() && turn.status != TurnStatus::Queued {
            turn.status = TurnStatus::RecoveryRequired;
            turn.pending_input_id = None;
            turn.pending_input = None;
            turn.detail = Some("stored Turn requires explicit recovery".into());
        }
        Ok(turn)
    }

    /// Called with admission held. State and gate ownership make removal atomic
    /// with observation registration and any operation retaining the unique gate.
    fn unload_hot(&self, thread_id: &str) -> Result<(), ServiceError> {
        let mut state = self.lock_state();
        let thread = state.threads.get(thread_id).ok_or_else(unknown_thread)?;
        let gate = Arc::clone(&thread.commit_lock);
        // A different Thread's workspace lease must not pin this idle cache.
        // Unattributed fences remain conservative recovery blockers.
        let workspace_owned = state
            .active_workspaces
            .get(&thread.snapshot.workspace)
            .map_or_else(
                || {
                    state
                        .workspace_fences
                        .contains_key(&thread.snapshot.workspace)
                },
                |id| {
                    id == thread_id
                        || state.turns.get(id).map_or_else(
                            || !state.threads.contains_key(id),
                            |turn| turn.thread_id == thread_id,
                        )
                },
            );
        let _guard = gate
            .try_lock()
            .map_err(|_| ServiceError::new(ErrorCode::Conflict, "Thread commit is in flight"))?;
        if Arc::strong_count(&gate) != 2
            || !matches!(
                thread.snapshot.status,
                ThreadStatus::Idle | ThreadStatus::Paused
            )
            || thread.snapshot.active_turn_id.is_some()
            || !thread.queued.is_empty()
            || !thread.snapshot.queued.is_empty()
            || thread.storage_error.is_some()
            || thread.presentation.view.recovery.is_some()
            || thread.presentation.publisher.receiver_count() != 0
            || state.running_turns.values().any(|id| id == thread_id)
            || workspace_owned
            || state
                .turns
                .values()
                .filter(|turn| turn.thread_id == thread_id)
                .any(|turn| {
                    !turn.snapshot.status.terminal()
                        || turn.snapshot.unknown_effect
                        || turn.pending.is_some()
                        || turn.storage_error.is_some()
                })
        {
            return Err(ServiceError::new(
                ErrorCode::Conflict,
                "Thread still owns execution, observation or recovery resources",
            ));
        }
        state.turns.retain(|_, turn| turn.thread_id != thread_id);
        state.ready_threads.retain(|id| id != thread_id);
        state.threads.remove(thread_id);
        Ok(())
    }

    pub async fn unload_thread(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
    ) -> Result<(), ServiceError> {
        let _admission = self.inner.admission.lock().await;
        self.read_thread_view(target, caller)?;
        self.unload_hot(&target.thread_id)
    }

    pub(super) fn reclaim_hot_capacity(&self, additional_bytes: usize) -> Result<(), ServiceError> {
        let mut candidates = {
            let state = self.lock_state();
            state
                .threads
                .iter()
                .map(|(id, thread)| (thread.last_used, id.clone()))
                .collect::<Vec<_>>()
        };
        candidates.sort_by_key(|(used, _)| *used);
        for (_, id) in candidates {
            let full = {
                let state = self.lock_state();
                state.threads.len() >= self.inner.limits.hot_threads
                    || state
                        .threads
                        .values()
                        .map(ThreadRecord::bytes)
                        .sum::<usize>()
                        .saturating_add(additional_bytes)
                        > self.inner.limits.hot_context_bytes
            };
            if !full {
                return Ok(());
            }
            let _ = self.unload_hot(&id);
        }
        let state = self.lock_state();
        if state.threads.len() >= self.inner.limits.hot_threads
            || state
                .threads
                .values()
                .map(ThreadRecord::bytes)
                .sum::<usize>()
                .saturating_add(additional_bytes)
                > self.inner.limits.hot_context_bytes
        {
            return Err(ServiceError::new(
                ErrorCode::Overloaded,
                "hot Thread capacity has no safely unloadable candidate",
            ));
        }
        Ok(())
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

    pub async fn cancel_turn(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        request: CancelTurnRequest,
    ) -> Result<TurnReceipt, ServiceError> {
        self.ensure_instance(Some(&target.server_instance_id))?;
        let admission = self.inner.admission.lock().await;
        let scope = key_scope(
            caller,
            Some(&target.thread_id),
            "cancel_turn",
            &request.idempotency_key,
        )?;
        let hash = fingerprint(&request.turn_id)?;
        if let Some(key) = self
            .accepted_key(&scope, &request.idempotency_key, &hash)
            .await?
        {
            drop(admission);
            return self.existing_turn_receipt(&key).await;
        }
        let gate = self.thread_gate(&target.thread_id)?;
        let _guard = gate.lock().await;
        self.authorize_active_turn(target, caller, &request.turn_id)?;
        let key = AcceptedKey {
            scope,
            key: request.idempotency_key,
            fingerprint: hash,
            thread_id: target.thread_id.clone(),
            turn_id: Some(request.turn_id.clone()),
        };
        self.append_facts_serialized(
            &request.turn_id,
            TurnEventPayload::CancelRequested,
            &[ExecutionRecord::AcceptedKey { entry: key.clone() }],
        )
        .await?;
        if let Some(task) = self.lock_state().turns.get(&request.turn_id) {
            task.cancel.cancel();
        }
        drop(_guard);
        drop(admission);
        self.existing_turn_receipt(&key).await
    }

    pub async fn answer_thread_input(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        answer: ApprovalAnswer,
    ) -> Result<TurnReceipt, ServiceError> {
        self.ensure_instance(Some(&target.server_instance_id))?;
        let admission = self.inner.admission.lock().await;
        let scope = key_scope(
            caller,
            Some(&target.thread_id),
            "answer_approval",
            &answer.idempotency_key,
        )?;
        let hash = fingerprint(&(&answer.turn_id, &answer.request_id, answer.approved))?;
        if let Some(key) = self
            .accepted_key(&scope, &answer.idempotency_key, &hash)
            .await?
        {
            drop(admission);
            return self.existing_turn_receipt(&key).await;
        }
        let gate = self.thread_gate(&target.thread_id)?;
        let _guard = gate.lock().await;
        self.authorize_active_turn(target, caller, &answer.turn_id)?;
        if answer.approved {
            let state = self.lock_state();
            let thread = state
                .threads
                .get(&target.thread_id)
                .ok_or_else(unknown_thread)?;
            self.check_thread_grant(&state, thread)?;
        }
        let key = AcceptedKey {
            scope,
            key: answer.idempotency_key,
            fingerprint: hash,
            thread_id: target.thread_id.clone(),
            turn_id: Some(answer.turn_id.clone()),
        };
        self.answer_input_serialized(
            &answer.turn_id,
            &answer.request_id,
            answer.approved,
            &[ExecutionRecord::AcceptedKey { entry: key.clone() }],
        )
        .await?;
        drop(_guard);
        drop(admission);
        self.existing_turn_receipt(&key).await
    }

    pub(super) fn authorize_active_turn(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        turn_id: &str,
    ) -> Result<(), ServiceError> {
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
                "uncertain Turn requires recovery",
            ));
        }
        if thread.snapshot.active_turn_id.as_deref() != Some(turn_id) {
            return Err(ServiceError::new(
                ErrorCode::Conflict,
                "expected active Turn has changed",
            ));
        }
        Ok(())
    }

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

    async fn admit_turn(
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

    fn spawn_thread_turn(
        &self,
        entry: QueuedTurn,
        agent: Agent,
        context: (Vec<Message>, u64),
        verification: Option<String>,
        cancel: CancellationToken,
    ) {
        self.inner.workers.spawn(self.run_turn(
            entry.turn_id,
            agent,
            RunInput {
                prompt: entry.prompt,
                messages: context.0,
                user_item_id: entry.user_item_id,
                context_version: context.1,
                checkpoint: None,
                complete_checkpoint: false,
                restored_verification: None,
            },
            verification,
            cancel,
        ));
    }

    fn check_turn_capacity(&self, state: &State, workspace: &Path) -> Result<(), ServiceError> {
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

    pub(super) fn check_thread_grant(
        &self,
        state: &State,
        thread: &ThreadRecord,
    ) -> Result<(), ServiceError> {
        if state
            .workspace_profiles
            .get(&thread.snapshot.workspace)
            .is_some_and(|profiles| profiles.contains(&thread.snapshot.permission_profile))
        {
            Ok(())
        } else {
            Err(ServiceError::new(
                ErrorCode::Unauthorized,
                "workspace permission profile is no longer granted",
            ))
        }
    }

    pub(super) fn check_snapshot_grant(
        &self,
        snapshot: &ThreadSnapshot,
    ) -> Result<(), ServiceError> {
        if self
            .lock_state()
            .workspace_profiles
            .get(&snapshot.workspace)
            .is_some_and(|profiles| profiles.contains(&snapshot.permission_profile))
        {
            Ok(())
        } else {
            Err(ServiceError::new(
                ErrorCode::Unauthorized,
                "workspace permission profile is no longer granted",
            ))
        }
    }

    pub(super) fn thread_gate(
        &self,
        thread_id: &str,
    ) -> Result<Arc<tokio::sync::Mutex<()>>, ServiceError> {
        self.lock_state()
            .threads
            .get(thread_id)
            .map(|thread| Arc::clone(&thread.commit_lock))
            .ok_or_else(unknown_thread)
    }

    async fn commit_thread_serialized(
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
            snapshot.status = if *status == TurnStatus::Completed {
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

    fn schedule_queue_retry(&self) {
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

    fn queue_retry(
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

    async fn pause_thread_serialized(
        &self,
        thread_id: &str,
        reason: String,
    ) -> Result<(), ServiceError> {
        self.checkpoint_queue_serialized(thread_id, reason, ThreadStatus::Paused)
            .await
    }

    async fn checkpoint_queue_serialized(
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
