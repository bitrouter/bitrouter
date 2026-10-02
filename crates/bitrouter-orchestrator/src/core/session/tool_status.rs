//! Authenticated harness lifecycle evidence, separate from tool outcomes.

use super::*;
use crate::core::protocol::{ToolObservation, ToolStatus};

impl CoreSession {
    /// Record a harness observation for an already dispatched invocation. This
    /// neither authorizes another execute nor supplies a missing tool result.
    pub async fn tool_status(
        &self,
        operation_id: &str,
        observation: ToolObservation,
    ) -> Result<OperationReceipt, CoreError> {
        let _input = self.shared.inputs.lock().await;
        validate_id(operation_id)?;
        let fingerprint = digest(&json!({"type":"tool.status","observation":observation}))?;
        if let Some(receipt) = self.replay(operation_id, &fingerprint).await? {
            return Ok(receipt);
        }
        validate_id(&observation.invocation_id)?;
        validate_id(&observation.attempt_id)?;
        let bytes = serde_json::to_vec(&observation).map_err(json_error)?.len() as u64;
        let (target, run_id) = {
            let live = self.shared.live.lock().await;
            if bytes > self.shared.limits.input_bytes
                || live
                    .state
                    .run
                    .as_ref()
                    .is_some_and(|run| bytes > run.limits.input_bytes)
            {
                return Err(reject(
                    ErrorCode::LimitExceeded,
                    "tool observation exceeds input bound",
                ));
            }
            if !live.sent_tools.contains(&observation.invocation_id) {
                return Err(reject(
                    ErrorCode::InvalidToolResult,
                    "tool observation has no dispatched invocation",
                ));
            }
            live.state
                .agents
                .values()
                .find_map(|agent| {
                    agent.turn.as_ref().and_then(|turn| {
                        turn.invocations
                            .iter()
                            .any(|call| call.dispatch.invocation_id == observation.invocation_id)
                            .then(|| (agent.agent_id.clone(), turn.run_id.clone()))
                    })
                })
                .ok_or_else(|| reject(ErrorCode::InvalidToolResult, "unknown tool invocation"))?
        };
        self.transition_scoped(
            Some(&target),
            Some(&run_id),
            "tool.status",
            |state, head, _| {
                let call = state
                    .agents
                    .get(&target)
                    .and_then(|agent| agent.turn.as_ref())
                    .and_then(|turn| {
                        turn.invocations
                            .iter()
                            .find(|call| call.dispatch.invocation_id == observation.invocation_id)
                    })
                    .ok_or_else(|| {
                        reject(ErrorCode::InvalidToolResult, "unknown tool invocation")
                    })?;
                validate_next(call, &observation, &state.operations)?;
                let turn = agent_turn(state, &target)?;
                let call = turn
                    .invocations
                    .iter_mut()
                    .find(|call| call.dispatch.invocation_id == observation.invocation_id)
                    .ok_or_else(|| {
                        reject(ErrorCode::InvalidToolResult, "unknown tool invocation")
                    })?;
                let unresolved = call
                    .result
                    .as_ref()
                    .is_none_or(|result| result.status == ToolOutcome::EffectUnknown);
                call.tool_observations
                    .insert(operation_id.into(), observation.clone());
                // A new operation can report renewed uncertainty after restoration
                // confirmed that the same invocation was still running. Exact
                // retries already returned their original receipt above.
                if unresolved && observation.status == ToolStatus::EffectUnknown {
                    turn.status = AgentStatus::RecoveryRequired;
                    active_run(state)?.status = RunStatus::RecoveryRequired;
                    state.manifest.workspace_revision = None;
                }
                let receipt = OperationReceipt {
                    operation_id: operation_id.into(),
                    request_sha256: fingerprint,
                    disposition: OperationDisposition::Accepted,
                    assigned_ids: BTreeMap::from([
                        ("invocation_id".into(), observation.invocation_id.clone()),
                        ("attempt_id".into(), observation.attempt_id.clone()),
                    ]),
                    state_revision: head.state_revision + 1,
                    error: None,
                };
                state.operations.insert(operation_id.into(), receipt);
                encode(&observation)
            },
        )
        .await?;
        self.operation(operation_id).await.ok_or_else(|| {
            reject(
                ErrorCode::CheckpointUnavailable,
                "tool observation was not committed",
            )
        })
    }
}

fn phase(status: ToolStatus) -> Option<u8> {
    match status {
        ToolStatus::NotStarted => Some(0),
        ToolStatus::WaitingApproval => Some(1),
        ToolStatus::Running => Some(2),
        ToolStatus::Stopped => Some(3),
        ToolStatus::EffectUnknown => None,
    }
}

pub(super) fn validate_identity(
    call: &Invocation,
    observation: &ToolObservation,
) -> Result<(), CoreError> {
    if call.dispatch.invocation_id != observation.invocation_id
        || call.dispatch.attempt_id != observation.attempt_id
    {
        return Err(reject(
            ErrorCode::InvalidToolResult,
            "tool observation names a different invocation or attempt",
        ));
    }
    Ok(())
}

fn validate_next(
    call: &Invocation,
    observation: &ToolObservation,
    operations: &BTreeMap<String, OperationReceipt>,
) -> Result<(), CoreError> {
    validate_identity(call, observation)?;
    if let Some(result) = &call.result
        && (phase(observation.status).is_some_and(|phase| phase < 3)
            || (observation.status == ToolStatus::EffectUnknown
                && result.status != ToolOutcome::EffectUnknown))
    {
        return Err(reject(
            ErrorCode::OperationConflict,
            "observation conflicts with committed tool outcome",
        ));
    }
    validate_phase(call, observation, operations)
}

pub(super) fn validate_phase(
    call: &Invocation,
    observation: &ToolObservation,
    operations: &BTreeMap<String, OperationReceipt>,
) -> Result<(), CoreError> {
    validate_identity(call, observation)?;
    if let Some(next) = phase(observation.status)
        && call
            .tool_observations
            .iter()
            .filter(|(operation_id, previous)| {
                // Restoration can reset an unstarted approval, but it cannot
                // erase execution evidence or a later approval observation.
                previous.status != ToolStatus::WaitingApproval
                    || !call.recovery_observation.as_ref().is_some_and(|fresh| {
                        fresh.status == ToolStatus::NotStarted
                            && operations.get(*operation_id).is_some_and(|receipt| {
                                receipt.state_revision < call.recovery_observation_revision
                            })
                    })
            })
            .map(|(_, previous)| previous)
            .chain(call.recovery_observation.iter())
            .chain(call.prior_recovery_observations.iter().filter(|previous| {
                matches!(previous.status, ToolStatus::Running | ToolStatus::Stopped)
            }))
            .any(|previous| phase(previous.status).is_some_and(|previous| previous > next))
    {
        return Err(reject(
            ErrorCode::OperationConflict,
            "tool lifecycle observation regressed",
        ));
    }
    Ok(())
}

pub(super) fn validate_restored_phase(
    call: &Invocation,
    observation: &ToolObservation,
    operations: &BTreeMap<String, OperationReceipt>,
) -> Result<(), CoreError> {
    validate_identity(call, observation)?;
    // An authenticated replacement may prove that a formerly pending approval
    // never crossed the durable start boundary. Started/stopped work and any
    // committed outcome cannot regain execution authority through this path.
    if observation.status == ToolStatus::NotStarted
        && call.result.is_none()
        && !observed(call, ToolStatus::Running)
        && !observed(call, ToolStatus::Stopped)
    {
        return Ok(());
    }
    validate_phase(call, observation, operations)
}

pub(super) fn observed(call: &Invocation, status: ToolStatus) -> bool {
    call.tool_observations
        .values()
        .chain(call.recovery_observation.iter())
        .chain(&call.prior_recovery_observations)
        .any(|observation| observation.status == status)
}

pub(super) fn activity_ids(state: &SessionSnapshot) -> BTreeSet<String> {
    state
        .agents
        .values()
        .filter_map(|agent| agent.turn.as_ref())
        .filter(|turn| {
            state
                .run
                .as_ref()
                .is_some_and(|run| run.run_id == turn.run_id)
        })
        .flat_map(|turn| &turn.invocations)
        .filter(|call| {
            call.result
                .as_ref()
                .is_none_or(|result| result.status == ToolOutcome::EffectUnknown)
                && !observed(call, ToolStatus::Stopped)
                && observed(call, ToolStatus::Running)
        })
        .map(|call| format!("tool/{}", call.dispatch.invocation_id))
        .collect()
}

pub(super) fn validate(
    call: &Invocation,
    operations: &BTreeMap<String, OperationReceipt>,
    revision: u64,
) -> Result<(), CoreError> {
    for (operation_id, observation) in &call.tool_observations {
        validate_id(operation_id)?;
        validate_identity(call, observation)?;
        let fingerprint = digest(&json!({"type":"tool.status","observation":observation}))?;
        if operations.get(operation_id).is_none_or(|receipt| {
            receipt.operation_id != *operation_id
                || receipt.request_sha256 != fingerprint
                || receipt.disposition != OperationDisposition::Accepted
                || receipt.error.is_some()
                || receipt.state_revision == 0
                || receipt.state_revision > revision
                || receipt.assigned_ids
                    != BTreeMap::from([
                        ("invocation_id".into(), observation.invocation_id.clone()),
                        ("attempt_id".into(), observation.attempt_id.clone()),
                    ])
        }) {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "tool observation differs from its acceptance receipt",
            ));
        }
    }
    if let Some(observation) = &call.recovery_observation {
        validate_identity(call, observation)?;
    }
    for observation in &call.prior_recovery_observations {
        validate_identity(call, observation)?;
    }
    if call.recovery_observation_revision > revision
        || (call.recovery_observation_revision != 0 && call.recovery_observation.is_none())
    {
        return Err(reject(
            ErrorCode::CheckpointConflict,
            "tool restoration revision is invalid",
        ));
    }
    Ok(())
}
