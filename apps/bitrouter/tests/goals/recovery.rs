//! Database fencing; runtime tool/workspace evidence is verified separately.
use super::*;
use bitrouter_orchestrator::goal::workflow::LocalReconciliation;

fn proof(workspace: &std::path::Path, control_source_revision: u64) -> Result<LocalReconciliation> {
    Ok(LocalReconciliation::Verified {
        task_revision: 1,
        control_source_revision,
        workspace: report(workspace, "stopped local workspace")?,
        transcript: report(workspace, "domain fixture, not real tool evidence")?,
    })
}

async fn request(goals: &GoalService, id: &str, key: &str) -> Result<GoalOperation, GoalError> {
    goals
        .workflow_change(
            actor(),
            id,
            key,
            1,
            Change::RequestExecution {
                task_id: "left".into(),
                execution_id: key.into(),
                workspace: std::env::temp_dir().join(key),
            },
            100,
        )
        .await
}

#[tokio::test]
async fn replacement_requires_confirmed_stop_and_current_control_proof() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    request(&goals, &id, "original").await?;
    admit_dispatch(&goals, &id, "original").await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "running",
            1,
            Change::ObserveExecution {
                execution_id: "original".into(),
                observation: Observation::Running {
                    run_id: "actual-run".into(),
                    native_session_id: None,
                },
            },
            100,
        )
        .await?;
    let current = snapshot(&goals, &id).await?;
    let rejected = goals
        .workflow_change(
            actor(),
            &id,
            "unconfirmed-proof",
            1,
            Change::ReconcileStoppedExecution {
                execution_id: "original".into(),
                reconciliation: proof(workspace.path(), current.control_source_revision)?,
            },
            100,
        )
        .await;
    assert!(matches!(rejected, Err(GoalError::Conflict)));
    assert_eq!(snapshot(&goals, &id).await?.revision, current.revision);
    assert!(matches!(
        request(&goals, &id, "no-proof").await,
        Err(GoalError::Conflict)
    ));
    goals
        .execute(
            "local",
            GoalCommand::Control {
                operation_id: "pause".into(),
                goal_id: id.clone(),
                expected_revision: current.revision,
                intent: RunIntent::Pause,
            },
            100,
        )
        .await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "stop",
            1,
            Change::RequestStop {
                execution_id: "original".into(),
                operation_id: "actual-stop".into(),
            },
            100,
        )
        .await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "stopped",
            1,
            Change::ObserveStop {
                execution_id: "original".into(),
                operation_id: "actual-stop".into(),
                confirmed: true,
                detail: "owned process stopped".into(),
            },
            100,
        )
        .await?;
    let paused = snapshot(&goals, &id).await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "paused-proof",
            1,
            Change::ReconcileStoppedExecution {
                execution_id: "original".into(),
                reconciliation: proof(workspace.path(), paused.control_source_revision)?,
            },
            100,
        )
        .await?;
    assert!(matches!(
        request(&goals, &id, "paused-replacement").await,
        Err(GoalError::Conflict)
    ));
    let paused = snapshot(&goals, &id).await?;
    goals
        .execute(
            "local",
            GoalCommand::Control {
                operation_id: "resume".into(),
                goal_id: id.clone(),
                expected_revision: paused.revision,
                intent: RunIntent::Run,
            },
            100,
        )
        .await?;
    assert!(matches!(
        request(&goals, &id, "stale-control-proof").await,
        Err(GoalError::Conflict)
    ));
    let current = snapshot(&goals, &id).await?;
    let mut stale = proof(workspace.path(), current.control_source_revision)?;
    if let LocalReconciliation::Verified { task_revision, .. } = &mut stale {
        *task_revision = 2;
    }
    assert!(matches!(
        goals
            .workflow_change(
                actor(),
                &id,
                "stale-task-proof",
                1,
                Change::ReconcileStoppedExecution {
                    execution_id: "original".into(),
                    reconciliation: stale,
                },
                100
            )
            .await,
        Err(GoalError::Conflict)
    ));
    goals
        .workflow_change(
            actor(),
            &id,
            "fresh-proof",
            1,
            Change::ReconcileStoppedExecution {
                execution_id: "original".into(),
                reconciliation: proof(workspace.path(), current.control_source_revision)?,
            },
            100,
        )
        .await?;
    request(&goals, &id, "current-replacement").await?;
    let replaced = snapshot(&goals, &id).await?;
    assert_eq!(replaced.workflow.tasks[0].executions.len(), 2);
    assert_eq!(replaced.workflow.tasks[0].executions[0].id, "original");
    assert!(replaced.workflow.tasks[0].executions[0].effects_reconciled());
    assert!(!replaced.workflow.tasks[0].executions[1].dispatch_requested);
    assert!(matches!(
        request(&goals, &id, "duplicate-replacement").await,
        Err(GoalError::Conflict)
    ));
    Ok(())
}
