//! Prospective normal model-wait delivery, alongside cancellation cleanup.
//! These sizes are forecasts only; actual results still use the shared action
//! dispatcher, response capture and canonical pairing implementation.

use super::*;
use crate::core::checkpoint::serialized_bytes;
use crate::core::collaboration::{WaitState, WaitTarget};

pub(super) const VERSION: u32 = 1;

pub(super) fn validate(state: &SessionSnapshot) -> Result<(), CoreError> {
    for call in state
        .agents
        .values()
        .filter_map(|agent| agent.turn.as_ref())
        .flat_map(|turn| &turn.core_calls)
    {
        if call.wait_output_version.is_some()
            && (call.wait_output_version != Some(VERSION)
                || !matches!(call.action, Action::Wait { .. }))
        {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "invalid model wait reservation policy",
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_history(
    payload: &CheckpointPayload,
    history: &mut BTreeMap<String, Option<u32>>,
) -> Result<(), CoreError> {
    let state: SessionSnapshot =
        serde_json::from_value(payload.checkpoint.state.clone()).map_err(json_error)?;
    validate(&state)?;
    for call in state
        .agents
        .values()
        .filter_map(|agent| agent.turn.as_ref())
        .flat_map(|turn| &turn.core_calls)
    {
        if history
            .insert(call.invocation_id.clone(), call.wait_output_version)
            .is_some_and(|previous| previous != call.wait_output_version)
        {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "model wait reservation policy changed",
            ));
        }
    }
    Ok(())
}

fn add(left: u64, right: u64) -> Result<u64, CoreError> {
    left.checked_add(right)
        .ok_or_else(|| reject(ErrorCode::LimitExceeded, "model wait reservation exhausted"))
}
fn times(bytes: u64, count: u64) -> Result<u64, CoreError> {
    bytes
        .checked_mul(count)
        .ok_or_else(|| reject(ErrorCode::LimitExceeded, "model wait reservation exhausted"))
}

fn pending(state: &SessionSnapshot) -> impl Iterator<Item = (&AgentState, &Call)> {
    state
        .agents
        .values()
        .filter(|agent| {
            agent.turn.as_ref().is_some_and(|turn| {
                !turn.status.terminal() && turn.status != AgentStatus::Cancelling
            })
        })
        .flat_map(|agent| {
            agent
                .turn
                .iter()
                .flat_map(|turn| &turn.core_calls)
                .filter(|call| {
                    call.wait_output_version == Some(VERSION)
                        && !call.consumed
                        && call.result.is_none()
                        && matches!(call.action, Action::Wait { .. })
                })
                .map(move |call| (agent, call))
        })
}

fn observes(call: &Call, target: &str) -> bool {
    matches!(&call.action, Action::Wait { agent_ids, .. } if agent_ids.iter().any(|id| id == target))
}

fn wait(call: &Call) -> WaitState {
    if let Some(wait) = &call.wait {
        return wait.clone();
    }
    let targets = match &call.action {
        // Rendering uses target keys. The actual dispatcher remains responsible
        // for cycle checks, target identities and wait readiness.
        Action::Wait { agent_ids, .. } => agent_ids
            .iter()
            .map(|id| {
                (
                    id.clone(),
                    WaitTarget {
                        agent_turn_id: String::new(),
                        status: None,
                    },
                )
            })
            .collect(),
        _ => BTreeMap::new(),
    };
    WaitState {
        targets,
        deadline_ms: u64::MAX,
    }
}

/// Sources inherited by one waiter can also enter its parent conclusion,
/// runtime waits and other model waits. Visit each receiving agent once while
/// counting every separate wait result; de-duplication can only reduce size.
fn source_copies(state: &SessionSnapshot, recipient: &str) -> Result<u64, CoreError> {
    let mut queue = VecDeque::from([recipient.to_owned()]);
    let mut seen = BTreeSet::new();
    let mut copies = 0;
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let Some(agent) = state.agents.get(&id) else {
            continue;
        };
        let mail =
            agent.parent_id.is_some() && agent.turn.as_ref().is_some_and(|turn| !turn.notified);
        let runtime = state
            .waits
            .values()
            .filter(|wait| wait.result.is_none() && wait.state.targets.contains_key(&id))
            .count() as u64;
        copies = add(copies, add(1 + u64::from(mail), times(runtime, 2)?)?)?;
        for (observer, _) in pending(state).filter(|(_, call)| observes(call, &id)) {
            // Two queue views, each with native result, JSON-in-string history
            // (up to twice its JSON bytes), durable event and Responses event.
            copies = add(copies, 10)?;
            queue.push_back(observer.agent_id.clone());
        }
    }
    Ok(copies)
}

/// Additional unknown answer/source contributions before the target model's
/// receipt is applied. Known result envelopes are counted by `reserved`.
pub(super) fn future(
    state: &SessionSnapshot,
    target: &str,
    answer_bytes: u64,
    source_bytes: u64,
) -> Result<u64, CoreError> {
    let mut bytes = 0;
    for (observer, _) in pending(state).filter(|(_, call)| observes(call, target)) {
        bytes = add(bytes, times(answer_bytes, 10)?)?;
        let copies = add(10, times(source_copies(state, &observer.agent_id)?, 2)?)?;
        bytes = add(bytes, times(source_bytes, copies)?)?;
    }
    Ok(bytes)
}

/// Tool evidence and completed-but-unpaired waits can still add sources to
/// an observed agent. Retain their downstream obligation until canonical
/// pairing installs those sources, including after a definite tool receipt.
fn incoming_sources(state: &SessionSnapshot, host: &Limits) -> Result<u64, CoreError> {
    let mut bytes = 0;
    for agent in state.agents.values() {
        let Some(turn) = &agent.turn else { continue };
        for call in turn.invocations.iter().filter(|call| !call.consumed) {
            let mut source = ContextSource {
                permission_revision: call.dispatch.permission_revision,
                workspace_revision: None,
                tool_manifest_digest: call.dispatch.tool_manifest_digest.clone(),
                materials: Vec::new(),
            };
            let source_bytes = match &call.result {
                Some(result)
                    if matches!(result.status, ToolOutcome::Succeeded | ToolOutcome::Failed) =>
                {
                    source.workspace_revision = result.workspace_revision.clone();
                    if agent.context_sources.contains(&source) {
                        continue;
                    }
                    add(serialized_bytes(&source)?, 1)?
                }
                Some(result) if result.status != ToolOutcome::EffectUnknown => continue,
                _ => {
                    let limits = tool_payloads::limits(
                        call,
                        host,
                        state.run.as_ref(),
                        turn.input.limits.as_ref(),
                    )?;
                    add(add(serialized_bytes(&source)?, limits.payload_bytes)?, 1)?
                }
            };
            bytes = add(bytes, future(state, &agent.agent_id, 0, source_bytes)?)?;
        }
        for call in turn
            .core_calls
            .iter()
            .filter(|call| !call.consumed && matches!(call.action, Action::Wait { .. }))
        {
            let Some(result) = &call.result else { continue };
            if let Some(observations) = result["value"]["agents"].as_array() {
                for observation in observations {
                    let sources: Vec<ContextSource> =
                        serde_json::from_value(observation["context_sources"].clone())
                            .map_err(json_error)?;
                    for source in sources {
                        if !agent.context_sources.contains(&source) {
                            bytes = add(
                                bytes,
                                future(
                                    state,
                                    &agent.agent_id,
                                    0,
                                    add(serialized_bytes(&source)?, 1)?,
                                )?,
                            )?;
                        }
                    }
                }
            }
        }
    }
    Ok(bytes)
}

pub(super) fn reserved(state: &SessionSnapshot, host: &Limits) -> Result<u64, CoreError> {
    validate(state)?;
    let mut after = state.clone();
    for agent in after.agents.values_mut() {
        agent.queue.clear();
        if let Some(turn) = &mut agent.turn {
            turn.status = AgentStatus::RecoveryRequired;
        }
    }
    let mut bytes = incoming_sources(state, host)?;
    for (agent, call) in pending(state) {
        let wait = wait(call);
        let sources = source_copies(state, &agent.agent_id)?;
        // Sum both views to cover mixed queue-cancellation boundaries without
        // choosing a smaller body to subsidize another outcome.
        for view in [state, &after] {
            let result = json!({"ok":true,"value":collaboration::wait_result(view, &wait, 0)});
            let event = DurableEvent {
                event_seq: u64::MAX,
                kind: "collaboration.applied".into(),
                run_id: agent.turn.as_ref().map(|turn| turn.run_id.clone()),
                agent_id: Some(agent.agent_id.clone()),
                payload: json!({"source":"model","invocation_id":call.invocation_id,"operation":"wait_agent","result":result}),
            };
            bytes = add(bytes, serialized_bytes(&result)?)?;
            bytes = add(
                bytes,
                serialized_bytes(&pairing::core_message(call, &result)?)?,
            )?;
            bytes = add(bytes, serialized_bytes(&event)?)?;
            if responses::active_id(state).is_some() {
                bytes = add(
                    bytes,
                    serialized_bytes(&responses::ResponseEvent {
                        agent_name: agent.display_path.clone(),
                        event,
                    })?,
                )?;
            }
            if let Some(observations) = result["value"]["agents"].as_array() {
                for observation in observations {
                    bytes = add(
                        bytes,
                        times(serialized_bytes(&observation["context_sources"])?, sources)?,
                    )?;
                }
            }
        }
    }
    Ok(bytes)
}
