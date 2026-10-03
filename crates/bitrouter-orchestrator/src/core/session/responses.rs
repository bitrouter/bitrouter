//! Durable response exchanges over the existing scheduler. An HTTP exchange
//! can finish at a tool boundary while its root run remains unfinished.
//! Reference: <https://developers.openai.com/api/docs/guides/responses-multi-agent>

use super::*;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseState {
    pub latest: Option<String>,
    pub exchanges: BTreeMap<String, ResponseExchange>,
}

impl ResponseState {
    pub fn is_empty(&self) -> bool {
        self.latest.is_none() && self.exchanges.is_empty()
    }
}

/// Retained core data for projection by a Responses adapter. Provider metadata
/// is not a public wire item and must never be relabeled as encrypted state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseOutput {
    pub event_seq: u64,
    pub agent_id: String,
    pub agent_name: String,
    pub agent_turn_id: String,
    pub step_id: String,
    pub message: Message,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseExchange {
    pub response_id: String,
    pub operation_id: String,
    pub run_id: String,
    pub previous_response_id: Option<String>,
    pub created_state_revision: u64,
    pub completed_state_revision: Option<u64>,
    pub run_status: Option<RunStatus>,
    pub output: Vec<ResponseOutput>,
    /// Public call IDs map to the exact retained invocation/attempt. This is
    /// the exchange's terminal view, not a mutable view of later tool results.
    pub pending: BTreeMap<String, ToolExecute>,
}

fn latest(state: &SessionSnapshot) -> Option<&ResponseExchange> {
    state
        .responses
        .latest
        .as_ref()
        .and_then(|id| state.responses.exchanges.get(id))
}

pub(super) fn active_id(state: &SessionSnapshot) -> Option<&str> {
    latest(state)
        .filter(|response| response.completed_state_revision.is_none())
        .map(|response| response.response_id.as_str())
}

pub(super) fn awaiting_continuation(state: &SessionSnapshot) -> bool {
    latest(state).is_some_and(|response| {
        response.completed_state_revision.is_some()
            && state.run.as_ref().is_some_and(|run| {
                run.run_id == response.run_id
                    && matches!(run.status, RunStatus::Running | RunStatus::Waiting)
                    && run.cancellation.is_none()
                    && run.resource_error.is_none()
            })
    })
}

pub(super) fn authorized(state: &SessionSnapshot, call: &ToolExecute) -> bool {
    call.response_id.as_ref().is_none_or(|id| {
        state.responses.exchanges.get(id).is_some_and(|response| {
            response.completed_state_revision.is_some()
                && response.run_id == call.run_id
                && response.pending.values().any(|pending| {
                    pending.invocation_id == call.invocation_id
                        && pending.attempt_id == call.attempt_id
                })
        })
    })
}

pub(super) fn begin(
    state: &mut SessionSnapshot,
    operation_id: &str,
    run_id: &str,
    previous_response_id: Option<String>,
    revision: u64,
) -> Result<String, CoreError> {
    if active_id(state).is_some() {
        return Err(reject(
            ErrorCode::Busy,
            "a response exchange is already active",
        ));
    }
    let response_id = id("resp");
    state.responses.exchanges.insert(
        response_id.clone(),
        ResponseExchange {
            response_id: response_id.clone(),
            operation_id: operation_id.into(),
            run_id: run_id.into(),
            previous_response_id,
            created_state_revision: revision,
            completed_state_revision: None,
            run_status: None,
            output: Vec::new(),
            pending: BTreeMap::new(),
        },
    );
    state.responses.latest = Some(response_id.clone());
    Ok(response_id)
}

pub(super) fn capture(state: &mut SessionSnapshot, event: &DurableEvent) -> Result<(), CoreError> {
    if event.kind != "model.output.applied" || event.payload.get("discarded").is_some() {
        return Ok(());
    }
    let Some(response_id) = active_id(state).map(str::to_owned) else {
        return Ok(());
    };
    let agent = event
        .agent_id
        .as_ref()
        .and_then(|id| state.agents.get(id))
        .ok_or_else(|| conflict("response output has no agent"))?;
    let turn = agent
        .turn
        .as_ref()
        .ok_or_else(|| conflict("response output has no turn"))?;
    let step_id = event.payload["step_id"]
        .as_str()
        .ok_or_else(|| conflict("response output has no step"))?;
    let output = ResponseOutput {
        event_seq: event.event_seq,
        agent_id: agent.agent_id.clone(),
        agent_name: agent.display_path.clone(),
        agent_turn_id: turn.agent_turn_id.clone(),
        step_id: step_id.into(),
        message: agent
            .history
            .last()
            .cloned()
            .ok_or_else(|| conflict("applied response output is missing"))?,
    };
    let response = state
        .responses
        .exchanges
        .get_mut(&response_id)
        .ok_or_else(|| conflict("active response is missing"))?;
    if response.run_id != turn.run_id || response.output.iter().any(|item| item.step_id == step_id)
    {
        return Err(conflict(
            "response output has duplicate or foreign ownership",
        ));
    }
    response.output.push(output);
    Ok(())
}

fn pending(state: &SessionSnapshot, run_id: &str) -> BTreeMap<String, ToolExecute> {
    state
        .agents
        .values()
        .filter_map(|agent| agent.turn.as_ref())
        .filter(|turn| turn.run_id == run_id)
        .flat_map(|turn| &turn.invocations)
        .filter(|call| {
            call.result
                .as_ref()
                .is_none_or(|result| result.status == ToolOutcome::EffectUnknown)
        })
        .map(|call| (call.public_call_id.clone(), call.dispatch.clone()))
        .collect()
}

fn complete(
    state: &mut SessionSnapshot,
    response_id: &str,
    revision: u64,
) -> Result<Value, CoreError> {
    let run = state
        .run
        .as_ref()
        .ok_or_else(|| conflict("response run is missing"))?;
    let calls = pending(state, &run.run_id);
    let response = state
        .responses
        .exchanges
        .get_mut(response_id)
        .ok_or_else(|| conflict("response is missing"))?;
    if response.completed_state_revision.is_some() || response.run_id != run.run_id {
        return Err(reject(
            ErrorCode::OperationConflict,
            "response was completed or its run changed",
        ));
    }
    response.completed_state_revision = Some(revision);
    response.run_status = Some(run.status);
    response.pending = calls;
    Ok(json!({"response_id":response_id,"run_id":run.run_id,
        "state_revision":revision,"run_status":run.status}))
}

impl CoreSession {
    /// Accept a root task and response identity in the same durable transition.
    pub async fn start_response(
        &self,
        operation_id: &str,
        expected_revision: u64,
        input: TaskInput,
    ) -> Result<OperationReceipt, CoreError> {
        self.start_input(operation_id, expected_revision, input, true)
            .await
    }

    /// Results and controls use the existing core methods before continuation.
    /// Merely reading/replaying a completed exchange never resumes its run.
    pub async fn continue_response(
        &self,
        operation_id: &str,
        expected_revision: u64,
        previous_response_id: &str,
    ) -> Result<OperationReceipt, CoreError> {
        let _input = self.shared.inputs.lock().await;
        validate_id(operation_id)?;
        validate_id(previous_response_id)?;
        let fingerprint = digest(
            &json!({"type":"response.continue", "expected_state_revision":expected_revision,
            "previous_response_id":previous_response_id}),
        )?;
        if let Some(receipt) = self.replay(operation_id, &fingerprint).await? {
            return Ok(receipt);
        }
        self.transition("response.accepted", |state, head| {
            if head.state_revision != expected_revision {
                return Err(reject(
                    ErrorCode::StaleRevision,
                    "response continuation revision is stale",
                ));
            }
            let previous = latest(state)
                .filter(|response| response.response_id == previous_response_id)
                .ok_or_else(|| {
                    reject(
                        ErrorCode::OperationConflict,
                        "continuation must reference the latest response",
                    )
                })?;
            let completed = previous
                .completed_state_revision
                .ok_or_else(|| reject(ErrorCode::Busy, "previous response is still active"))?;
            let run = state
                .run
                .as_ref()
                .filter(|run| run.run_id == previous.run_id && !run.status.terminal())
                .ok_or_else(|| {
                    reject(
                        ErrorCode::OperationConflict,
                        "response run cannot be continued",
                    )
                })?;
            if completed == head.state_revision {
                return Err(reject(
                    ErrorCode::Busy,
                    "continuation requires new results or control state",
                ));
            }
            let run_id = run.run_id.clone();
            let response_id = begin(
                state,
                operation_id,
                &run_id,
                Some(previous_response_id.into()),
                head.state_revision + 1,
            )?;
            let receipt = OperationReceipt {
                operation_id: operation_id.into(),
                request_sha256: fingerprint,
                disposition: OperationDisposition::Accepted,
                assigned_ids: BTreeMap::from([
                    ("run_id".into(), run_id),
                    ("response_id".into(), response_id),
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
        self.operation(operation_id)
            .await
            .ok_or_else(|| conflict("response receipt is missing"))
    }

    pub async fn response(&self, response_id: &str) -> Option<ResponseExchange> {
        self.shared
            .live
            .lock()
            .await
            .state
            .responses
            .exchanges
            .get(response_id)
            .cloned()
    }

    /// Drive the shared scheduler to an exchange boundary, commit completion,
    /// then release tool delivery. A matching terminal ACK is required even if
    /// the response consumer has disconnected or has not read its SSE frame.
    pub async fn drive_response(&self, response_id: &str) -> Result<ResponseExchange, CoreError> {
        let _driver = self
            .shared
            .driver
            .try_lock()
            .map_err(|_| reject(ErrorCode::Busy, "session driver is already active"))?;
        loop {
            let response = self
                .response(response_id)
                .await
                .ok_or_else(|| reject(ErrorCode::UnauthorizedScope, "unknown response"))?;
            if response.completed_state_revision.is_some() {
                self.dispatch_response_tools(&response).await?;
                return Ok(response);
            }
            let revision = match self.drive_inner().await {
                Ok((_, revision)) => revision,
                Err(error) => {
                    let (state, revision) = self.drive_view().await;
                    if !self.can_progress().await
                        || state.run.as_ref().is_none_or(|run| {
                            run.run_id != response.run_id
                                || !(run.status.terminal()
                                    || run.status == RunStatus::RecoveryRequired)
                        })
                    {
                        return Err(error);
                    }
                    // A settled failure still needs an immutable exchange
                    // boundary. Transport/commit errors remain retryable.
                    revision
                }
            };
            let _input = self.shared.inputs.lock().await;
            let result = self
                .transition("response.completed", |state, head| {
                    if head.state_revision != revision {
                        return Err(reject(ErrorCode::Busy, "response boundary changed"));
                    }
                    if state
                        .agents
                        .values()
                        .filter_map(|agent| agent.turn.as_ref())
                        .filter(|turn| turn.run_id == response.run_id)
                        .any(|turn| turn.steps.iter().any(|step| !step.settled))
                    {
                        return Err(reject(
                            ErrorCode::RecoveryRequired,
                            "pending model output must be reconciled before response completion",
                        ));
                    }
                    complete(state, response_id, head.state_revision + 1)
                })
                .await;
            drop(_input);
            match result {
                Err(error) if error.code == ErrorCode::Busy => continue,
                Err(error) => return Err(error),
                Ok(()) => {}
            }
        }
    }

    async fn dispatch_response_tools(&self, response: &ResponseExchange) -> Result<(), CoreError> {
        let state = self.snapshot().await;
        if state.responses.latest.as_ref() != Some(&response.response_id)
            || state.run.as_ref().is_none_or(|run| {
                run.run_id != response.run_id
                    || !matches!(run.status, RunStatus::Running | RunStatus::Waiting)
            })
        {
            return Ok(());
        }
        for agent_id in state.agents.keys() {
            self.dispatch_tools(agent_id).await?;
        }
        Ok(())
    }
}

/// Reserve completion before any new output/call becomes durable. Only this
/// private size projection uses padded counters or a hypothetical run status.
pub(super) fn reserve_terminal(state: &mut SessionSnapshot) -> Result<Option<Value>, CoreError> {
    let Some(response_id) = active_id(state).map(str::to_owned) else {
        return Ok(None);
    };
    let mut event = complete(state, &response_id, u64::MAX)?;
    event["run_status"] = json!(RunStatus::RecoveryRequired);
    let response = state
        .responses
        .exchanges
        .get_mut(&response_id)
        .ok_or_else(|| conflict("response projection is missing"))?;
    response.run_status = Some(RunStatus::RecoveryRequired);
    for command in response.pending.values_mut() {
        command.execution_epoch = u64::MAX;
        command.authorizing_event_seq = u64::MAX;
    }
    Ok(Some(event))
}

pub(super) fn validate(state: &SessionSnapshot, binding: &Bind) -> Result<(), CoreError> {
    let records = &state.responses.exchanges;
    let newest = records
        .values()
        .max_by_key(|response| response.created_state_revision);
    if newest.map(|response| &response.response_id) != state.responses.latest.as_ref() {
        return Err(conflict(
            "latest response identity differs from retained history",
        ));
    }
    let mut revisions = BTreeSet::new();
    let mut steps = BTreeSet::new();
    let mut successors = BTreeSet::new();
    for (id, response) in records {
        validate_id(id)?;
        validate_id(&response.operation_id)?;
        validate_id(&response.run_id)?;
        let receipt = state
            .operations
            .get(&response.operation_id)
            .ok_or_else(|| conflict("response acceptance receipt is missing"))?;
        if id != &response.response_id
            || response.created_state_revision == 0
            || response.created_state_revision > binding.durable_head.state_revision
            || !revisions.insert(response.created_state_revision)
            || receipt.assigned_ids.get("response_id") != Some(id)
            || receipt.assigned_ids.get("run_id") != Some(&response.run_id)
            || receipt.state_revision != response.created_state_revision
        {
            return Err(conflict("response identity or acceptance boundary differs"));
        }
        if let Some(previous_id) = &response.previous_response_id {
            let previous = records
                .get(previous_id)
                .ok_or_else(|| conflict("previous response is missing"))?;
            if previous.run_id != response.run_id
                || !successors.insert(previous_id)
                || previous
                    .completed_state_revision
                    .is_none_or(|revision| revision >= response.created_state_revision)
            {
                return Err(conflict(
                    "response continuation is forked or precedes completion",
                ));
            }
        }
        match response.completed_state_revision {
            Some(revision)
                if revision > response.created_state_revision
                    && revision <= binding.durable_head.state_revision
                    && response.run_status.is_some() => {}
            None if response.run_status.is_none()
                && response.pending.is_empty()
                && state.responses.latest.as_ref() == Some(id)
                && state
                    .run
                    .as_ref()
                    .is_some_and(|run| run.run_id == response.run_id) => {}
            _ => return Err(conflict("response completion boundary is invalid")),
        }
        let mut previous_seq = 0;
        for output in &response.output {
            validate_id(&output.agent_id)?;
            validate_id(&output.agent_turn_id)?;
            validate_id(&output.step_id)?;
            if !steps.insert(&output.step_id)
                || output.message.role != Role::Assistant
                || output.event_seq <= previous_seq
                || output.event_seq > binding.durable_head.event_seq
            {
                return Err(conflict(
                    "response output attribution or sequence is invalid",
                ));
            }
            previous_seq = output.event_seq;
        }
        for (call_id, call) in &response.pending {
            validate_id(call_id)?;
            validate_id(&call.invocation_id)?;
            validate_id(&call.attempt_id)?;
            let origin = call
                .response_id
                .as_ref()
                .and_then(|id| records.get(id))
                .ok_or_else(|| conflict("pending response call has no exchange"))?;
            if call.run_id != response.run_id
                || origin.run_id != response.run_id
                || origin.completed_state_revision.is_none()
                || origin.completed_state_revision > response.completed_state_revision
                || origin
                    .pending
                    .get(call_id)
                    .is_none_or(|original| !same_command(original, call))
            {
                return Err(conflict("pending call exceeds its response authorization"));
            }
        }
    }
    for receipt in state.operations.values() {
        if let Some(id) = receipt.assigned_ids.get("response_id")
            && records
                .get(id)
                .is_none_or(|response| response.operation_id != receipt.operation_id)
        {
            return Err(conflict("response acceptance was erased or aliased"));
        }
    }
    for call in state
        .agents
        .values()
        .filter_map(|agent| agent.turn.as_ref())
        .flat_map(|turn| &turn.invocations)
    {
        if let Some(id) = &call.dispatch.response_id {
            let response = records
                .get(id)
                .ok_or_else(|| conflict("tool exchange is missing"))?;
            if response.run_id != call.dispatch.run_id
                || (response.completed_state_revision.is_some()
                    && call.result.as_ref().is_none_or(|result| {
                        !matches!(
                            result.status,
                            ToolOutcome::Denied | ToolOutcome::NotExecuted
                        )
                    })
                    && response
                        .pending
                        .get(&call.public_call_id)
                        .is_none_or(|original| !same_command(original, &call.dispatch)))
                || (response.completed_state_revision.is_none()
                    && (call.result.as_ref().is_some_and(|result| {
                        !matches!(
                            result.status,
                            ToolOutcome::Denied | ToolOutcome::NotExecuted
                        )
                    }) || tool_status::observed(
                        call,
                        super::super::protocol::ToolStatus::Running,
                    ) || tool_status::observed(
                        call,
                        super::super::protocol::ToolStatus::Stopped,
                    )))
            {
                return Err(conflict("tool executed before its exchange completion"));
            }
        } else if records
            .values()
            .any(|response| response.run_id == call.dispatch.run_id)
        {
            return Err(conflict("managed tool lost its response authorization"));
        }
    }
    Ok(())
}

pub(super) fn validate_history(
    payload: &CheckpointPayload,
    previous: &mut Option<ResponseState>,
) -> Result<(), CoreError> {
    let current: ResponseState = payload
        .checkpoint
        .state
        .get("responses")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(json_error)?
        .unwrap_or_default();
    if let Some(previous) = previous.as_ref() {
        for (id, old) in &previous.exchanges {
            let new = current
                .exchanges
                .get(id)
                .ok_or_else(|| conflict("response history was erased"))?;
            if old.completed_state_revision.is_some() {
                if new != old {
                    return Err(conflict("completed response was rewritten"));
                }
            } else if new.response_id != old.response_id
                || new.operation_id != old.operation_id
                || new.run_id != old.run_id
                || new.previous_response_id != old.previous_response_id
                || new.created_state_revision != old.created_state_revision
                || !new.output.starts_with(&old.output)
                || new
                    .completed_state_revision
                    .is_some_and(|revision| revision != payload.checkpoint.state_revision)
            {
                return Err(conflict("active response history was rewritten"));
            }
        }
    }
    for (id, response) in &current.exchanges {
        let old = previous.as_ref().and_then(|state| state.exchanges.get(id));
        if old.is_none() && response.created_state_revision == payload.checkpoint.state_revision {
            if !payload.events.iter().any(|event| {
                matches!(event.kind.as_str(), "input.accepted" | "response.accepted")
                    && event.payload["assigned_ids"]["response_id"].as_str() == Some(id)
            }) {
                return Err(conflict("response acceptance event is missing"));
            }
        } else if old.is_none() && previous.is_some() {
            return Err(conflict(
                "response appeared without its acceptance transition",
            ));
        }
        if response.completed_state_revision == Some(payload.checkpoint.state_revision)
            && !payload.events.iter().any(|event| {
                event.kind == "response.completed"
                    && event.payload["response_id"].as_str() == Some(id)
                    && event.payload["run_id"].as_str() == Some(&response.run_id)
                    && event.payload["run_status"] == json!(response.run_status)
                    && event.payload["state_revision"].as_u64() == response.completed_state_revision
            })
        {
            return Err(conflict(
                "response completion event is missing or inconsistent",
            ));
        }
        if previous.is_some() {
            for output in response
                .output
                .iter()
                .skip(old.map_or(0, |record| record.output.len()))
            {
                if !payload.events.iter().any(|event| {
                    event.kind == "model.output.applied"
                        && event.event_seq == output.event_seq
                        && event.agent_id.as_ref() == Some(&output.agent_id)
                        && event.payload["step_id"].as_str() == Some(&output.step_id)
                }) {
                    return Err(conflict("response output has no applied model event"));
                }
                let agent = &payload.checkpoint.state["agents"][&output.agent_id];
                if agent["display_path"].as_str() != Some(&output.agent_name)
                    || agent["turn"]["agent_turn_id"].as_str() != Some(&output.agent_turn_id)
                    || agent["turn"]["run_id"].as_str() != Some(&response.run_id)
                    || agent["history"]
                        .as_array()
                        .and_then(|history| history.last())
                        != Some(&encode(&output.message)?)
                {
                    return Err(conflict("response output differs from its applied message"));
                }
            }
        }
    }
    *previous = Some(current);
    Ok(())
}

fn conflict(message: &str) -> CoreError {
    reject(ErrorCode::CheckpointConflict, message)
}

fn same_command(original: &ToolExecute, current: &ToolExecute) -> bool {
    // Restoration can reauthorize an unstarted attempt under a new owner.
    // The completed response retains its original immutable terminal view.
    let mut original = original.clone();
    original.execution_epoch = current.execution_epoch;
    original.authorizing_event_seq = current.authorizing_event_seq;
    original == *current
}
