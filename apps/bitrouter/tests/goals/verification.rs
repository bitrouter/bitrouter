use super::*;
use bitrouter_orchestrator::goal::verification::{CheckOutput, CheckRecipe, CheckSpec};
use bitrouter_orchestrator::goal::workflow::CheckState;

fn command() -> CheckSpec {
    CheckSpec {
        id: "behavior".into(),
        description: "Actual regression check must pass".into(),
        recipe: CheckRecipe::CargoTest,
        timeout_seconds: 1200,
    }
}

async fn checked_candidate(
    goals: &GoalService,
    db: &DatabaseConnection,
    workspace: &std::path::Path,
) -> Result<(String, Handoff)> {
    let id = planned(goals, workspace).await?;
    // Bind a required check in the real store before the execution. This
    // isolates completion evidence validation from model planning quality.
    let mut head = snapshot(goals, &id).await?;
    let expected = head.revision;
    head.revision += 1;
    head.workflow.tasks[0].spec.checks = vec![command()];
    let operation = GoalOperation {
        id: "required-check".into(),
        fingerprint: "fixture-check-definition".into(),
        goal_id: id.clone(),
        revision: head.revision,
        requirements_revision: 1,
        accepted_at_ms: 40,
        kind: OperationKind::Input {
            text: "Require the behavioral check".into(),
            task_reference: Some("left".into()),
        },
    };
    DatabaseGoalStore::new(db.clone())
        .commit(Some(expected), head, operation)
        .await?;
    let handoff = request_handoff(goals, &id, workspace, "left").await?;
    Ok((id, handoff))
}

fn output(exit_status: i32) -> CheckOutput {
    CheckOutput {
        completed: true,
        passed: true,
        exit_status: Some(exit_status),
        timed_out: false,
        environment: serde_json::json!({"source":"fixture-observation"}),
        output: serde_json::json!({"stdout":"success","exit_status":exit_status}),
    }
}

#[tokio::test]
async fn exact_successful_check_allows_the_bound_handoff() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let (id, handoff) = checked_candidate(&goals, &db, workspace.path()).await?;
    for (key, change) in [
        (
            "check-request",
            Change::RequestCheck {
                execution_id: "left-execution".into(),
                attempt_id: "check".into(),
                check_id: "behavior".into(),
                workspace: handoff.workspace.clone(),
            },
        ),
        (
            "check-result",
            Change::ObserveCheck {
                execution_id: "left-execution".into(),
                attempt_id: "check".into(),
                report: report(workspace.path(), "actual exit 0")?,
                output: output(0),
                workspace_after: handoff.workspace.clone(),
            },
        ),
        (
            "checked-handoff",
            Change::ApproveHandoff {
                execution_id: "left-execution".into(),
                handoff,
            },
        ),
    ] {
        goals
            .workflow_change(actor(), &id, key, 1, change, 120)
            .await?;
    }
    assert!(
        snapshot(&goals, &id).await?.workflow.tasks[0].executions[0]
            .handoff
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn nonzero_exit_and_changed_artifact_never_support_handoff() -> Result<()> {
    let db = database().await?;

    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let (id, handoff) = checked_candidate(&goals, &db, workspace.path()).await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "check-request",
            1,
            Change::RequestCheck {
                execution_id: "left-execution".into(),
                attempt_id: "check".into(),
                check_id: "behavior".into(),
                workspace: handoff.workspace.clone(),
            },
            100,
        )
        .await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "check-result",
            1,
            Change::ObserveCheck {
                execution_id: "left-execution".into(),
                attempt_id: "check".into(),
                report: report(workspace.path(), "actual exit 7")?,
                output: output(7),
                workspace_after: handoff.workspace.clone(),
            },
            110,
        )
        .await?;
    assert!(matches!(
        snapshot(&goals, &id).await?.workflow.tasks[0].executions[0].checks[0].state,
        CheckState::Observed { passed: false, .. }
    ));
    let result = goals
        .workflow_change(
            actor(),
            &id,
            "bad-handoff",
            1,
            Change::ApproveHandoff {
                execution_id: "left-execution".into(),
                handoff,
            },
            120,
        )
        .await;
    assert!(matches!(result, Err(GoalError::Conflict)));
    Ok(())
}

#[tokio::test]
async fn positive_exit_requires_the_exact_checked_workspace_version() -> Result<()> {
    let db = database().await?;

    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let (id, handoff) = checked_candidate(&goals, &db, workspace.path()).await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "check-request",
            1,
            Change::RequestCheck {
                execution_id: "left-execution".into(),
                attempt_id: "check".into(),
                check_id: "behavior".into(),
                workspace: handoff.workspace.clone(),
            },
            100,
        )
        .await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "check-result",
            1,
            Change::ObserveCheck {
                execution_id: "left-execution".into(),
                attempt_id: "check".into(),
                report: report(workspace.path(), "actual exit 0")?,
                output: output(0),
                workspace_after: report(workspace.path(), "changed during test")?,
            },
            110,
        )
        .await?;
    assert!(matches!(
        snapshot(&goals, &id).await?.workflow.tasks[0].executions[0].checks[0].state,
        CheckState::Observed { passed: false, .. }
    ));
    Ok(())
}
