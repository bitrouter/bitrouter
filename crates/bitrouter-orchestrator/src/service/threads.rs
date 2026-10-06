//! Thread state is owned by ThreadService, alongside its Turn projections. This
//! module does not introduce a second runner or a second commit authority.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bitrouter_sdk::App;
use bitrouter_sdk::caller::CallerContext;

use super::admission::{fingerprint, key_scope};
use super::state::{State, ThreadRecord};
use super::{ErrorCode, ServiceError, ThreadService, unknown_turn};
use crate::agent::{Agent, ToolMode};
use crate::store::{AcceptedKey, ExecutionRecord, ExecutionStore};
use crate::thread::{
    PermissionProfile, ThreadRequest, ThreadSnapshot, ThreadStatus, ThreadTarget, WorkspaceGrant,
};
use crate::turn::{
    ApprovalAnswer, CancelTurnRequest, TurnEventPayload, TurnReceipt, TurnSnapshot, TurnStatus,
};

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

    /// Scan a bounded page of durable roots and return only authorized public
    /// views. Cold reads install no worker, subscriber, context or queue runner.
    pub async fn list_threads(
        &self,
        epoch: &str,
        caller: &CallerContext,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
    ) -> Result<crate::thread::ThreadDirectoryPage, ServiceError> {
        self.ensure_instance(Some(epoch))?;
        if caller.is_anonymous()
            || !(1..=16).contains(&limit)
            || cutoff.is_some_and(|cutoff| after > cutoff)
        {
            return Err("authenticated caller and valid bounded directory page required".into());
        }
        let page = self
            .inner
            .store
            .read_index(after, cutoff, limit, self.inner.limits.recovery_page_bytes)
            .await
            .map_err(ServiceError::storage)?;
        if after > page.cutoff
            || cutoff.is_some_and(|cutoff| cutoff != page.cutoff)
            || page.entries.len() > limit
        {
            return Err(ServiceError::storage("invalid Thread directory page"));
        }
        let mut entries = Vec::new();
        let mut previous = after;
        for head in page.entries {
            if head.position <= previous || head.position > page.cutoff {
                return Err(ServiceError::storage("invalid Thread directory position"));
            }
            previous = head.position;
            let target = ThreadTarget {
                thread_id: head.execution_id,
                server_instance_id: epoch.into(),
            };
            let view = match self.read_stored_thread_view(&target, caller).await {
                Ok(view) => view,
                Err(error)
                    if matches!(
                        error.code,
                        ErrorCode::Unauthorized | ErrorCode::UnknownThread
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            entries.push(crate::thread::ThreadDirectoryEntry {
                turn_status: view.latest_turn.as_ref().map(|turn| turn.status),
                turn_id: view.latest_turn.as_ref().map(|turn| turn.turn_id.clone()),
                needs_input: view
                    .latest_turn
                    .as_ref()
                    .is_some_and(|turn| turn.pending_input_id.is_some()),
                thread: view.thread,
            });
        }
        if page
            .next_after
            .is_some_and(|next| next != previous || next <= after)
        {
            return Err(ServiceError::storage("Thread directory made no progress"));
        }
        let result = crate::thread::ThreadDirectoryPage {
            cutoff: page.cutoff,
            next_after: page.next_after,
            entries,
        };
        if serde_json::to_vec(&result)
            .map_err(|error| ServiceError::storage(error.to_string()))?
            .len()
            > self.inner.limits.history_page_bytes
        {
            return Err(ServiceError::new(
                ErrorCode::Overloaded,
                "Thread directory byte bound exceeded",
            ));
        }
        Ok(result)
    }

    pub async fn create_thread(
        &self,
        server_instance_id: &str,
        request: ThreadRequest,
    ) -> Result<ThreadSnapshot, ServiceError> {
        self.create_thread_with_servers(server_instance_id, request, None)
            .await
    }

    /// Bind immutable MCP descriptors selected by a trusted execution host.
    /// Public transports must authorize descriptors before invoking this method.
    pub async fn create_thread_with_servers(
        &self,
        server_instance_id: &str,
        request: ThreadRequest,
        servers: Option<Vec<bitrouter_sdk::mcp::transport::McpServerConfig>>,
    ) -> Result<ThreadSnapshot, ServiceError> {
        if let Some(servers) = &servers {
            if servers.len() > 32
                || serde_json::to_vec(servers)
                    .map_err(|e| e.to_string())?
                    .len()
                    > self.inner.limits.request_bytes
            {
                return Err("Thread MCP bindings exceed runtime bounds".into());
            }
            let mut names = std::collections::HashSet::new();
            for server in servers {
                server.validate().map_err(|e| e.to_string())?;
                if !names.insert(&server.name) {
                    return Err("duplicate Thread MCP server name".into());
                }
            }
        }
        self.ensure_instance(Some(server_instance_id))?;
        let admission = self.inner.admission.lock().await;
        let scope = key_scope(
            &request.caller,
            None,
            "create_thread",
            &request.idempotency_key,
        )?;
        let mut hash = fingerprint(&(
            &request.workspace,
            &request.config,
            request.permission_profile,
            &request.verification_command,
        ))?;
        if servers.is_some() {
            let mut bindings = serde_json::to_value(&servers).map_err(|error| error.to_string())?;
            bindings.sort_all_objects();
            hash = fingerprint(&(&hash, &bindings))?;
        }
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
        let mut facts = vec![
            ExecutionRecord::ThreadCreated {
                caller: request.caller.clone(),
                snapshot: snapshot.clone(),
                config: Box::new(config.clone()),
                verification_command: request.verification_command.clone(),
            },
            ExecutionRecord::AcceptedKey { entry: key.clone() },
        ];
        if let Some(servers) = &servers {
            facts.push(ExecutionRecord::ThreadResources {
                servers: servers.clone(),
            });
        }
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
                servers,
                presentation,
                snapshot: snapshot.clone(),
                caller: request.caller,
                config,
                verification_command: request.verification_command,
                messages: Vec::new(),
                instructions: None,
                instructions_epoch: None,
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

    /// Reopening may validate but cannot replace an immutable resource binding.
    pub fn check_thread_servers(
        &self,
        target: &ThreadTarget,
        caller: &CallerContext,
        servers: &[bitrouter_sdk::mcp::transport::McpServerConfig],
    ) -> Result<(), ServiceError> {
        self.ensure_instance(Some(&target.server_instance_id))?;
        let state = self.lock_state();
        let thread = state
            .threads
            .get(&target.thread_id)
            .ok_or_else(unknown_thread)?;
        thread.authorize(caller)?;
        self.check_thread_grant(&state, thread)?;
        let bound = thread
            .servers
            .as_deref()
            .unwrap_or(&self.inner.resources.servers);
        if serde_json::to_value(bound).map_err(|e| e.to_string())?
            != serde_json::to_value(servers).map_err(|e| e.to_string())?
        {
            return Err(ServiceError::new(
                ErrorCode::Conflict,
                "session MCP bindings cannot change on reopen",
            ));
        }
        Ok(())
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
        if matches!(
            thread.snapshot.status,
            ThreadStatus::RecoveryRequired | ThreadStatus::Closing
        ) {
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
}
