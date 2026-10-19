use super::*;
use bitrouter_orchestrator::goal::decision::{
    Event, PermissionDecision, PermissionOption, PermissionState,
};

pub(super) async fn pending(
    goals: &GoalService,
    workspace: &std::path::Path,
) -> Result<(String, PermissionDecision)> {
    let id = planned(goals, workspace).await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "launch",
            1,
            Change::RequestExecution {
                task_id: "left".into(),
                execution_id: "execution".into(),
                workspace: workspace.join("left"),
            },
            40,
        )
        .await?;
    admit_dispatch(goals, &id, "execution").await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "running",
            1,
            Change::ObserveExecution {
                execution_id: "execution".into(),
                observation: Observation::Running {
                    run_id: Some("run".into()),
                    native_session_id: None,
                },
            },
            50,
        )
        .await?;
    let decision = PermissionDecision {
        check: None,
        id: "permission-decision".into(),
        execution_id: "execution".into(),
        run_id: Some("run".into()),
        permission_id: "actual-permission-id".into(),
        requirements_revision: 1,
        task_revision: 1,
        request: report(workspace, "Actual captured permission request")?,
        options: vec![
            PermissionOption {
                id: "offered-once".into(),
                label: "Allow once".into(),
                allows: true,
                persistent: false,
            },
            PermissionOption {
                id: "offered-always".into(),
                label: "Always allow".into(),
                allows: true,
                persistent: true,
            },
            PermissionOption {
                id: "offered-reject".into(),
                label: "Reject once".into(),
                allows: false,
                persistent: false,
            },
        ],
        state: PermissionState::Pending,
    };
    goals
        .workflow_change(
            actor(),
            &id,
            "observe-permission",
            1,
            Change::Decision {
                event: Event::Observe {
                    decision: decision.clone(),
                },
            },
            60,
        )
        .await?;
    Ok((id, decision))
}

fn response(id: &str, option_id: &str) -> GoalCommand {
    GoalCommand::RespondDecision {
        operation_id: format!("response-{option_id}"),
        goal_id: id.into(),
        decision_id: "permission-decision".into(),
        expected_requirements_revision: 1,
        option_id: option_id.into(),
    }
}

#[tokio::test]
async fn actual_option_admission_is_idempotent_and_distinct_from_submission() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let (id, _) = pending(&goals, workspace.path()).await?;
    let command = response(&id, "offered-once");
    let first = accepted(goals.execute("local", command.clone(), 70).await?)?;
    let second = accepted(service(db).execute("local", command, 80).await?)?;
    assert_eq!(first, second);
    let head = snapshot(&goals, &id).await?;
    assert!(matches!(
        &head.workflow.decisions[0].state,
        PermissionState::Requested {
            dispatch_requested: false,
            ..
        }
    ));
    goals
        .workflow_change(
            actor(),
            &id,
            "permission-dispatch",
            1,
            Change::Decision {
                event: Event::RequestDelivery {
                    decision_id: "permission-decision".into(),
                    operation_id: "response-offered-once".into(),
                },
            },
            90,
        )
        .await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "permission-selected",
            1,
            Change::Decision {
                event: Event::Result {
                    decision_id: "permission-decision".into(),
                    operation_id: "response-offered-once".into(),
                    submitted: true,
                    detail: "Controller recorded selection; tool effects remain unverified".into(),
                },
            },
            100,
        )
        .await?;
    assert!(matches!(
        snapshot(&goals, &id).await?.workflow.decisions[0].state,
        PermissionState::Submitted { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn permission_cannot_select_an_unoffered_option_or_approve_while_paused() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let (id, _) = pending(&goals, workspace.path()).await?;
    assert!(matches!(
        goals
            .execute("local", response(&id, "invented-option"), 70)
            .await,
        Err(GoalError::Invalid { .. })
    ));
    let head = snapshot(&goals, &id).await?;
    goals
        .execute(
            "local",
            GoalCommand::Control {
                operation_id: "pause".into(),
                goal_id: id.clone(),
                expected_revision: head.revision,
                intent: RunIntent::Pause,
            },
            80,
        )
        .await?;
    assert!(matches!(
        goals
            .execute("local", response(&id, "offered-once"), 90)
            .await,
        Err(GoalError::Conflict)
    ));
    goals
        .execute("local", response(&id, "offered-reject"), 100)
        .await?;
    Ok(())
}

#[tokio::test]
async fn automatic_response_cannot_grant_a_persistent_permission() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let (id, _) = pending(&goals, workspace.path()).await?;
    let result = goals
        .workflow_change(
            actor(),
            &id,
            "auto-always",
            1,
            Change::Decision {
                event: Event::Respond {
                    decision_id: "permission-decision".into(),
                    operation_id: "auto-effect".into(),
                    option_id: "offered-always".into(),
                    automatic: true,
                },
            },
            70,
        )
        .await;
    assert!(matches!(result, Err(GoalError::Conflict)));
    assert_eq!(
        snapshot(&goals, &id).await?.workflow.decisions[0].state,
        PermissionState::Pending
    );
    Ok(())
}

#[tokio::test]
async fn newer_requirements_reject_stale_permission_response() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let (id, _) = pending(&goals, workspace.path()).await?;
    let mut head = snapshot(&goals, &id).await?;
    let expected = head.revision;
    head.revision += 1;
    head.requirements_revision = 2;
    let operation = GoalOperation {
        id: "requirements-changed".into(),
        fingerprint: "fixture-version-change".into(),
        goal_id: id.clone(),
        revision: head.revision,
        requirements_revision: 2,
        accepted_at_ms: 70,
        kind: OperationKind::Input {
            text: "New requirement".into(),
            task_reference: None,
        },
    };
    DatabaseGoalStore::new(db)
        .commit(Some(expected), head, operation)
        .await?;
    assert!(matches!(
        goals
            .execute("local", response(&id, "offered-once"), 80)
            .await,
        Err(GoalError::Conflict)
    ));
    Ok(())
}
