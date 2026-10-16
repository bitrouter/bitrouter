//! Database fencing; runtime tool/workspace evidence is verified separately.
use super::*;
use bitrouter_orchestrator::goal::workflow::LocalReconciliation;

#[tokio::test]
async fn late_original_stop_confirmation_survives_reopen_without_reconciling_effects() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("late-stop.db").display()
    );
    let db = bitrouter::db::connect(&url).await?;
    bitrouter::db::run_migrations(&db).await?;
    let goals = service(db.clone());
    let id = planned(&goals, directory.path()).await?;
    request(&goals, &id, "original").await?;
    admit_dispatch(&goals, &id, "original").await?;
    for (key, change) in [
        (
            "unknown-execution",
            Change::ObserveExecution {
                execution_id: "original".into(),
                observation: Observation::Unknown {
                    run_id: Some("original-run".into()),
                    reason: "original process outcome unavailable".into(),
                },
            },
        ),
        (
            "stop-original",
            Change::RequestStop {
                execution_id: "original".into(),
                operation_id: "original-stop".into(),
            },
        ),
        (
            "unknown-stop",
            Change::ObserveStop {
                execution_id: "original".into(),
                operation_id: "original-stop".into(),
                confirmed: false,
                detail: "stop result has not arrived".into(),
            },
        ),
    ] {
        goals
            .workflow_change(actor(), &id, key, 1, change, 100)
            .await?;
    }
    let before = snapshot(&goals, &id).await?;
    drop(goals);
    db.close().await?;
    let goals = service(bitrouter::db::connect(&url).await?);
    let confirmation = Change::ObserveStop {
        execution_id: "original".into(),
        operation_id: "original-stop".into(),
        confirmed: true,
        detail: "late original process exit receipt; external effects remain unresolved".into(),
    };
    for change in [
        Change::ObserveStop {
            execution_id: "original".into(),
            operation_id: "another-stop".into(),
            confirmed: true,
            detail: "receipt for another action".into(),
        },
        Change::ObserveStop {
            execution_id: "original".into(),
            operation_id: "original-stop".into(),
            confirmed: false,
            detail: "still unknown".into(),
        },
    ] {
        assert!(matches!(
            goals
                .workflow_change(actor(), &id, "invalid-refinement", 1, change, 100)
                .await,
            Err(GoalError::Conflict)
        ));
    }
    assert!(matches!(
        goals
            .workflow_change(
                WorkflowActor {
                    owner: "local",
                    coordinator_id: "stale-owner"
                },
                &id,
                "stale-refinement",
                1,
                confirmation.clone(),
                100,
            )
            .await,
        Err(GoalError::Conflict)
    ));
    assert_eq!(snapshot(&goals, &id).await?.revision, before.revision);
    let receipt = goals
        .workflow_change(
            actor(),
            &id,
            "late-confirmation",
            1,
            confirmation.clone(),
            100,
        )
        .await?;
    assert_eq!(
        goals
            .workflow_change(actor(), &id, "late-confirmation", 1, confirmation, 101)
            .await?,
        receipt
    );
    let after = snapshot(&goals, &id).await?;
    let original = &after.workflow.tasks[0].executions[0];
    assert!(matches!(&original.stop,
        Some(StopState::Confirmed { operation_id, .. }) if operation_id == "original-stop"));
    assert_eq!(
        original.observation,
        before.workflow.tasks[0].executions[0].observation
    );
    assert!(!original.effects_reconciled());
    assert_eq!(after.run_intent, before.run_intent);
    assert_eq!(after.budget, before.budget);
    assert_eq!(after.requirements_revision, before.requirements_revision);
    assert_eq!(after.workflow.tasks[0].executions.len(), 1);
    assert!(!after.workflow.completion.completed_for(1));
    assert!(matches!(
        request(&goals, &id, "unreconciled-replacement").await,
        Err(GoalError::Conflict)
    ));
    assert!(matches!(
        goals
            .workflow_change(
                actor(),
                &id,
                "downgrade-confirmed",
                1,
                Change::ObserveStop {
                    execution_id: "original".into(),
                    operation_id: "original-stop".into(),
                    confirmed: false,
                    detail: "late uncertainty cannot erase confirmation".into()
                },
                101
            )
            .await,
        Err(GoalError::Conflict)
    ));
    Ok(())
}

#[tokio::test]
async fn mapped_failure_stop_is_separate_from_control_intent_and_retry_admission() -> Result<()> {
    for observation in [
        Observation::Unknown {
            run_id: Some("owned-run".into()),
            reason: "startup result uncertain".into(),
        },
        Observation::Failed {
            run_id: Some("owned-run".into()),
            reason: "startup failed".into(),
        },
        Observation::Running {
            run_id: Some("owned-run".into()),
            native_session_id: None,
        },
    ] {
        let goals = service(database().await?);
        let workspace = tempfile::tempdir()?;
        let id = planned(&goals, workspace.path()).await?;
        request(&goals, &id, "original").await?;
        admit_dispatch(&goals, &id, "original").await?;
        goals
            .workflow_change(
                actor(),
                &id,
                "actual-observation",
                1,
                Change::ObserveExecution {
                    execution_id: "original".into(),
                    observation: observation.clone(),
                },
                100,
            )
            .await?;
        let before = snapshot(&goals, &id).await?;
        let stopped = goals
            .workflow_change(
                actor(),
                &id,
                "request-stop",
                1,
                Change::RequestStop {
                    execution_id: "original".into(),
                    operation_id: "owned-stop".into(),
                },
                100,
            )
            .await;
        if matches!(observation, Observation::Running { .. }) {
            assert!(matches!(stopped, Err(GoalError::Conflict)));
            assert_eq!(snapshot(&goals, &id).await?.revision, before.revision);
            continue;
        }
        stopped?;
        assert!(matches!(
            request(&goals, &id, "unconfirmed-retry").await,
            Err(GoalError::Conflict)
        ));
        goals
            .workflow_change(
                actor(),
                &id,
                "confirm-stop",
                1,
                Change::ObserveStop {
                    execution_id: "original".into(),
                    operation_id: "owned-stop".into(),
                    confirmed: true,
                    detail: "owned process exit observed; effects remain unresolved".into(),
                },
                100,
            )
            .await?;
        assert!(matches!(
            request(&goals, &id, "unreconciled-retry").await,
            Err(GoalError::Conflict)
        ));
        let after = snapshot(&goals, &id).await?;
        assert_eq!(after.run_intent, RunIntent::Run);
        assert_eq!(after.requirements_revision, before.requirements_revision);
        assert_eq!(after.budget, before.budget);
        assert_eq!(after.workflow.tasks[0].executions.len(), 1);
        assert_eq!(
            after.workflow.tasks[0].executions[0].observation,
            observation
        );
        assert!(!after.workflow.tasks[0].executions[0].effects_reconciled());
        assert!(!after.workflow.completion.completed_for(1));
    }
    Ok(())
}

#[tokio::test]
async fn recovering_unknown_dispatch_only_restores_a_mapping() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    request(&goals, &id, "original").await?;
    admit_dispatch(&goals, &id, "original").await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "lost-mapping",
            1,
            Change::ObserveExecution {
                execution_id: "original".into(),
                observation: Observation::Unknown {
                    run_id: None,
                    reason: "Original dispatch result commit was unavailable".into(),
                },
            },
            100,
        )
        .await?;
    for (key, change) in [
        (
            "unadmitted-execution",
            Change::RequestExecution {
                task_id: "right".into(),
                execution_id: "unadmitted".into(),
                workspace: workspace.path().join("unadmitted"),
            },
        ),
        (
            "unadmitted-unknown",
            Change::ObserveExecution {
                execution_id: "unadmitted".into(),
                observation: Observation::Unknown {
                    run_id: None,
                    reason: "No dispatch was admitted".into(),
                },
            },
        ),
    ] {
        goals
            .workflow_change(actor(), &id, key, 1, change, 100)
            .await?;
    }
    assert!(
        goals
            .workflow_change(
                actor(),
                &id,
                "unadmitted-mapping",
                1,
                Change::ObserveExecution {
                    execution_id: "unadmitted".into(),
                    observation: Observation::Running {
                        run_id: Some("not-admitted-run".into()),
                        native_session_id: None,
                    },
                },
                100
            )
            .await
            .is_err()
    );
    let before = snapshot(&goals, &id).await?;
    for (index, observation) in [
        Observation::Requested,
        Observation::Candidate {
            run_id: Some("unproved-result".into()),
            native_session_id: None,
            report: report(workspace.path(), "not original execution evidence")?,
        },
        Observation::Unknown {
            run_id: None,
            reason: "Still no original identity".into(),
        },
    ]
    .into_iter()
    .enumerate()
    {
        assert!(
            goals
                .workflow_change(
                    actor(),
                    &id,
                    &format!("bad-recovery-{index}"),
                    1,
                    Change::ObserveExecution {
                        execution_id: "original".into(),
                        observation
                    },
                    100
                )
                .await
                .is_err()
        );
        assert_eq!(snapshot(&goals, &id).await?.revision, before.revision);
    }
    goals
        .execute(
            "local",
            GoalCommand::Control {
                operation_id: "pause-recovery".into(),
                goal_id: id.clone(),
                expected_revision: before.revision,
                intent: RunIntent::Pause,
            },
            100,
        )
        .await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "recover-original",
            1,
            Change::ObserveExecution {
                execution_id: "original".into(),
                observation: Observation::Unknown {
                    run_id: Some("original-stopped-run".into()),
                    reason: "Original process stopped; effects remain unresolved".into(),
                },
            },
            100,
        )
        .await?;
    let recovered = snapshot(&goals, &id).await?;
    let execution = &recovered.workflow.tasks[0].executions[0];
    assert_eq!(recovered.run_intent, RunIntent::Pause);
    assert_eq!(recovered.workflow.tasks[0].executions.len(), 1);
    assert!(!execution.effects_reconciled());
    assert!(
        matches!(&execution.observation, Observation::Unknown { run_id: Some(run_id), .. }
        if run_id == "original-stopped-run")
    );
    assert!(matches!(
        request(&goals, &id, "replayed-dispatch").await,
        Err(GoalError::Conflict)
    ));
    assert!(
        goals
            .workflow_change(
                actor(),
                &id,
                "replace-original-mapping",
                1,
                Change::ObserveExecution {
                    execution_id: "original".into(),
                    observation: Observation::Running {
                        run_id: Some("another-run".into()),
                        native_session_id: None,
                    },
                },
                100
            )
            .await
            .is_err()
    );
    assert_eq!(snapshot(&goals, &id).await?.revision, recovered.revision);
    Ok(())
}

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
                    run_id: Some("actual-run".into()),
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

#[tokio::test]
async fn superseded_stop_wait_is_durable_bounded_and_not_stop_or_retry_authority() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    request(&goals, &id, "original").await?;
    admit_dispatch(&goals, &id, "original").await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "mapped-original",
            1,
            Change::ObserveExecution {
                execution_id: "original".into(),
                observation: Observation::Running {
                    run_id: Some("owned-run".into()),
                    native_session_id: None,
                },
            },
            100,
        )
        .await?;
    super::input_tests::commit_task_update(&goals,&id,"new-version",serde_json::from_value(serde_json::json!({"task_id":"left","instruction":"Deliver the new version without changing the original contract"}))?).await?;
    let before = snapshot(&goals, &id).await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "boundary-wait",
            2,
            Change::AwaitStopBoundary {
                execution_id: "original".into(),
                deadline_ms: 500,
            },
            150,
        )
        .await?;
    let restored = snapshot(&service(db), &id).await?;
    let execution = &restored.workflow.tasks[0].executions[0];
    assert_eq!(execution.stop_boundary_deadline_ms, Some(500));
    assert_eq!(execution.stop, None);
    assert_eq!(restored.run_intent, before.run_intent);
    assert_eq!(restored.budget, before.budget);
    for (key, deadline) in [("extend", 600), ("unbounded", 100_000), ("expired", 149)] {
        assert!(matches!(
            goals
                .workflow_change(
                    actor(),
                    &id,
                    key,
                    2,
                    Change::AwaitStopBoundary {
                        execution_id: "original".into(),
                        deadline_ms: deadline
                    },
                    151
                )
                .await,
            Err(GoalError::Conflict)
        ));
    }
    assert!(matches!(
        goals
            .workflow_change(
                actor(),
                &id,
                "early-replacement",
                2,
                Change::RequestExecution {
                    task_id: "left".into(),
                    execution_id: "new-execution".into(),
                    workspace: workspace.path().join("new")
                },
                151
            )
            .await,
        Err(GoalError::Conflict)
    ));
    let current = snapshot(&goals, &id).await?;
    goals
        .execute(
            "local",
            GoalCommand::Control {
                operation_id: "explicit-pause".into(),
                goal_id: id.clone(),
                expected_revision: current.revision,
                intent: RunIntent::Pause,
            },
            152,
        )
        .await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "stop-before-wait-deadline",
            2,
            Change::RequestStop {
                execution_id: "original".into(),
                operation_id: "actual-stop-intent".into(),
            },
            153,
        )
        .await?;
    let stopped = snapshot(&goals, &id).await?;
    let execution = &stopped.workflow.tasks[0].executions[0];
    assert_eq!(execution.stop_boundary_deadline_ms, None);
    assert!(
        matches!(&execution.stop,Some(bitrouter_orchestrator::goal::workflow::StopState::Requested { operation_id }) if operation_id=="actual-stop-intent")
    );
    assert!(!execution.effects_reconciled());
    Ok(())
}
