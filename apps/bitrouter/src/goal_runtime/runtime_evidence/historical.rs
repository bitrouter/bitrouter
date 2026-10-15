//! Past activity can support workflow history, never current artifact authority.
use super::*;

pub(super) async fn collect(
    coordinator: &Coordinator,
    goal: &GoalSnapshot,
    task: &Task,
) -> serde_json::Value {
    let mut entries = Vec::new();
    let previous = task.executions.len().saturating_sub(1);
    for execution in task.executions.iter().take(previous.min(2)) {
        entries.push(super::collect(coordinator, goal, task, execution).await);
    }
    let mut archives_observed = 0_u64;
    if let Some(reference) = &task.latest_archive {
        let result = archive(coordinator, goal, task, reference).await;
        match result {
            Ok(value) => {
                entries.push(value);
                archives_observed = 1;
            }
            Err(error) => entries.push(serde_json::json!({"status":"unavailable",
                "archive":reference,"reason":bounded_error(&error)})),
        }
    }
    let omitted = previous.saturating_sub(previous.min(2)) as u64
        + task.archived_executions.saturating_sub(archives_observed);
    let mut value = serde_json::json!({"historical_only":true,"current_artifact_authority":false,
        "goal_id":goal.id,"task_id":task.spec.id,"entries":entries,"omitted_executions":omitted,
        "scope":"Bounded earlier execution activity from live records or an exact committed archive receipt. Complete ordered activity boundaries may support a historical workflow condition such as parallel work. These observations do not verify the current delivery, satisfy current task checks, approve a handoff, grant permissions or authorize replay. Omitted, changed or incomplete observations remain unknown."});
    if serde_json::to_vec(&value).map_or(true, |bytes| bytes.len() > 48 * 1024) {
        value["entries"] = serde_json::json!([]);
        value["omitted_executions"] = (previous as u64 + task.archived_executions).into();
        value["status"] = "unavailable: history projection exceeded 48 KiB".into();
    }
    value
}

async fn archive(
    coordinator: &Coordinator,
    goal: &GoalSnapshot,
    task: &Task,
    reference: &ArtifactRef,
) -> Result<serde_json::Value> {
    anyhow::ensure!(
        reference.bytes <= bitrouter_orchestrator::goal::MAX_RECORD_BYTES as u64,
        "archive exceeds history bound"
    );
    let root = crate::goal_artifacts::root(&coordinator.workspaces, &goal.id);
    let bytes = crate::goal_artifacts::read(&root, reference).await?;
    let material: serde_json::Value = serde_json::from_slice(&bytes)?;
    let id = material["execution"]["id"]
        .as_str()
        .context("archive execution missing")?;
    let operation = coordinator
        .goals
        .execute(
            "local",
            GoalCommand::Operation {
                operation_id: bitrouter_orchestrator::goal::workflow::archive_operation_id(
                    &goal.id, id,
                ),
            },
            now()?,
        )
        .await?;
    let GoalReply::Accepted { operation } = operation else {
        anyhow::bail!("archive has no committed receipt")
    };
    anyhow::ensure!(
        operation.revision <= goal.revision,
        "archive receipt exceeds the Goal cutoff"
    );
    let record =
        crate::goal_archive::decode(&goal.id, &task.spec.id, reference, &bytes, &operation)?;
    let execution = &record.execution;
    let snapshot = coordinator
        .supervisor
        .start_snapshot(&coordinator.grant, &execution.id)
        .await?
        .context("archived execution mapping unavailable")?;
    let SessionResponse::Events { replay } = coordinator
        .supervisor
        .dispatch(SessionRequest::authorized(
            &coordinator.grant,
            SessionCommand::Events {
                run_id: snapshot.run_id.clone(),
                after_seq: 0,
            },
        ))
        .await?
    else {
        anyhow::bail!("archived runtime history unavailable")
    };
    let mut value = project_bound(goal, task, execution, &snapshot, &replay)?;
    value["committed_archive"] = serde_json::to_value(reference)?;
    value["archive_operation_id"] = operation.id.into();
    Ok(value)
}
