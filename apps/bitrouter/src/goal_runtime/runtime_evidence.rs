//! Public protocol observations for reasoning; never a private transcript or new authority.
use super::*;
use crate::supervisor::{ReplayBatch, RunSnapshot, SessionEventKind};
use agent_client_protocol::schema::v1::SessionUpdate;

const MAX_TIMELINE_EVENTS: usize = 64;
mod historical;

pub(super) async fn historical(
    coordinator: &Coordinator,
    goal: &GoalSnapshot,
    task: &Task,
) -> serde_json::Value {
    Box::pin(historical::collect(coordinator, goal, task)).await
}

pub(super) async fn collect(
    coordinator: &Coordinator,
    goal: &GoalSnapshot,
    task: &Task,
    execution: &Execution,
) -> serde_json::Value {
    let result = async {
        if task.execution_harness(execution).is_none() {
            return coordinator
                .native_execution_evidence(goal, task, execution)
                .await;
        }
        let snapshot = coordinator.acp_run_snapshot(goal, task, execution).await?;
        let response = coordinator
            .supervisor
            .dispatch(SessionRequest::authorized(
                &coordinator.grant,
                SessionCommand::Events {
                    run_id: snapshot.run_id.clone(),
                    after_seq: 0,
                },
            ))
            .await?;
        let SessionResponse::Events { replay } = response else {
            anyhow::bail!("runtime event observations unavailable");
        };
        project(goal, task, execution, &snapshot, &replay)
    }
    .await;
    match result {
        Ok(value) => value,
        Err(error) => serde_json::json!({
            "status":"unavailable", "goal_id":goal.id,
            "requirements_revision":goal.requirements_revision,
            "task_id":task.spec.id,"task_revision":task.requirements_revision,
            "execution_id":execution.id,"reason":bounded_error(&error),
            "authority":"An unavailable observation supplies no execution, control, permission or verification evidence."
        }),
    }
}

fn project(
    goal: &GoalSnapshot,
    task: &Task,
    execution: &Execution,
    snapshot: &RunSnapshot,
    replay: &ReplayBatch,
) -> Result<serde_json::Value> {
    anyhow::ensure!(
        goal.workflow
            .tasks
            .iter()
            .any(|known| known.spec == task.spec
                && known.requirements_revision == task.requirements_revision
                && known.executions.contains(execution)),
        "execution is outside this Goal/task version"
    );
    project_bound(goal, task, execution, snapshot, replay)
}

// Callers must establish either live membership or an exact archive receipt.
fn project_bound(
    goal: &GoalSnapshot,
    task: &Task,
    execution: &Execution,
    snapshot: &RunSnapshot,
    replay: &ReplayBatch,
) -> Result<serde_json::Value> {
    let run_id = match &execution.observation {
        Observation::Running { run_id, .. } | Observation::Candidate { run_id, .. } => {
            run_id.as_ref()
        }
        Observation::Failed { run_id, .. } | Observation::Unknown { run_id, .. } => run_id.as_ref(),
        Observation::Requested => None,
    };
    anyhow::ensure!(
        run_id == Some(&snapshot.run_id)
            && task.execution_harness(execution) == Some(snapshot.agent_id.as_str())
            && snapshot.cwd.canonicalize()? == execution.workspace.canonicalize()?,
        "runtime observations do not match this execution, harness or workspace"
    );
    anyhow::ensure!(
        replay.snapshot_seq == snapshot.last_seq,
        "execution observations changed while collecting evidence"
    );
    let mut last_seq = replay.first_retained_seq.saturating_sub(1);
    let mut last_at = None;
    let mut time_ordered = true;
    let mut omitted_payloads = 0_u64;
    let mut timeline = VecDeque::new();
    let mut observed_events = 0_u64;
    let mut prompt_open = false;
    let mut worker_activity_seen = false;
    let mut activity_markers = 0_u64;
    for event in &replay.events {
        anyhow::ensure!(
            last_seq.checked_add(1) == Some(event.seq) && event.seq <= replay.snapshot_seq,
            "runtime event sequence is inconsistent"
        );
        last_seq = event.seq;
        if last_at.is_some_and(|at| at > event.at) {
            time_ordered = false;
        }
        last_at = Some(event.at);
        let observation = match &event.kind {
            SessionEventKind::Lifecycle { process } => {
                Some(serde_json::json!({"event":"process","process":process}))
            }
            SessionEventKind::Turn { turn } => {
                Some(serde_json::json!({"event":"controller_turn_state","turn":turn}))
            }
            SessionEventKind::UserPrompt { .. } => {
                prompt_open = true;
                worker_activity_seen = false;
                Some(serde_json::json!({"event":"prompt_admitted"}))
            }
            SessionEventKind::Update { update } if prompt_open && !worker_activity_seen => {
                if matches!(
                    update,
                    SessionUpdate::AgentMessageChunk(_)
                        | SessionUpdate::ToolCall(_)
                        | SessionUpdate::ToolCallUpdate(_)
                ) {
                    worker_activity_seen = true;
                    activity_markers += 1;
                    Some(serde_json::json!({"event":"worker_activity_observed"}))
                } else {
                    None
                }
            }
            SessionEventKind::Permission { .. } if prompt_open && !worker_activity_seen => {
                worker_activity_seen = true;
                activity_markers += 1;
                Some(serde_json::json!({"event":"worker_permission_requested"}))
            }
            SessionEventKind::TurnSettled { .. } => {
                prompt_open = false;
                Some(serde_json::json!({"event":"turn_settled"}))
            }
            SessionEventKind::TurnFailed { .. } => {
                prompt_open = false;
                Some(serde_json::json!({"event":"turn_failed"}))
            }
            SessionEventKind::PayloadOmitted { .. } => {
                omitted_payloads += 1;
                None
            }
            _ => None,
        };
        if let Some(mut observation) = observation {
            observation["seq"] = event.seq.into();
            observation["at"] = serde_json::to_value(event.at)?;
            observed_events += 1;
            timeline.push_back(observation);
            if timeline.len() > MAX_TIMELINE_EVENTS {
                timeline.pop_front();
            }
        }
    }
    anyhow::ensure!(
        last_seq == replay.snapshot_seq || replay.events.is_empty(),
        "runtime event cutoff was not reached"
    );
    let complete = replay.history_complete
        && omitted_payloads == 0
        && (snapshot.last_seq == 0 || replay.first_retained_seq == 1)
        && (snapshot.last_seq == 0 || !replay.events.is_empty());
    Ok(serde_json::json!({
        "status":"observed", "goal_id":goal.id,"requirements_revision":goal.requirements_revision,
        "task_id":task.spec.id,"task_revision":task.requirements_revision,
        "execution_id":execution.id,"execution_requirements_revision":execution.requirements_revision,
        "execution_task_revision":execution.task_revision,
        "current_task_version":execution.task_revision == task.requirements_revision,
        "run_id":snapshot.run_id,"harness":snapshot.agent_id,
        "process":snapshot.process,"turn":snapshot.turn,"failure_recorded":snapshot.failure.is_some(),
        "requested_model_route":execution.resources.as_ref().and_then(|r|r.selection.model_route.as_ref()),
        "controller_confirmed_route":execution.confirmed_model_route,
        "supervisor_confirmed_route":snapshot.confirmed_route,
        "actual_provider_model":"unreported by these protocol observations",
        "history_complete":complete,"snapshot_seq":replay.snapshot_seq,
        "timeline_complete":complete && observed_events <= MAX_TIMELINE_EVENTS as u64,
        "time_ordered":time_ordered,"timeline":timeline,
        "worker_activity_markers":activity_markers,"admission_is_execution_evidence":false,
        "omitted_timeline_events":observed_events.saturating_sub(MAX_TIMELINE_EVENTS as u64),
        "scope":"Only this execution. No prompt text, reasoning, worker output, tool arguments or permission grants are copied. Missing or omitted history is unknown, not evidence of no work. Prompt and Working markers are controller admission, not acknowledgment or actual worker activity. Public agent output/tool updates or an incoming permission request provide one observed activity marker per pending turn; repeated chunks are coalesced. An activity marker does not prove tool completion or artifact correctness. Only complete, ordered activity-to-settled boundaries can support execution overlap; never infer missing boundaries or use process admission times. Uncontrolled default routing requires no model-switch acknowledgment and is not an actual provider-model identity. These observations grant no permission and do not verify artifacts."
    }))
}

#[cfg(test)]
mod tests;
