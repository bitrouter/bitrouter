//! Admission reserves a conservative cancellation/settlement projection. The
//! projection is never committed or executed: real outcomes still come only
//! from authenticated evidence. Canonical model results have frozen allowances;
//! complete attempt reports have their own limits. Physical allocations retain
//! separate admission obligations.

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
    // An uncertain result still permits a first uncertain lifecycle report.
    let uncertain = tool_status::observed(call, ToolStatus::EffectUnknown);
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
    reserve.add(model_output::reserved(state)?)?;
    reserve.add(auxiliary_output::reserved(state)?)?;
    reserve.add(context_decisions::reserved(state)?)?;
    reserve.add(wait_output::reserved(state, host)?)?;
    let mut event_payloads = vec![json!({"reason":"x".repeat(128)})];
    if let Some(event) = responses::reserve_terminal(&mut projected)? {
        event_payloads.push(event);
    }
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
    let native_values = state
        .manifest
        .required_features
        .iter()
        .any(|feature| feature == super::super::context_router::NATIVE_TOOLS);
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
            pairing::consume(agent, native_values)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal(
        state: &SessionSnapshot,
        grant: &OwnershipGrant,
    ) -> Result<CheckpointPayload, CoreError> {
        Ok(CheckpointPayload {
            identity: BatchIdentity {
                batch_id: "batch".into(),
                session_id: state.session_id.clone(),
                core_instance_id: grant.core_instance_id.clone(),
                execution_epoch: 1,
            },
            base_state_revision: 1,
            base_event_seq: 1,
            events: Vec::new(),
            tool_start_fences: Vec::new(),
            checkpoint: Checkpoint {
                schema_version: VERSION,
                state_revision: 2,
                artifact_refs: Vec::new(),
                state: encode(state)?,
            },
        })
    }

    fn fixture() -> Result<SessionSnapshot, Box<dyn std::error::Error>> {
        let input = json!({"text":"task", "model":"model", "acceptance_criteria":[], "required_materials":[]});
        let state: SessionSnapshot = serde_json::from_value(json!({
            "session_id":"session", "agent_id":"root",
            "manifest":{"tools":[],"tool_manifest_digest":HarnessManifest::digest(&[])?,
                "workspace_id":"workspace","permission_revision":1,"max_tool_output_bytes":1024,
                "artifact_quota_bytes":1048576,"max_artifact_chunk_bytes":1024,"required_features":[]},
            "agents":{"root":{"agent_id":"root","display_path":"/root","depth":0,
                "context_revision":0,"history":[],"required_instructions":[],"queue":[],
                "mailbox":[],"context_sources":[],"last_scheduled":0,
                "turn":{"run_id":"run","agent_turn_id":"root_turn","assigned_by":"root",
                    "input":input,"status":"runnable","steps":[],"invocations":[],"core_calls":[],"notified":false}}},
            "run":{"run_id":"run","agent_turn_id":"root_turn","input":input,
                "limits":Limits::default(),"status":"running","model_attempts":0,"active_ms":0},
            "operations":{},"waits":{},"signals":SignalState::default(),"allocations":{}
        }))?;
        Ok(state)
    }

    fn exact_host(
        state: &SessionSnapshot,
        grant: &OwnershipGrant,
        limits: &Limits,
    ) -> Result<Limits, CoreError> {
        let proposed = proposal(state, grant)?;
        let mut host = limits.clone();
        let (mut low, mut high) = (0, host.checkpoint_bytes);
        check(state, &proposed, &host, grant)?;
        while low < high {
            host.checkpoint_bytes = low + (high - low) / 2;
            match check(state, &proposed, &host, grant) {
                Ok(()) => high = host.checkpoint_bytes,
                Err(error) if error.code == ErrorCode::LimitExceeded => {
                    low = host.checkpoint_bytes + 1;
                }
                Err(error) => return Err(error),
            }
        }
        host.checkpoint_bytes = high;
        Ok(host)
    }

    #[test]
    fn saturated_auxiliary_reports_keep_admitted_capacity() -> Result<(), Box<dyn std::error::Error>>
    {
        use bitrouter_sdk::language_model::native::NativeRouteConstraints;
        for kind in ["preparation", "count", "validation", "provider"] {
            let mut state = fixture()?;
            let limits = state.run.as_ref().ok_or("run")?.limits.clone();
            let plan = json!({
                "request_id":"request", "original_model":"model", "effective_model":"model", "effort_source":"caller",
                "prompt":{"model":"model", "messages":[], "stream":false},
                "routes":[{"provider":"provider","model":"model","protocol":"chat_completions","constraints":NativeRouteConstraints::default()}]
            });
            let prompt: Prompt = serde_json::from_value(plan["prompt"].clone())?;
            let context = ContextManifest::capture(&state, "root", &prompt)?;
            let mut step = json!({
                "step_id":"step","decision_id":"decision","context_revision":0,"signal_revision":0,
                "manifest":state.manifest,"materials":[],"context":context,"input_state_revision":1,"input_history":[],
                "attempts":[],"settled":false,"auxiliary_output_version":1
            });
            let report = match kind {
                "preparation" => {
                    let work =
                        json!({"request_id":"request","kind":"pre_request_hook","work_index":0});
                    step["preparation_work"] = json!([{"work":work}]);
                    json!({"work":work,"elapsed_ms":u64::MAX,"error_code":""})
                }
                "count" => {
                    step["count_plan"] = plan.clone();
                    step["input_counts"] = json!([{"route_index":0}]);
                    json!({"request_id":"request","route_index":0,"elapsed_ms":u64::MAX,"outcome":{"status":"unavailable","reason":""}})
                }
                "validation" => {
                    step["context_validation"] = json!({"request_id":"request","applied":false});
                    json!({"request_id":"request","allowed":false,"elapsed_ms":u64::MAX,"work_elapsed_ms":u64::MAX,"error_code":""})
                }
                "provider" => {
                    let work = json!({"request_id":"request","attempt_index":0,"work_index":0,"kind":"authentication"});
                    step["plan"] = plan.clone();
                    step["attempts"] =
                        json!([{"attempt_id":"attempt","index":0,"provider_work":[{"work":work}]}]);
                    json!({"work":work,"elapsed_ms":u64::MAX,"http_status":null,"error_code":""})
                }
                _ => return Err("unknown auxiliary fixture".into()),
            };
            agent_turn(&mut state, "root")?
                .steps
                .push(serde_json::from_value(step)?);
            crate::core::accounting::work::synchronize(&mut state)?;
            let grant = OwnershipGrant {
                session_id: "session".into(),
                harness_id: "harness".into(),
                core_instance_id: "core".into(),
                execution_epoch: 1,
            };
            let host = exact_host(&state, &grant, &limits)?;
            let mut report = report;
            let bound = serialized_bytes(&"request")? + 4096;
            let padding = usize::try_from(bound - serialized_bytes(&report)?)?;
            if kind == "count" {
                report["outcome"]["reason"] = json!("x".repeat(padding));
            } else {
                report["error_code"] = json!("x".repeat(padding));
            }
            assert_eq!(serialized_bytes(&report)?, bound);
            let step = &mut agent_turn(&mut state, "root")?.steps[0];
            match kind {
                "preparation" => {
                    step.preparation_work[0].report = Some(serde_json::from_value(report.clone())?)
                }
                "count" => {
                    step.input_counts[0].report = Some(serde_json::from_value(report.clone())?)
                }
                "validation" => {
                    step.context_validation.as_mut().ok_or("validation")?.report =
                        Some(serde_json::from_value(report.clone())?)
                }
                "provider" => {
                    step.attempts[0].provider_work[0].report =
                        Some(serde_json::from_value(report.clone())?)
                }
                _ => return Err("unknown auxiliary fixture".into()),
            }
            crate::core::accounting::work::synchronize(&mut state)?;
            let mut received = proposal(&state, &grant)?;
            received.events.push(DurableEvent {
                event_seq: 2,
                kind: format!("{kind}.outcome"),
                run_id: Some("run".into()),
                agent_id: Some("root".into()),
                payload: report,
            });
            check(&state, &received, &host, &grant)?;
            // Spend the released report reservation before terminal delivery.
            // A maximal readable reason and an oversized diagnostic must both
            // remain deliverable without relying on the earlier free space.
            let host = exact_host(&state, &grant, &limits)?;
            for oversized in [false, true] {
                let mut failed = state.clone();
                let turn = agent_turn(&mut failed, "root")?;
                let reason = if oversized {
                    "failure\0\"".repeat(100_000)
                } else {
                    "x".repeat(1022)
                };
                let reason = auxiliary_output::failure(turn.steps.last_mut(), &reason)?;
                turn.steps[0].settled = true;
                turn.status = AgentStatus::Failed;
                turn.terminal_reason = Some(reason.clone());
                let mut outcome = proposal(&failed, &grant)?;
                outcome.events.push(DurableEvent {
                    event_seq: 2,
                    kind: "agent.failed".into(),
                    run_id: Some("run".into()),
                    agent_id: Some("root".into()),
                    payload: json!({"reason":reason}),
                });
                check(&failed, &outcome, &host, &grant)?;
            }
        }
        Ok(())
    }

    #[test]
    fn saturated_complete_report_and_error_handoffs_keep_frozen_capacity()
    -> Result<(), Box<dyn std::error::Error>> {
        use bitrouter_sdk::language_model::native::{NativePlan, NativeRouteConstraints};
        use bitrouter_sdk::language_model::native_accounting::NativeTokenCost;
        for failed in [false, true] {
            let mut state = fixture()?;
            let mut child = state.agents.get("root").ok_or("root")?.clone();
            child.agent_id = "child".into();
            child.parent_id = Some("root".into());
            child.display_path = "/root/child".into();
            child.depth = 1;
            let turn = child.turn.as_mut().ok_or("turn")?;
            turn.agent_turn_id = "child-turn".into();
            turn.status = AgentStatus::ModelRunning;
            state.agents.insert("child".into(), child);
            let limits = Limits {
                checkpoint_bytes: 1024 * 1024,
                ..Limits::default()
            };
            state.run.as_mut().ok_or("run")?.limits = limits.clone();
            let plan: NativePlan = serde_json::from_value(json!({
                "request_id":"request", "original_model":"model", "effective_model":"model", "effort_source":"caller",
                "prompt":{"model":"model", "messages":[], "stream":false},
                "routes":[{"provider":"provider","model":"model","protocol":"chat_completions","constraints":NativeRouteConstraints::default()}]
            }))?;
            let canonical = model_output::allowance(&limits)?;
            let bound =
                model_output::report_allowance(canonical, &plan.request_id, &plan.routes[0])?;
            let mut report: NativeAttemptReport = serde_json::from_value(json!({
                "request_id":"request", "attempt_index":0, "route":plan.routes[0], "elapsed_ms":u64::MAX,
                "error":if failed { Some("") } else { None },
                "actual_provider":if failed { None } else { Some("") },
                "actual_model":if failed { None } else { Some("actual-model") },
                "result":if failed { Value::Null } else { json!({"content":[],"finish_reason":"stop","provider_metadata":{}}) }
            }))?;
            // Both serving identity and cost metadata can independently occupy
            // space outside the canonical result. Retain their full values.
            if !failed {
                report.token_cost = NativeTokenCost::unknown("\0\"".repeat(512));
            }
            let padding = usize::try_from(bound - serialized_bytes(&report)?)?;
            if failed {
                report.error = Some("x".repeat(padding));
            } else {
                report.actual_provider = Some("x".repeat(padding));
            }
            assert_eq!(serialized_bytes(&report)?, bound);
            let context = ContextManifest::capture(&state, "child", &plan.prompt)?;
            let step: ModelStep = serde_json::from_value(json!({
                "step_id":"step","decision_id":"decision","context_revision":0,"signal_revision":0,
                "manifest":state.manifest,"materials":[],"context":context,"input_state_revision":1,"input_history":[],
                "plan":plan,"attempts":[{"attempt_id":"attempt","index":0,"canonical_output_bytes":canonical,
                    "canonical_output_version":3,"attempt_report_bytes":bound}],"settled":false
            }))?;
            agent_turn(&mut state, "child")?.steps.push(step);
            crate::core::accounting::work::synchronize(&mut state)?;
            let grant = OwnershipGrant {
                session_id: "session".into(),
                harness_id: "harness".into(),
                core_instance_id: "core".into(),
                execution_epoch: 1,
            };
            let host = exact_host(&state, &grant, &limits)?;
            let receipt = ExecutionReceipt::capture("decision", "attempt", &plan, report.clone());
            agent_turn(&mut state, "child")?.steps[0].attempts[0].receipt = Some(receipt.clone());
            crate::core::accounting::work::synchronize(&mut state)?;
            let mut received = proposal(&state, &grant)?;
            received.events.push(DurableEvent {
                event_seq: 2,
                kind: "model.attempt.outcome".into(),
                run_id: Some("run".into()),
                agent_id: Some("child".into()),
                payload: encode(&receipt)?,
            });
            check(&state, &received, &host, &grant)?;
            assert!(report.report_rejection.is_none());
            if failed {
                // Spend newly released headroom after recording the report;
                // the known error must remain reserved until application.
                let host = exact_host(&state, &grant, &limits)?;
                let turn = agent_turn(&mut state, "child")?;
                turn.steps[0].settled = true;
                turn.terminal_reason = report.error.clone();
                turn.status = AgentStatus::Failed;
                let mut finished = proposal(&state, &grant)?;
                finished.events.push(DurableEvent {
                    event_seq: 3,
                    kind: "agent.failed".into(),
                    run_id: Some("run".into()),
                    agent_id: Some("child".into()),
                    payload: json!({"reason":report.error}),
                });
                check(&state, &finished, &host, &grant)?;
            }
        }
        Ok(())
    }

    #[test]
    fn saturated_managed_checkpoint_keeps_room_for_child_delivery()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut state = fixture()?;
        let mut child = state.agents.get("root").ok_or("root")?.clone();
        child.agent_id = "child".into();
        child.parent_id = Some("root".into());
        child.display_path = format!("/root/{}", "child".repeat(20));
        child.depth = 1;
        let turn = child.turn.as_mut().ok_or("turn")?;
        turn.agent_turn_id = "child_turn".into();
        turn.status = AgentStatus::Completed;
        turn.final_answer = Some("child conclusion".into());
        state.agents.insert("child".into(), child);
        responses::begin(&mut state, "input", "run", None, 1)?;
        let grant = OwnershipGrant {
            session_id: "session".into(),
            harness_id: "harness".into(),
            core_instance_id: "core".into(),
            execution_epoch: 1,
        };
        let before = proposal(&state, &grant)?;
        let mut limits = Limits::default();
        let (mut low, mut high) = (0, limits.checkpoint_bytes);
        // Find the exact admission boundary instead of relying on a fixture's
        // incidental spare bytes to hide growth during an essential transition.
        while low < high {
            let middle = low + (high - low) / 2;
            limits.checkpoint_bytes = middle;
            match check(&state, &before, &limits, &grant) {
                Ok(()) => high = middle,
                Err(error) if error.code == ErrorCode::LimitExceeded => low = middle + 1,
                Err(error) => return Err(error.into()),
            }
        }
        limits.checkpoint_bytes = high;
        check(&state, &before, &limits, &grant)?;
        collaboration::enqueue_mail(
            &mut state,
            "child",
            "root",
            "agent_result",
            json!({"agent_id":"child","agent_turn_id":"child_turn","status":"completed",
                "answer":"child conclusion","reason":null,"provenance":"agent_conclusion"}),
        )?;
        agent_turn(&mut state, "child")?.notified = true;
        responses::capture(
            &mut state,
            &DurableEvent {
                event_seq: 2,
                kind: "agent.result.delivered".into(),
                run_id: Some("run".into()),
                agent_id: Some("child".into()),
                payload: json!({"parent_id":"root"}),
            },
        )?;
        check(&state, &proposal(&state, &grant)?, &limits, &grant)?;
        Ok(())
    }

    #[test]
    fn saturated_model_delivery_covers_wait_answers_and_retained_sources()
    -> Result<(), Box<dyn std::error::Error>> {
        model_delivery_boundary(false)
    }

    #[test]
    fn saturated_model_wait_delivery_preserves_results_history_and_sources()
    -> Result<(), Box<dyn std::error::Error>> {
        model_delivery_boundary(true)
    }

    fn model_delivery_boundary(model_waits: bool) -> Result<(), Box<dyn std::error::Error>> {
        let mut state = fixture()?;
        let root = state.agents.get("root").ok_or("root")?.clone();
        let mut child = root.clone();
        child.agent_id = "child".into();
        child.parent_id = Some("root".into());
        child.display_path = "/root/child".into();
        child.depth = 1;
        let turn = child.turn.as_mut().ok_or("turn")?;
        turn.agent_turn_id = "child_turn".into();
        turn.status = AgentStatus::ModelRunning;
        for index in 0..4 {
            child.context_sources.push(ContextSource {
                permission_revision: index,
                workspace_revision: Some("retained-source".repeat(512)),
                tool_manifest_digest: state.manifest.tool_manifest_digest.clone(),
                materials: Vec::new(),
            });
        }
        state.agents.insert("child".into(), child);
        let limits = Limits {
            checkpoint_bytes: 2 * 1024 * 1024,
            active_models: if model_waits {
                16
            } else {
                Limits::default().active_models
            },
            ..Limits::default()
        };
        state.run.as_mut().ok_or("run")?.limits = limits.clone();
        responses::begin(&mut state, "input", "run", None, 1)?;
        for index in 0..4 {
            let Applied::Waiting(wait) = collaboration::apply(
                &mut state,
                "root",
                &Action::Wait {
                    agent_ids: vec!["child".into()],
                    timeout_ms: 600_000,
                },
                0,
                1,
            )?
            else {
                return Err("expected pending wait".into());
            };
            state.waits.insert(
                format!("wait-{index}"),
                RuntimeWait {
                    actor_id: "root".into(),
                    state: wait,
                    result: None,
                },
            );
        }
        if model_waits {
            let wait = state
                .waits
                .get("wait-0")
                .ok_or("runtime wait")?
                .state
                .clone();
            let root = agent_mut(&mut state, "root")?;
            let turn = root.turn.as_mut().ok_or("root turn")?;
            turn.status = AgentStatus::WaitingMessage;
            let mut content = Vec::new();
            for index in 0..2 {
                let id = format!("model-wait-{index}");
                let action = Action::Wait {
                    agent_ids: vec!["child".into()],
                    timeout_ms: 600_000,
                };
                content.push(Content::ToolCall {
                    id: id.clone(),
                    name: "wait_agent".into(),
                    arguments: json!({"agent_ids":["child"],"timeout_ms":600_000}).to_string(),
                    provider_executed: false,
                    dynamic: false,
                    provider_metadata: Default::default(),
                });
                turn.core_calls.push(Call {
                    invocation_id: id.clone(),
                    public_call_id: id.clone(),
                    provider_call_id: id,
                    step_id: "root-step".into(),
                    action,
                    wait_output_version: Some(wait_output::VERSION),
                    wait: Some(wait.clone()),
                    result: None,
                    consumed: false,
                });
            }
            root.history.push(Message {
                role: Role::Assistant,
                content,
            });
        }
        let prompt: Prompt =
            serde_json::from_value(json!({"model":"model", "messages":[], "stream":false}))?;
        let context = ContextManifest::capture(&state, "child", &prompt)?;
        // Retain the version-2 arithmetic fixture; live tests use the current policy.
        let bound = limits.checkpoint_bytes / (8 * (u64::from(limits.active_models) + 1));
        let mut result: bitrouter_sdk::language_model::types::GenerateResult =
            serde_json::from_value(json!({
                "content":[],"finish_reason":"stop","provider_metadata":{}
            }))?;
        let empty = Message::text(Role::Assistant, "");
        result.content = empty.content;
        let padding = bound
            .checked_sub(serialized_bytes(&result)?)
            .ok_or("result envelope")?;
        let message = Message::text(Role::Assistant, "x".repeat(usize::try_from(padding)?));
        result.content = message.content.clone();
        assert_eq!(serialized_bytes(&result)?, bound);
        let receipt = json!({"decision_id":"decision","attempt_id":"attempt","cost_source":"unknown","cache_observation_source":"unknown",
            "report":{"request_id":"request","attempt_index":0,"elapsed_ms":0,"result":result,
                "route":{"provider":"provider","model":"model","protocol":"chat_completions",
                    "constraints":bitrouter_sdk::language_model::native::NativeRouteConstraints::default()}}});
        let step: ModelStep = serde_json::from_value(json!({
            "step_id":"step", "decision_id":"decision", "context_revision":0,"signal_revision":0,
            "manifest":state.manifest,"materials":[],"context":context,"input_state_revision":1,"input_history":[],
            "attempts":[{"attempt_id":"attempt","index":0,"receipt":receipt,"canonical_output_bytes":bound,"canonical_output_version":2}],"settled":false
        }))?;
        agent_turn(&mut state, "child")?.steps.push(step);
        let grant = OwnershipGrant {
            session_id: "session".into(),
            harness_id: "harness".into(),
            core_instance_id: "core".into(),
            execution_epoch: 1,
        };
        let before = proposal(&state, &grant)?;
        // Search the exact headroom needed by the pending-output projection.
        // Root policy remains frozen; only the independent host admission bound
        // is varied, avoiding incidental fixture slack in this arithmetic test.
        let mut host = limits.clone();
        let (mut low, mut high) = (0, host.checkpoint_bytes);
        check(&state, &before, &host, &grant)?;
        while low < high {
            let middle = low + (high - low) / 2;
            host.checkpoint_bytes = middle;
            match check(&state, &before, &host, &grant) {
                Ok(()) => high = middle,
                Err(error) if error.code == ErrorCode::LimitExceeded => low = middle + 1,
                Err(error) => return Err(error.into()),
            }
        }
        host.checkpoint_bytes = high;
        check(&state, &before, &host, &grant)?;
        let source = ContextSource::capture(&context);
        let child = agent_mut(&mut state, "child")?;
        child.history.push(message);
        child.context_sources.push(source);
        child.context_revision += 1;
        let turn = child.turn.as_mut().ok_or("turn")?;
        turn.steps[0].settled = true;
        turn.status = AgentStatus::Runnable;
        turn.final_answer = Some("x".repeat(usize::try_from(padding)?));
        responses::capture(
            &mut state,
            &DurableEvent {
                event_seq: 2,
                kind: "model.output.applied".into(),
                run_id: Some("run".into()),
                agent_id: Some("child".into()),
                payload: json!({"step_id":"step","request_id":"request"}),
            },
        )?;
        check(&state, &proposal(&state, &grant)?, &host, &grant)?;
        for wait in state.waits.values() {
            let completed = collaboration::wait_result(&state, &wait.state, 0);
            assert_eq!(
                completed["agents"][0]["final_answer"],
                json!(turn_answer(&state)?)
            );
            assert_eq!(
                completed["agents"][0]["context_sources"]
                    .as_array()
                    .ok_or("sources")?
                    .len(),
                5
            );
        }
        if model_waits {
            let mut legacy = state.clone();
            for call in &mut agent_turn(&mut legacy, "root")?.core_calls {
                call.wait_output_version = None;
            }
            let legacy_host = saturated_host(&legacy, &grant, &host)?;
            assert_eq!(wait_output::reserved(&legacy, &legacy_host)?, 0);
            assert_eq!(
                check(&state, &proposal(&state, &grant)?, &legacy_host, &grant)
                    .err()
                    .map(|error| error.code),
                Some(ErrorCode::LimitExceeded)
            );
            host = saturated_host(&state, &grant, &host)?;
            for index in 0..2 {
                let wait = state.agents["root"].turn.as_ref().ok_or("root")?.core_calls[index]
                    .wait
                    .clone()
                    .ok_or("wait")?;
                let result =
                    json!({"ok":true,"value":collaboration::wait_result(&state, &wait, 0)});
                let call = &mut agent_turn(&mut state, "root")?.core_calls[index];
                call.result = Some(result.clone());
                let event = DurableEvent {
                    event_seq: 3 + index as u64,
                    kind: "collaboration.applied".into(),
                    run_id: Some("run".into()),
                    agent_id: Some("root".into()),
                    payload: json!({"source":"model","invocation_id":call.invocation_id,"operation":"wait_agent","result":result}),
                };
                responses::capture(&mut state, &event)?;
                check(&state, &proposal(&state, &grant)?, &host, &grant)?;
            }
            pairing::consume(agent_mut(&mut state, "root")?, false)?;
            check(&state, &proposal(&state, &grant)?, &host, &grant)?;
            let root = &state.agents["root"];
            assert_eq!(root.context_sources.len(), 5);
            assert_eq!(root.history.len(), 3);
            for message in &root.history[1..] {
                let Content::ToolResult {
                    output: bitrouter_sdk::language_model::types::ToolResultOutput::Text { value },
                    ..
                } = &message.content[0]
                else {
                    return Err("unpaired model wait result".into());
                };
                let value: Value = serde_json::from_str(value)?;
                assert_eq!(
                    value["value"]["agents"][0]["final_answer"],
                    json!(turn_answer(&state)?)
                );
                assert_eq!(
                    value["value"]["agents"][0]["context_sources"]
                        .as_array()
                        .ok_or("sources")?
                        .len(),
                    5
                );
            }
        }
        Ok(())
    }

    fn saturated_host(
        state: &SessionSnapshot,
        grant: &OwnershipGrant,
        limits: &Limits,
    ) -> Result<Limits, CoreError> {
        let proposed = proposal(state, grant)?;
        let mut host = limits.clone();
        check(state, &proposed, &host, grant)?;
        let (mut low, mut high) = (0, host.checkpoint_bytes);
        while low < high {
            let middle = low + (high - low) / 2;
            host.checkpoint_bytes = middle;
            match check(state, &proposed, &host, grant) {
                Ok(()) => high = middle,
                Err(error) if error.code == ErrorCode::LimitExceeded => low = middle + 1,
                Err(error) => return Err(error),
            }
        }
        host.checkpoint_bytes = high;
        check(state, &proposed, &host, grant)?;
        Ok(host)
    }

    #[test]
    fn saturated_model_wait_keeps_verification_sources_through_pairing()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut state = fixture()?;
        let mut child = state.agents["root"].clone();
        child.agent_id = "child".into();
        child.parent_id = Some("root".into());
        child.display_path = "/root/child".into();
        child.depth = 1;
        let turn = child.turn.as_mut().ok_or("child turn")?;
        turn.agent_turn_id = "child-turn".into();
        turn.status = AgentStatus::WaitingTool;
        turn.final_answer = Some("provisional answer".into());
        let limits = Limits {
            checkpoint_bytes: 4 * 1024 * 1024,
            ..Limits::default()
        };
        state.run.as_mut().ok_or("run")?.limits = limits.clone();
        let result_limits =
            crate::core::protocol::ToolResultLimits::for_input(limits.input_bytes, 1024)?;
        let call: Invocation = serde_json::from_value(json!({
            "dispatch":{"invocation_id":"verification","attempt_id":"tool-attempt","run_id":"run","agent_id":"child",
                "agent_turn_id":"child-turn","step_id":"settled-step","context_revision":0,"tool":"verify","arguments":{},
                "tool_manifest_digest":state.manifest.tool_manifest_digest,"permission_revision":1,"workspace_id":"workspace",
                "execution_epoch":1,"authorizing_event_seq":1,"verification":true,"result_limits":result_limits},
            "public_call_id":"verification","provider_call_id":"verification","consumed":false,
            "result_limit_bytes":1024,"effect":"read","signal_revision":0
        }))?;
        let mut result = ToolResult {
            invocation_id: "verification".into(),
            attempt_id: "tool-attempt".into(),
            status: ToolOutcome::Succeeded,
            output: String::new(),
            evidence: Vec::new(),
            workspace_revision: Some(String::new()),
        };
        let padding = result_limits
            .payload_bytes
            .checked_sub(serialized_bytes(&result)?)
            .ok_or("result envelope")?;
        // Quotes force the second JSON-string encoding in model-wait history.
        result.workspace_revision = Some(
            "\"".repeat(usize::try_from(padding / 2)?) + &"x".repeat(usize::try_from(padding % 2)?),
        );
        assert_eq!(serialized_bytes(&result)?, result_limits.payload_bytes);
        result_limits.validate_result(&result)?;
        turn.invocations.push(call);
        state.agents.insert("child".into(), child);
        let root = agent_mut(&mut state, "root")?;
        let turn = root.turn.as_mut().ok_or("root turn")?;
        turn.status = AgentStatus::WaitingMessage;
        for index in 0..2 {
            let id = format!("model-wait-{index}");
            let action = Action::Wait {
                agent_ids: vec!["child".into()],
                timeout_ms: 600_000,
            };
            turn.core_calls.push(Call {
                invocation_id: id.clone(),
                public_call_id: id.clone(),
                provider_call_id: id,
                step_id: "root-step".into(),
                action,
                wait_output_version: Some(wait_output::VERSION),
                wait: Some(collaboration::WaitState {
                    targets: BTreeMap::from([(
                        "child".into(),
                        collaboration::WaitTarget {
                            agent_turn_id: "child-turn".into(),
                            status: Some(AgentStatus::WaitingTool),
                        },
                    )]),
                    deadline_ms: 600_000,
                }),
                result: None,
                consumed: false,
            });
        }
        responses::begin(&mut state, "input", "run", None, 1)?;
        let grant = OwnershipGrant {
            session_id: "session".into(),
            harness_id: "harness".into(),
            core_instance_id: "core".into(),
            execution_epoch: 1,
        };
        let host = saturated_host(&state, &grant, &limits)?;
        agent_turn(&mut state, "child")?.invocations[0].result = Some(result.clone());
        check(&state, &proposal(&state, &grant)?, &host, &grant)?;
        // Spend every other available byte after the receipt is durable; its
        // downstream source obligation must survive until canonical pairing.
        let host = saturated_host(&state, &grant, &host)?;
        pairing::consume(agent_mut(&mut state, "child")?, false)?;
        check(&state, &proposal(&state, &grant)?, &host, &grant)?;
        assert_eq!(
            state.agents["child"].context_sources[0].workspace_revision,
            result.workspace_revision
        );
        let host = saturated_host(&state, &grant, &host)?;
        for index in 0..2 {
            let wait = state.agents["root"].turn.as_ref().ok_or("turn")?.core_calls[index]
                .wait
                .clone()
                .ok_or("wait")?;
            let value = json!({"ok":true,"value":collaboration::wait_result(&state, &wait, 0)});
            let call = &mut agent_turn(&mut state, "root")?.core_calls[index];
            call.result = Some(value.clone());
            let event = DurableEvent {
                event_seq: 3 + index as u64,
                kind: "collaboration.applied".into(),
                run_id: Some("run".into()),
                agent_id: Some("root".into()),
                payload: json!({"source":"model","invocation_id":call.invocation_id,"operation":"wait_agent","result":value}),
            };
            responses::capture(&mut state, &event)?;
            check(&state, &proposal(&state, &grant)?, &host, &grant)?;
        }
        pairing::consume(agent_mut(&mut state, "root")?, false)?;
        check(&state, &proposal(&state, &grant)?, &host, &grant)?;
        assert_eq!(
            state.agents["root"].context_sources,
            state.agents["child"].context_sources
        );
        Ok(())
    }

    fn turn_answer(state: &SessionSnapshot) -> Result<&str, Box<dyn std::error::Error>> {
        state
            .agents
            .get("child")
            .and_then(|agent| agent.turn.as_ref())
            .and_then(|turn| turn.final_answer.as_deref())
            .ok_or_else(|| "child answer missing".into())
    }
}
