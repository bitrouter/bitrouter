//! Admission reserves a conservative cancellation/settlement projection. The
//! projection is never committed or executed: real outcomes still come only
//! from authenticated evidence. Unknown provider responses are a separate
//! admission problem; these bounds cover retained state and admitted tools.

use super::*;
use crate::core::checkpoint::{ToolStartFence, serialized_bytes};
use crate::core::protocol::{OwnershipGrant, RunActivityReconciliation, ToolStatus};

#[derive(Default)]
struct Reservation {
    extra: u64,
    event_extra: u64,
}

impl Reservation {
    fn add(&mut self, bytes: u64) -> Result<(), CoreError> {
        self.extra = sum(self.extra, bytes)?;
        Ok(())
    }
}

fn sum(left: u64, right: u64) -> Result<u64, CoreError> {
    left.checked_add(right).ok_or_else(|| {
        reject(
            ErrorCode::LimitExceeded,
            "cleanup reservation size exhausted",
        )
    })
}

fn times(bytes: u64, count: u64) -> Result<u64, CoreError> {
    bytes.checked_mul(count).ok_or_else(|| {
        reject(
            ErrorCode::LimitExceeded,
            "cleanup reservation size exhausted",
        )
    })
}

fn receipt(assigned_ids: BTreeMap<String, String>) -> OperationReceipt {
    OperationReceipt {
        operation_id: "x".repeat(128),
        request_sha256: "0".repeat(64),
        disposition: OperationDisposition::Accepted,
        assigned_ids,
        state_revision: u64::MAX,
        error: None,
    }
}

fn entry_bytes(value: &impl Serialize) -> Result<u64, CoreError> {
    // A maximum-length operation key, JSON quotes, colon and comma. Counting
    // the comma even for an empty map is conservative.
    sum(serialized_bytes(value)?, 128 + 4)
}

fn larger_reason(current: &mut Option<String>) -> Result<(), CoreError> {
    // All core-authored cleanup reasons are shorter than 128 ASCII bytes.
    // Retain longer existing reasons so no shrink can subsidize new admission.
    let reserve = Some("x".repeat(128));
    if serialized_bytes(current)? < serialized_bytes(&reserve)? {
        *current = reserve;
    }
    Ok(())
}

fn unfinished(call: &Invocation) -> bool {
    call.result
        .as_ref()
        .is_none_or(|result| result.status == ToolOutcome::EffectUnknown)
}

fn reserve_tool(
    call: &mut Invocation,
    host: &Limits,
    run: Option<&RootRun>,
    input_limits: Option<&Limits>,
    context_copies: u64,
    reserve: &mut Reservation,
) -> Result<(), CoreError> {
    // Restoration may widen numeric fields even when the tool itself is never
    // dispatched again. None of these placeholders leaves this projection.
    call.dispatch.execution_epoch = u64::MAX;
    call.dispatch.authorizing_event_seq = u64::MAX;
    call.recovery_observation_revision = u64::MAX;
    if !unfinished(call) {
        return Ok(());
    }
    let limits = tool_payloads::limits(call, host, run, input_limits)?;
    let result_slots = if call.result.is_none() { 2 } else { 1 };
    let operation = receipt(BTreeMap::from([
        ("invocation_id".into(), call.dispatch.invocation_id.clone()),
        ("attempt_id".into(), call.dispatch.attempt_id.clone()),
    ]));
    // One uncertain outcome can be retained alongside the eventual definite
    // outcome. Each bounded body can add at most the same number of bytes in
    // distinct checkpoint artifact references, plus its operation receipt.
    reserve.add(times(
        sum(times(limits.payload_bytes, 2)?, entry_bytes(&operation)?)?,
        result_slots,
    )?)?;

    let stopped = tool_status::observed(call, ToolStatus::Stopped);
    let running = tool_status::observed(call, ToolStatus::Running);
    let uncertain = tool_status::observed(call, ToolStatus::EffectUnknown) || call.result.is_some();
    let lifecycle = u64::from(!stopped)
        + u64::from(!stopped && !running && call.result.is_none())
        + u64::from(!uncertain);
    // Reserve first running/stopped/uncertain observations. Optional approval,
    // repeated status and repeated uncertainty records still need fresh space.
    let observation = sum(
        sum(times(limits.payload_bytes, 2)?, entry_bytes(&operation)?)?,
        128 + 4,
    )?;
    reserve.add(times(observation, lifecycle)?)?;
    reserve.event_extra = sum(reserve.event_extra, sum(limits.payload_bytes, 1)?)?;

    // Future output enters canonical history once. A future workspace revision
    // also enters a source record, the child's terminal mailbox, and any open
    // runtime waits observing that source. Account for separate source records
    // even if the empty placeholders happen to deduplicate in the projection.
    if !call.dispatch.verification {
        reserve.add(sum(
            limits.payload_bytes,
            serialized_bytes(&"NotExecuted: ")?,
        )?)?;
    }
    let source = ContextSource {
        permission_revision: call.dispatch.permission_revision,
        workspace_revision: None,
        tool_manifest_digest: call.dispatch.tool_manifest_digest.clone(),
        materials: Vec::new(),
    };
    reserve.add(times(
        sum(sum(limits.payload_bytes, serialized_bytes(&source)?)?, 1)?,
        context_copies,
    )?)?;
    let empty = ToolResult {
        invocation_id: call.dispatch.invocation_id.clone(),
        attempt_id: call.dispatch.attempt_id.clone(),
        status: ToolOutcome::EffectUnknown,
        output: String::new(),
        evidence: Vec::new(),
        workspace_revision: None,
    };
    if call.prior_uncertain_result.is_none() {
        call.prior_uncertain_result = Some(call.result.clone().unwrap_or_else(|| empty.clone()));
    }
    call.result = Some(ToolResult {
        status: ToolOutcome::Failed,
        workspace_revision: Some(String::new()),
        ..empty
    });
    Ok(())
}

pub(super) fn check(
    state: &SessionSnapshot,
    proposed: &CheckpointPayload,
    host: &Limits,
    grant: &OwnershipGrant,
) -> Result<(), CoreError> {
    let mut limits = host.clone();
    if let Some(run) = &state.run {
        limits.checkpoint_bytes = limits.checkpoint_bytes.min(run.limits.checkpoint_bytes);
        limits.unacknowledged_bytes = limits
            .unacknowledged_bytes
            .min(run.limits.unacknowledged_bytes);
    }
    CheckpointBatch::check_projected_size(
        &proposed.identity,
        serialized_bytes(proposed)?,
        &limits,
    )?;
    let mut projected = state.clone();
    let mut reserve = Reservation::default();
    let mut event_payloads = vec![json!({"reason":"x".repeat(128)})];
    let original_run = state.run.as_ref();
    let open = original_run.is_some_and(|run| !run.status.terminal());
    if open && let Some(run) = &mut projected.run {
        if run.cancellation.is_none() {
            reserve.add(entry_bytes(&receipt(BTreeMap::from([(
                "run_id".into(),
                run.run_id.clone(),
            )])))?)?;
        }
        run.cancellation = Some("x".repeat(128));
        run.status = RunStatus::RecoveryRequired;
        run.active_ms = u64::MAX;
        if run.resource_error.is_none() {
            run.resource_constraint = Some(ResourceConstraint::CheckpointCapacity);
            run.resource_error = Some(CoreError {
                code: ErrorCode::LimitExceeded,
                message: "x".repeat(128),
                commit_status: CommitStatus::Committed,
            });
        }
        larger_reason(&mut run.terminal_reason)?;
    }
    for agent in projected.agents.values_mut() {
        let Some(turn) = &mut agent.turn else {
            continue;
        };
        let terminal = turn.status.terminal();
        let status = turn.status;
        let pending = turn.invocations.iter().any(|call| !call.consumed)
            || turn.core_calls.iter().any(|call| !call.consumed);
        let mut copies = 1 + u64::from(agent.parent_id.is_some() && !turn.notified);
        if turn.final_answer.is_some() {
            copies = sum(
                copies,
                state
                    .waits
                    .values()
                    .filter(|wait| {
                        wait.result.is_none() && wait.state.targets.contains_key(&agent.agent_id)
                    })
                    .count() as u64,
            )?;
        }
        for call in &mut turn.invocations {
            reserve_tool(
                call,
                host,
                original_run,
                turn.input.limits.as_ref(),
                copies,
                &mut reserve,
            )?;
        }
        for call in &mut turn.core_calls {
            if call.result.is_none() {
                call.result = Some(json!({"ok":false,"reason":"agent interrupted"}));
            }
        }
        let old_reason = turn.terminal_reason.clone();
        let consumed = turn
            .invocations
            .iter()
            .map(|call| call.consumed)
            .collect::<Vec<_>>();
        let core_consumed = turn
            .core_calls
            .iter()
            .map(|call| call.consumed)
            .collect::<Vec<_>>();
        turn.status = AgentStatus::Cancelling;
        if pending {
            // Projection uses the very same pairing implementation as cleanup.
            // Numeric padding below accounts for the real increment separately.
            agent.context_revision = 0;
            pairing::consume(agent)?;
        }
        agent.context_revision = u64::MAX;
        let turn = agent
            .turn
            .as_mut()
            .ok_or_else(|| reject(ErrorCode::CheckpointConflict, "projection lost agent turn"))?;
        if serialized_bytes(&old_reason)? > serialized_bytes(&turn.terminal_reason)? {
            turn.terminal_reason = old_reason;
        }
        if !terminal {
            larger_reason(&mut turn.terminal_reason)?;
        }
        turn.status = if terminal {
            status
        } else {
            AgentStatus::RecoveryRequired
        };
        // Do not use shrinking booleans to pay for unrelated state growth.
        for (call, value) in turn.invocations.iter_mut().zip(consumed) {
            call.consumed = value;
        }
        for (call, value) in turn.core_calls.iter_mut().zip(core_consumed) {
            call.consumed = value;
        }
        event_payloads.push(
            json!({"status":turn.status,"answer":turn.final_answer,"reason":turn.terminal_reason}),
        );
    }
    steering::cancel_inactive(&mut projected, u64::MAX);

    // Cancellation archives child conclusions at their existing parents, even
    // when ordinary mailbox admission is full. Do not remove queued work or
    // existing mailbox records from the byte projection.
    let deliveries = projected.agents.values().filter_map(|agent| {
        let turn = agent.turn.as_ref()?;
        (agent.parent_id.is_some() && !turn.notified).then(|| (turn.assigned_by.clone(), Mail {
            message_id: "x".repeat(128), sender_id: agent.agent_id.clone(), sender_turn_id: turn.agent_turn_id.clone(), kind: "agent_result".into(),
            content: json!({"agent_id":agent.agent_id,"agent_turn_id":turn.agent_turn_id,"status":turn.status,"answer":turn.final_answer,"reason":turn.terminal_reason,"provenance":"agent_conclusion"}),
            context_sources: agent.context_sources.clone(), consumed: false,
        }))
    }).collect::<Vec<_>>();
    for (parent, mail) in deliveries {
        agent_mut(&mut projected, &parent)?.mailbox.push(mail);
    }
    let mut view = projected.clone();
    for agent in view.agents.values_mut() {
        agent.queue.clear();
    }
    for (id, wait) in &state.waits {
        if wait.result.is_some() {
            continue;
        }
        // A wait can settle before or after queued assignments are cancelled.
        // Their sum also covers mixed per-agent views of that boundary.
        let before = collaboration::wait_result(state, &wait.state, 0);
        let after = collaboration::wait_result(&view, &wait.state, 0);
        reserve.add(serialized_bytes(&before)?)?;
        if let Some(wait) = projected.waits.get_mut(id) {
            wait.result = Some(after);
        }
    }
    let answer = projected
        .root_turn()
        .and_then(|turn| turn.final_answer.clone());
    if open && let Some(run) = &mut projected.run {
        if serialized_bytes(&answer)? > serialized_bytes(&run.final_answer)? {
            run.final_answer = answer;
        }
        event_payloads.push(json!({"status":run.status,"answer":run.final_answer,"reason":run.terminal_reason,"error":run.resource_error}));
    }
    // Finishing intent-only cost rows changes "intent_recorded" to
    // "outcome_recorded", one extra ASCII byte. Reports themselves must pass
    // ordinary admission and are not manufactured by this projection.
    reserve.add(
        state
            .cost_work
            .values()
            .flat_map(|run| run.work.values())
            .filter(|work| {
                work.state == crate::core::accounting::work::CostWorkState::IntentRecorded
            })
            .count() as u64,
    )?;

    if !state.releases.values().any(|record| &record.grant == grant) {
        let operation = receipt(BTreeMap::from([
            ("session_id".into(), grant.session_id.clone()),
            ("core_instance_id".into(), grant.core_instance_id.clone()),
            ("execution_epoch".into(), grant.execution_epoch.to_string()),
        ]));
        reserve.add(entry_bytes(&operation)?)?;
        reserve.add(entry_bytes(&release::ReleaseRecord {
            operation_id: operation.operation_id.clone(),
            grant: grant.clone(),
            state_revision: u64::MAX,
        })?)?;
        event_payloads.push(encode(&operation)?);
    }
    let identity = BatchIdentity {
        batch_id: "x".repeat(128),
        session_id: state.session_id.clone(),
        core_instance_id: "x".repeat(128),
        execution_epoch: u64::MAX,
    };
    let head = DurableHead {
        execution_epoch: u64::MAX,
        state_revision: u64::MAX,
        event_seq: u64::MAX,
        batch_id: Some("x".repeat(128)),
        payload_sha256: Some("0".repeat(64)),
    };
    let handoff = original_run.map(|run| RunActivityReconciliation {
        run_id: run.run_id.clone(),
        durable_head: head,
        active_ms: u64::MAX,
    });
    let unresolved = state
        .agents
        .values()
        .filter_map(|agent| agent.turn.as_ref())
        .flat_map(|turn| &turn.invocations)
        .filter(|call| unfinished(call))
        .collect::<Vec<_>>();
    event_payloads.push(json!({"previous_owner_stopped":false,"workspace_changed":false,"active_time":handoff,"tool_observations":[],"interrupted_steps":state.agents.values().filter_map(|agent| agent.turn.as_ref()).flat_map(|turn| &turn.steps).map(|step| &step.step_id).collect::<Vec<_>>()}));
    event_payloads.push(json!({"invocation_ids":unresolved.iter().map(|call| &call.dispatch.invocation_id).collect::<Vec<_>>(),"reason":"x".repeat(128)}));
    let event_bytes = event_payloads
        .iter()
        .map(serialized_bytes)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .unwrap_or(0);
    let mut payload = proposed.clone();
    payload.identity = identity;
    payload.base_state_revision = u64::MAX;
    payload.base_event_seq = u64::MAX;
    payload.checkpoint.state_revision = u64::MAX;
    // Repeated recovery evidence occupies an immutable archive, not another
    // online history entry. Reserve the root even before the first recovery;
    // changing its content then replaces a fixed-size reference.
    let mut prepared = archive::prepare(&projected, true, host)?;
    archive::pad_reference(&mut prepared.state);
    payload.checkpoint.state = encode(&prepared.state)?;
    payload.checkpoint.artifact_refs = recovery::artifacts(&prepared.state)?
        .into_values()
        .collect();
    payload.tool_start_fences = unresolved
        .iter()
        .map(|call| ToolStartFence {
            invocation_id: call.dispatch.invocation_id.clone(),
            attempt_id: call.dispatch.attempt_id.clone(),
        })
        .collect();
    payload.events = vec![DurableEvent {
        event_seq: u64::MAX,
        kind: "x".repeat(128),
        run_id: original_run.map(|run| run.run_id.clone()),
        agent_id: Some("x".repeat(128)),
        payload: Value::Null,
    }];
    let bytes = serialized_bytes(&payload)?.saturating_sub(4);
    let bytes = sum(
        sum(sum(bytes, event_bytes)?, reserve.event_extra)?,
        reserve.extra,
    )?;
    CheckpointBatch::check_projected_size(&payload.identity, bytes, &limits)
}
