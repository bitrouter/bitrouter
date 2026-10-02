//! Durable FIFO root inputs. Acceptance reserves identity; activation alone
//! creates the run's context, budget and cost inventory.

use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedRootInput {
    pub operation_id: String,
    pub run_id: String,
    pub agent_turn_id: String,
    pub input: TaskInput,
    pub limits: Limits,
    /// Harness requirements can grow after acceptance without changing the
    /// caller's bounded input envelope or its original operation fingerprint.
    pub required_materials: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RootQueue {
    pub pending: VecDeque<QueuedRootInput>,
    pub paused: bool,
}

impl CoreSession {
    /// Accept a future root input without changing the active agent context.
    /// The immutable receipt reserves the same run/turn IDs across restoration.
    pub async fn enqueue(
        &self,
        operation_id: &str,
        expected_revision: u64,
        input: TaskInput,
    ) -> Result<OperationReceipt, CoreError> {
        let _input = self.shared.inputs.lock().await;
        validate_id(operation_id)?;
        let fingerprint = digest(&json!({"type":"input.enqueue",
            "expected_state_revision":expected_revision,"input":input}))?;
        if let Some(receipt) = self.replay(operation_id, &fingerprint).await? {
            return Ok(receipt);
        }
        validate_input(&input, &self.shared.limits)?;
        let run_id = id("run");
        self.transition_scoped(None, Some(&run_id), "input.enqueued", |state, head, _| {
            if head.state_revision != expected_revision {
                return Err(reject(
                    ErrorCode::StaleRevision,
                    "enqueue revision is stale",
                ));
            }
            if state.root_queue.pending.len() >= self.shared.limits.queued_runs as usize {
                return Err(reject(ErrorCode::LimitExceeded, "root queue is full"));
            }
            validate_verification(&input, &state.manifest)?;
            let mut resolved = input.clone();
            pin_required_materials(state, &mut resolved)?;
            let agent_turn_id = id("turn");
            let limits = input
                .limits
                .clone()
                .unwrap_or_else(|| self.shared.limits.clone());
            state.root_queue.pending.push_back(QueuedRootInput {
                operation_id: operation_id.into(),
                run_id: run_id.clone(),
                agent_turn_id: agent_turn_id.clone(),
                input,
                limits,
                required_materials: resolved.required_materials,
            });
            let receipt = OperationReceipt {
                operation_id: operation_id.into(),
                request_sha256: fingerprint,
                disposition: OperationDisposition::Accepted,
                assigned_ids: BTreeMap::from([
                    ("run_id".into(), run_id.clone()),
                    ("agent_id".into(), state.agent_id.clone()),
                    ("agent_turn_id".into(), agent_turn_id),
                ]),
                state_revision: head.state_revision + 1,
                error: None,
            };
            state
                .operations
                .insert(operation_id.into(), receipt.clone());
            encode(&receipt)
        })
        .await?;
        self.queue_receipt(operation_id).await
    }

    /// Resume only after current execution and effects have settled. This does
    /// not resolve a recovery-required run, and does not start a second driver.
    pub async fn resume_queue(
        &self,
        operation_id: &str,
        expected_revision: u64,
    ) -> Result<OperationReceipt, CoreError> {
        let _input = self.shared.inputs.lock().await;
        validate_id(operation_id)?;
        let fingerprint = digest(&json!({"type":"queue.resume",
            "expected_state_revision":expected_revision}))?;
        if let Some(receipt) = self.replay(operation_id, &fingerprint).await? {
            return Ok(receipt);
        }
        if self
            .shared
            .live
            .lock()
            .await
            .model_controls
            .iter()
            .any(|control| control.strong_count() > 0)
        {
            return Err(reject(
                ErrorCode::Busy,
                "previous SDK execution is still settling",
            ));
        }
        self.transition("queue.resumed", |state, head| {
            if head.state_revision != expected_revision {
                return Err(reject(
                    ErrorCode::StaleRevision,
                    "queue resume revision is stale",
                ));
            }
            if !settled(state) {
                return Err(reject(
                    ErrorCode::Busy,
                    "current execution must settle before resuming the queue",
                ));
            }
            state.root_queue.paused = false;
            let receipt = OperationReceipt {
                operation_id: operation_id.into(),
                request_sha256: fingerprint,
                disposition: OperationDisposition::Applied,
                assigned_ids: BTreeMap::new(),
                state_revision: head.state_revision + 1,
                error: None,
            };
            state
                .operations
                .insert(operation_id.into(), receipt.clone());
            encode(&receipt)
        })
        .await?;
        self.queue_receipt(operation_id).await
    }

    pub(super) async fn cancel_queued_root(
        &self,
        operation_id: &str,
        expected_revision: u64,
        run_id: &str,
        fingerprint: &str,
    ) -> Result<OperationReceipt, CoreError> {
        self.transition_scoped(None, Some(run_id), "input.cancelled", |state, head, _| {
            if head.state_revision != expected_revision {
                return Err(reject(
                    ErrorCode::StaleRevision,
                    "queued cancellation revision is stale",
                ));
            }
            let index = state
                .root_queue
                .pending
                .iter()
                .position(|entry| entry.run_id == run_id)
                .ok_or_else(|| reject(ErrorCode::Busy, "queued input already advanced"))?;
            let entry =
                state.root_queue.pending.remove(index).ok_or_else(|| {
                    reject(ErrorCode::CheckpointConflict, "queued input disappeared")
                })?;
            state.root_queue.paused = true;
            let receipt = OperationReceipt {
                operation_id: operation_id.into(),
                request_sha256: fingerprint.into(),
                disposition: OperationDisposition::Applied,
                assigned_ids: BTreeMap::from([
                    ("run_id".into(), entry.run_id),
                    ("input_operation_id".into(), entry.operation_id),
                    ("agent_turn_id".into(), entry.agent_turn_id),
                ]),
                state_revision: head.state_revision + 1,
                error: None,
            };
            state
                .operations
                .insert(operation_id.into(), receipt.clone());
            encode(&receipt)
        })
        .await?;
        self.queue_receipt(operation_id).await
    }

    async fn queue_receipt(&self, operation_id: &str) -> Result<OperationReceipt, CoreError> {
        self.operation(operation_id).await.ok_or_else(|| {
            reject(
                ErrorCode::CheckpointUnavailable,
                "queue operation was not acknowledged",
            )
        })
    }

    pub(super) async fn advance_root_queue(&self) -> Result<bool, CoreError> {
        let _input = self.shared.inputs.lock().await;
        if self
            .shared
            .live
            .lock()
            .await
            .model_controls
            .iter()
            .any(|control| control.strong_count() > 0)
        {
            return Ok(false);
        }
        let state = self.snapshot().await;
        if state.root_queue.paused || !settled(&state) {
            return Ok(false);
        }
        let Some(entry) = state.root_queue.pending.front() else {
            return Ok(false);
        };
        let outcome = self
            .transition_with_gate(None, "input.accepted", |state, head, dispatch| {
                if !dispatch {
                    return Err(reject(
                        ErrorCode::CheckpointUnavailable,
                        "queue activation awaits committed authority",
                    ));
                }
                if state.root_queue.paused
                    || !settled(state)
                    || state
                        .root_queue
                        .pending
                        .front()
                        .is_none_or(|next| next.run_id != entry.run_id)
                {
                    return Err(reject(ErrorCode::Busy, "queue activation boundary changed"));
                }
                let mut input = entry.input.clone();
                validate_input(&input, &entry.limits)?;
                for id in &entry.required_materials {
                    if !input.required_materials.contains(id) {
                        input.required_materials.push(id.clone());
                    }
                }
                validate_verification(&input, &state.manifest)?;
                pin_required_materials(state, &mut input)?;
                activate(
                    state,
                    &entry.run_id,
                    &entry.agent_turn_id,
                    input,
                    entry.limits.clone(),
                )?;
                state.root_queue.pending.pop_front();
                Ok(
                    json!({"operation_id":entry.operation_id,"run_id":entry.run_id,
                "agent_turn_id":entry.agent_turn_id,"state_revision":head.state_revision+1,
                "source":"queue"}),
                )
            })
            .await;
        if let Err(error) = &outcome
            && error.commit_status == CommitStatus::NotCommitted
            && !matches!(
                error.code,
                ErrorCode::Busy | ErrorCode::CheckpointUnavailable
            )
        {
            self.transition_scoped(None, Some(&entry.run_id), "queue.paused", |state, _, _| {
                state.root_queue.paused = true;
                Ok(json!({"operation_id":entry.operation_id,"error":error}))
            })
            .await?;
        }
        outcome.map(|()| true)
    }
}

fn settled(state: &SessionSnapshot) -> bool {
    state.run.as_ref().is_none_or(|run| run.status.terminal())
        && state.agents.values().all(|agent| {
            agent.queue.is_empty()
                && agent.turn.as_ref().is_none_or(|turn| {
                    turn.status.terminal()
                        && turn.invocations.iter().all(|call| {
                            call.result
                                .as_ref()
                                .is_some_and(|result| result.status != ToolOutcome::EffectUnknown)
                        })
                })
        })
}

pub(super) fn activate(
    state: &mut SessionSnapshot,
    run_id: &str,
    turn_id: &str,
    input: TaskInput,
    limits: Limits,
) -> Result<(), CoreError> {
    let agent = state
        .agents
        .get_mut(&state.agent_id)
        .ok_or_else(|| reject(ErrorCode::CheckpointConflict, "root agent is absent"))?;
    let history_start = agent.history.len();
    agent.history.push(Message::text(Role::User, &input.text));
    if !agent.required_instructions.contains(&input.text) {
        agent.required_instructions.push(input.text.clone());
    }
    agent.context_revision += 1;
    agent.turn = Some(AgentTurn {
        run_id: run_id.into(),
        agent_turn_id: turn_id.into(),
        assigned_by: agent.agent_id.clone(),
        input: input.clone(),
        allocation_id: None,
        history_start: Some(history_start),
        status: AgentStatus::Runnable,
        cancellation_requested: false,
        steps: Vec::new(),
        invocations: Vec::new(),
        core_calls: Vec::new(),
        final_answer: None,
        terminal_reason: None,
        notified: false,
    });
    state.cost_work.insert(run_id.into(), Default::default());
    state.run = Some(RootRun {
        run_id: run_id.into(),
        agent_turn_id: turn_id.into(),
        input,
        limits,
        status: RunStatus::Running,
        model_attempts: 0,
        token_accounting: Some(Default::default()),
        active_ms: 0,
        cancellation: None,
        final_answer: None,
        terminal_reason: None,
    });
    Ok(())
}

pub(super) fn validate(state: &SessionSnapshot, limits: &Limits) -> Result<(), CoreError> {
    if state.root_queue.pending.len() > limits.queued_runs as usize {
        return Err(reject(
            ErrorCode::LimitExceeded,
            "restored root queue exceeds negotiated bound",
        ));
    }
    let mut operation_ids = BTreeSet::new();
    let mut run_ids = BTreeSet::new();
    let mut turn_ids = state
        .agents
        .values()
        .filter_map(|agent| agent.turn.as_ref())
        .map(|turn| &turn.agent_turn_id)
        .collect::<BTreeSet<_>>();
    for entry in &state.root_queue.pending {
        for id in [&entry.operation_id, &entry.run_id, &entry.agent_turn_id] {
            validate_id(id)?;
        }
        if !operation_ids.insert(&entry.operation_id)
            || !run_ids.insert(&entry.run_id)
            || !turn_ids.insert(&entry.agent_turn_id)
        {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "duplicate queued identity",
            ));
        }
        for id in &entry.required_materials {
            validate_id(id)?;
        }
        entry.limits.within(limits)?;
        validate_input(&entry.input, &entry.limits)?;
        let receipt = state.operations.get(&entry.operation_id).ok_or_else(|| {
            reject(
                ErrorCode::CheckpointConflict,
                "queued input receipt is absent",
            )
        })?;
        let expected_revision = receipt.state_revision.checked_sub(1).ok_or_else(|| {
            reject(
                ErrorCode::CheckpointConflict,
                "queued input receipt has no acceptance revision",
            )
        })?;
        let fingerprint = digest(&json!({"type":"input.enqueue",
            "expected_state_revision":expected_revision,"input":entry.input}))?;
        if state.cost_work.contains_key(&entry.run_id)
            || state
                .run
                .as_ref()
                .is_some_and(|run| run.run_id == entry.run_id)
            || entry
                .input
                .limits
                .as_ref()
                .is_some_and(|requested| requested != &entry.limits)
            || receipt.operation_id != entry.operation_id
            || receipt.request_sha256 != fingerprint
            || receipt.error.is_some()
            || receipt.disposition != OperationDisposition::Accepted
            || receipt.assigned_ids.get("run_id") != Some(&entry.run_id)
            || receipt.assigned_ids.get("agent_turn_id") != Some(&entry.agent_turn_id)
            || receipt.assigned_ids.get("agent_id") != Some(&state.agent_id)
        {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "queued input has conflicting execution or receipt",
            ));
        }
    }
    Ok(())
}
