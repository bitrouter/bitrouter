//! Targeted durable input. Receipt and application are separate barriers;
//! provider output and workspace effects retain their original attribution.

use super::*;
use crate::core::checkpoint::{ToolStartFence, validate_tool_start_fences};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteeringDisposition {
    Received,
    Applied,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SteeringRecord {
    pub operation_id: String,
    pub run_id: String,
    pub agent_id: String,
    pub agent_turn_id: String,
    pub text: String,
    pub received_state_revision: u64,
    pub disposition: SteeringDisposition,
    pub resolved_state_revision: Option<u64>,
    pub tool_start_fences: Vec<ToolStartFence>,
}

pub(super) struct SteeringTarget {
    pub agent_id: String,
    pub run_id: String,
    pub agent_turn_id: String,
}

struct SteeringFence {
    session: CoreSession,
    id: Option<String>,
}

impl SteeringFence {
    async fn release(&mut self) {
        if let Some(id) = &self.id {
            self.session
                .shared
                .live
                .lock()
                .await
                .provisional_steering
                .remove(id);
            self.id = None;
            self.session.shared.steering_changed.notify_waiters();
            self.session.shared.changed.notify_one();
        }
    }
}

impl Drop for SteeringFence {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let session = self.session.clone();
            // Dropping an unaccepted request cannot strand its target. An
            // uncertain checkpoint retains its independent durable gate.
            tokio::spawn(async move {
                session
                    .shared
                    .live
                    .lock()
                    .await
                    .provisional_steering
                    .remove(&id);
                session.shared.steering_changed.notify_waiters();
                session.shared.changed.notify_one();
            });
        }
    }
}

pub(super) fn provisionally_blocked(live: &LiveSession, agent_id: &str) -> bool {
    live.state
        .agents
        .get(agent_id)
        .and_then(|agent| agent.turn.as_ref())
        .is_some_and(|turn| {
            live.provisional_steering.values().any(|target| {
                target.agent_id == agent_id
                    && target.run_id == turn.run_id
                    && target.agent_turn_id == turn.agent_turn_id
            })
        })
}

impl CoreSession {
    /// Accept input for exactly one active turn. The immutable acceptance
    /// receipt does not imply that the input has entered the model context.
    pub async fn steer(
        &self,
        operation_id: &str,
        expected_revision: u64,
        run_id: &str,
        agent_turn_id: &str,
        text: String,
    ) -> Result<OperationReceipt, CoreError> {
        validate_id(operation_id)?;
        let fingerprint = fingerprint(expected_revision, run_id, agent_turn_id, &text)?;
        if let Some(receipt) = self.replay(operation_id, &fingerprint).await? {
            return Ok(receipt);
        }
        validate_text(run_id, agent_turn_id, &text, &self.shared.limits)?;
        let fence_id = id("steering_gate");
        let target = {
            let mut live = self.shared.live.lock().await;
            let agent_id = live
                .state
                .agents
                .values()
                .find(|agent| {
                    agent.turn.as_ref().is_some_and(|turn| {
                        turn.run_id == run_id && turn.agent_turn_id == agent_turn_id
                    })
                })
                .map(|agent| agent.agent_id.clone())
                .ok_or_else(|| reject(ErrorCode::UnauthorizedScope, "unknown steering turn"))?;
            live.provisional_steering.insert(
                fence_id.clone(),
                SteeringTarget {
                    agent_id: agent_id.clone(),
                    run_id: run_id.into(),
                    agent_turn_id: agent_turn_id.into(),
                },
            );
            agent_id
        };
        let mut fence = SteeringFence {
            session: self.clone(),
            id: Some(fence_id),
        };
        let _input = self.shared.inputs.lock().await;
        let result = async {
            if let Some(receipt) = self.replay(operation_id, &fingerprint).await? {
                return Ok(receipt);
            }
            self.transition_for(Some(&target), "input.steer.received", |state, head| {
                if head.state_revision != expected_revision {
                    return Err(reject(
                        ErrorCode::StaleRevision,
                        "steering revision is stale",
                    ));
                }
                let run = state.run.as_ref().ok_or_else(|| {
                    reject(ErrorCode::Busy, "steering requires an active root run")
                })?;
                if run.run_id != run_id {
                    return Err(reject(ErrorCode::UnauthorizedScope, "steering run differs"));
                }
                if !matches!(run.status, RunStatus::Running | RunStatus::Waiting) {
                    return Err(reject(ErrorCode::Busy, "run no longer accepts steering"));
                }
                validate_text(run_id, agent_turn_id, &text, &run.limits)?;
                let agent = state
                    .agents
                    .values()
                    .find(|agent| {
                        agent.turn.as_ref().is_some_and(|turn| {
                            turn.run_id == run_id && turn.agent_turn_id == agent_turn_id
                        })
                    })
                    .ok_or_else(|| reject(ErrorCode::UnauthorizedScope, "unknown steering turn"))?;
                let turn = agent
                    .turn
                    .as_ref()
                    .ok_or_else(|| reject(ErrorCode::Busy, "steering target has no turn"))?;
                if turn.status.terminal()
                    || turn.cancellation_requested
                    || matches!(
                        turn.status,
                        AgentStatus::Cancelling | AgentStatus::RecoveryRequired
                    )
                {
                    return Err(reject(ErrorCode::Busy, "turn no longer accepts steering"));
                }
                if pending_count(state, &agent.agent_id) >= run.limits.mailbox_messages as usize {
                    return Err(reject(ErrorCode::LimitExceeded, "pending steering is full"));
                }
                let agent_id = agent.agent_id.clone();
                let tool_start_fences = turn
                    .invocations
                    .iter()
                    .filter(|call| call.result.is_none())
                    .map(|call| ToolStartFence {
                        invocation_id: call.dispatch.invocation_id.clone(),
                        attempt_id: call.dispatch.attempt_id.clone(),
                    })
                    .collect();
                let receipt = OperationReceipt {
                    operation_id: operation_id.into(),
                    request_sha256: fingerprint,
                    disposition: OperationDisposition::Accepted,
                    assigned_ids: identities(run_id, &agent_id, agent_turn_id),
                    state_revision: head.state_revision + 1,
                    error: None,
                };
                state.steering.insert(
                    operation_id.into(),
                    SteeringRecord {
                        operation_id: operation_id.into(),
                        run_id: run_id.into(),
                        agent_id,
                        agent_turn_id: agent_turn_id.into(),
                        text,
                        received_state_revision: receipt.state_revision,
                        disposition: SteeringDisposition::Received,
                        resolved_state_revision: None,
                        tool_start_fences,
                    },
                );
                state
                    .operations
                    .insert(operation_id.into(), receipt.clone());
                encode(&receipt)
            })
            .await?;
            self.operation(operation_id).await.ok_or_else(|| {
                reject(
                    ErrorCode::CheckpointUnavailable,
                    "steering receipt is not committed",
                )
            })
        }
        .await;
        fence.release().await;
        result
    }

    pub(super) async fn advance_steering(&self, agent_id: &str) -> Result<bool, CoreError> {
        let _input = self.shared.inputs.lock().await;
        let (state, unsent) = {
            let live = self.shared.live.lock().await;
            if !has_pending(&live.state, agent_id) {
                return Ok(false);
            }
            let turn = live
                .state
                .agents
                .get(agent_id)
                .and_then(|agent| agent.turn.as_ref())
                .ok_or_else(|| reject(ErrorCode::Busy, "steering turn ended"))?;
            if live.model_controls.iter().any(|control| {
                control.upgrade().is_some_and(|control| {
                    control.agent_id == agent_id
                        && control.agent_turn_id == turn.agent_turn_id
                        && control.run_id == turn.run_id
                })
            }) {
                return Ok(false);
            }
            let unsent = turn
                .invocations
                .iter()
                .filter(|call| {
                    call.result.is_none() && !live.sent_tools.contains(&call.dispatch.invocation_id)
                })
                .map(|call| call.dispatch.invocation_id.clone())
                .collect::<BTreeSet<_>>();
            (live.state.clone(), unsent)
        };
        let turn = state
            .agents
            .get(agent_id)
            .and_then(|agent| agent.turn.as_ref())
            .ok_or_else(|| reject(ErrorCode::Busy, "steering target has no turn"))?;
        if !unsent.is_empty()
            || turn.core_calls.iter().any(|call| call.result.is_none())
            || turn.steps.last().is_some_and(|step| !step.settled)
        {
            self.transition_for(Some(agent_id), "input.steer.quiescing", |state, _| {
                let turn = agent_turn(state, agent_id)?;
                if let Some(step) = turn.steps.last_mut().filter(|step| !step.settled) {
                    step.interrupted = true;
                    step.settled = true;
                }
                for call in &mut turn.invocations {
                    if unsent.contains(&call.dispatch.invocation_id) && call.result.is_none() {
                        call.result = Some(ToolResult {
                            invocation_id: call.dispatch.invocation_id.clone(),
                            attempt_id: call.dispatch.attempt_id.clone(),
                            status: ToolOutcome::NotExecuted,
                            output: String::new(),
                            evidence: Vec::new(),
                            workspace_revision: None,
                        });
                    }
                }
                for call in &mut turn.core_calls {
                    if call.result.is_none() {
                        call.result = Some(json!({"ok":false,"reason":"superseded by steering"}));
                    }
                }
                Ok(json!({"invocation_ids":unsent}))
            })
            .await?;
            return Ok(true);
        }
        if turn.invocations.iter().any(|call| {
            call.result
                .as_ref()
                .is_none_or(|result| result.status == ToolOutcome::EffectUnknown)
        }) {
            return Ok(false);
        }
        if turn.invocations.iter().any(|call| !call.consumed)
            || turn.core_calls.iter().any(|call| !call.consumed)
        {
            self.consume_results(agent_id).await?;
            return Ok(true);
        }
        self.transition_for(Some(agent_id), "input.steer.applied", |state, head| {
            let mut received = state
                .steering
                .values()
                .filter(|record| {
                    record.agent_id == agent_id
                        && record.disposition == SteeringDisposition::Received
                })
                .cloned()
                .collect::<Vec<_>>();
            received.sort_by_key(|record| record.received_state_revision);
            let agent = agent_mut(state, agent_id)?;
            let turn = agent
                .turn
                .as_mut()
                .ok_or_else(|| reject(ErrorCode::Busy, "steering turn ended"))?;
            if turn.status.terminal()
                || turn.cancellation_requested
                || matches!(
                    turn.status,
                    AgentStatus::Cancelling | AgentStatus::RecoveryRequired
                )
                || received.iter().any(|record| {
                    record.run_id != turn.run_id || record.agent_turn_id != turn.agent_turn_id
                })
            {
                return Err(reject(ErrorCode::Busy, "steering safe boundary changed"));
            }
            for record in &received {
                agent.history.push(Message::text(Role::User, &record.text));
                if !agent.required_instructions.contains(&record.text) {
                    agent.required_instructions.push(record.text.clone());
                }
            }
            agent.context_revision = agent
                .context_revision
                .checked_add(1)
                .ok_or_else(|| reject(ErrorCode::LimitExceeded, "context revision exhausted"))?;
            turn.final_answer = None;
            turn.terminal_reason = None;
            turn.status = AgentStatus::Runnable;
            let operation_ids = received
                .iter()
                .map(|record| record.operation_id.clone())
                .collect::<Vec<_>>();
            for record in &mut received {
                record.disposition = SteeringDisposition::Applied;
                record.resolved_state_revision = Some(head.state_revision + 1);
                state
                    .steering
                    .insert(record.operation_id.clone(), record.clone());
            }
            Ok(json!({"operation_ids":operation_ids}))
        })
        .await?;
        Ok(true)
    }

    /// The SDK has already settled this step. A steering receipt can invalidate
    /// dependent output, but cannot erase provider execution or cost evidence.
    /// The caller holds the input serializer through supersession or application.
    pub(super) async fn supersede_steered_step(
        &self,
        agent_id: &str,
        step_id: &str,
    ) -> Result<bool, CoreError> {
        if !has_pending(&self.snapshot().await, agent_id) {
            return Ok(false);
        }
        self.transition_for(Some(agent_id), "model.output.superseded", |state, _| {
            let step = current_step(state, agent_id, step_id)?;
            step.settled = true;
            step.interrupted = true;
            agent_turn(state, agent_id)?.status = AgentStatus::Runnable;
            Ok(json!({"step_id":step_id,"reason":"steering received"}))
        })
        .await?;
        Ok(true)
    }
}

fn identities(run_id: &str, agent_id: &str, turn_id: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("run_id".into(), run_id.into()),
        ("agent_id".into(), agent_id.into()),
        ("agent_turn_id".into(), turn_id.into()),
    ])
}

fn fingerprint(
    revision: u64,
    run_id: &str,
    turn_id: &str,
    text: &str,
) -> Result<String, CoreError> {
    digest(
        &json!({"type":"input.steer","expected_state_revision":revision,
        "run_id":run_id,"agent_turn_id":turn_id,"text":text}),
    )
}

fn validate_text(
    run_id: &str,
    turn_id: &str,
    text: &str,
    limits: &Limits,
) -> Result<(), CoreError> {
    validate_id(run_id)?;
    validate_id(turn_id)?;
    if text.is_empty() {
        return Err(reject(
            ErrorCode::NoFeasibleRoute,
            "steering text is required",
        ));
    }
    if serde_json::to_vec(&json!({"run_id":run_id,"agent_turn_id":turn_id,"text":text}))
        .map_err(json_error)?
        .len() as u64
        > limits.input_bytes
    {
        return Err(reject(
            ErrorCode::LimitExceeded,
            "steering input exceeds bound",
        ));
    }
    Ok(())
}

fn pending_count(state: &SessionSnapshot, agent_id: &str) -> usize {
    state
        .steering
        .values()
        .filter(|record| {
            record.agent_id == agent_id && record.disposition == SteeringDisposition::Received
        })
        .count()
}

pub(super) fn has_pending(state: &SessionSnapshot, agent_id: &str) -> bool {
    pending_count(state, agent_id) > 0
}

pub(super) fn ensure_ready(state: &SessionSnapshot, agent_id: &str) -> Result<(), CoreError> {
    if has_pending(state, agent_id) {
        return Err(reject(
            ErrorCode::Busy,
            "steering awaits a safe context boundary",
        ));
    }
    Ok(())
}

/// Retain cancellation in the same checkpoint as the owning run/turn change.
pub(super) fn cancel_inactive(state: &mut SessionSnapshot, revision: u64) {
    for record in state
        .steering
        .values_mut()
        .filter(|record| record.disposition == SteeringDisposition::Received)
    {
        if state.run.as_ref().is_none_or(|run| {
            run.run_id != record.run_id || run.status.terminal() || run.cancellation.is_some()
        }) || state
            .agents
            .get(&record.agent_id)
            .and_then(|agent| agent.turn.as_ref())
            .is_none_or(|turn| {
                turn.run_id != record.run_id
                    || turn.agent_turn_id != record.agent_turn_id
                    || turn.status.terminal()
                    || turn.cancellation_requested
                    || turn.status == AgentStatus::Cancelling
            })
        {
            record.disposition = SteeringDisposition::Cancelled;
            record.resolved_state_revision = Some(revision);
        }
    }
}

pub(super) fn validate(
    state: &SessionSnapshot,
    limits: &Limits,
    revision: u64,
) -> Result<(), CoreError> {
    for (id, record) in &state.steering {
        validate_tool_start_fences(&record.tool_start_fences)?;
        validate_id(id)?;
        validate_id(&record.agent_id)?;
        validate_text(&record.run_id, &record.agent_turn_id, &record.text, limits)?;
        let receipt = state
            .operations
            .get(id)
            .ok_or_else(|| reject(ErrorCode::CheckpointConflict, "steering receipt missing"))?;
        let prior = record
            .received_state_revision
            .checked_sub(1)
            .ok_or_else(|| reject(ErrorCode::CheckpointConflict, "invalid steering revision"))?;
        if id != &record.operation_id
            || receipt.operation_id != *id
            || receipt.state_revision != record.received_state_revision
            || receipt.state_revision > revision
            || receipt.disposition != OperationDisposition::Accepted
            || receipt.error.is_some()
            || receipt.assigned_ids
                != identities(&record.run_id, &record.agent_id, &record.agent_turn_id)
            || receipt.request_sha256
                != fingerprint(prior, &record.run_id, &record.agent_turn_id, &record.text)?
            || match record.disposition {
                SteeringDisposition::Received => record.resolved_state_revision.is_some(),
                SteeringDisposition::Applied | SteeringDisposition::Cancelled => {
                    record.resolved_state_revision.is_none_or(|resolved| {
                        resolved <= record.received_state_revision || resolved > revision
                    })
                }
            }
        {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "steering identity or disposition is inconsistent",
            ));
        }
        if record.disposition == SteeringDisposition::Received {
            let run = state
                .run
                .as_ref()
                .filter(|run| {
                    run.run_id == record.run_id
                        && !run.status.terminal()
                        && run.cancellation.is_none()
                })
                .ok_or_else(|| {
                    reject(ErrorCode::CheckpointConflict, "pending steering run ended")
                })?;
            validate_text(
                &record.run_id,
                &record.agent_turn_id,
                &record.text,
                &run.limits,
            )?;
            let turn = state
                .agents
                .get(&record.agent_id)
                .and_then(|agent| agent.turn.as_ref())
                .ok_or_else(|| reject(ErrorCode::CheckpointConflict, "steering target missing"))?;
            if record.tool_start_fences.iter().any(|fence| {
                !turn.invocations.iter().any(|call| {
                    call.dispatch.invocation_id == fence.invocation_id
                        && call.dispatch.attempt_id == fence.attempt_id
                })
            }) || turn.invocations.iter().any(|call| {
                call.result.is_none()
                    && !record.tool_start_fences.iter().any(|fence| {
                        fence.invocation_id == call.dispatch.invocation_id
                            && fence.attempt_id == call.dispatch.attempt_id
                    })
            }) {
                return Err(reject(
                    ErrorCode::CheckpointConflict,
                    "steering tool start fences differ from target",
                ));
            }
            if state
                .agents
                .get(&record.agent_id)
                .and_then(|agent| agent.turn.as_ref())
                .is_none_or(|turn| {
                    turn.run_id != record.run_id
                        || turn.agent_turn_id != record.agent_turn_id
                        || turn.status.terminal()
                        || turn.cancellation_requested
                        || turn.status == AgentStatus::Cancelling
                })
                || pending_count(state, &record.agent_id) > run.limits.mailbox_messages as usize
            {
                return Err(reject(
                    ErrorCode::CheckpointConflict,
                    "pending steering target or bound is invalid",
                ));
            }
        }
    }
    Ok(())
}
