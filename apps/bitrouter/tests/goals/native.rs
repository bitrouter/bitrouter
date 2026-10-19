//! Domain/SQLite association guarantees; no executor effect is claimed here.
use super::*;
use bitrouter_orchestrator::goal::workflow::ThreadExecution;

#[tokio::test]
async fn native_binding_survives_pause_without_an_acp_identity_or_replacement() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let directory = tempfile::tempdir()?;
    let id = accepted(
        goals
            .execute("local", create("native-create", directory.path()), 10)
            .await?,
    )?
    .goal_id;
    claim(&goals, &id).await?;
    let mut spec = task("native", &[]);
    spec.harness = None;
    let plan = Plan {
        tasks: vec![spec],
        completion: completion_spec(),
    };
    plan.validate(&BTreeSet::new())?;
    for (key, change) in [
        (
            "native-owner",
            Change::BindOwnerThread {
                thread_id: "owner-thread".into(),
            },
        ),
        (
            "native-plan-request",
            Change::RequestPlan {
                attempt_id: "native-plan".into(),
            },
        ),
        (
            "native-plan-install",
            Change::InstallPlan {
                attempt_id: "native-plan".into(),
                plan,
                available_harnesses: BTreeSet::new(),
            },
        ),
        (
            "native-attempt",
            Change::RequestExecution {
                task_id: "native".into(),
                execution_id: "native-execution".into(),
                workspace: directory.path().join("isolated"),
            },
        ),
    ] {
        goals
            .workflow_change(actor(), &id, key, 1, change, 20)
            .await?;
    }
    let binding = ThreadExecution {
        thread_id: "worker-thread".into(),
        turn_id: "worker-turn".into(),
    };
    let bind = Change::BindWorkerTurn {
        execution_id: "native-execution".into(),
        thread: binding.clone(),
    };
    assert!(
        goals
            .workflow_change(actor(), &id, "before-dispatch", 1, bind.clone(), 21)
            .await
            .is_err()
    );
    admit_dispatch(&goals, &id, "native-execution").await?;
    assert!(
        goals
            .workflow_change(
                actor(),
                &id,
                "missing-binding",
                1,
                Change::ObserveExecution {
                    execution_id: "native-execution".into(),
                    observation: Observation::Running {
                        run_id: None,
                        native_session_id: None
                    },
                },
                42
            )
            .await
            .is_err()
    );
    let GoalReply::Snapshot { goal } = goals
        .execute(
            "local",
            GoalCommand::Get {
                goal_id: id.clone(),
            },
            43,
        )
        .await?
    else {
        anyhow::bail!("native Goal missing");
    };
    let original_control_source = goal.control_source_revision;
    goals
        .execute(
            "local",
            GoalCommand::Control {
                operation_id: "native-pause".into(),
                goal_id: id.clone(),
                expected_revision: goal.revision,
                intent: RunIntent::Pause,
            },
            44,
        )
        .await?;
    let receipt = goals
        .workflow_change(actor(), &id, "native-link", 1, bind.clone(), 45)
        .await?;
    let reopened = service(db);
    let replay = reopened
        .workflow_change(actor(), &id, "native-link", 1, bind, 46)
        .await?;
    assert_eq!(receipt.id, replay.id);
    assert!(
        reopened
            .workflow_change(
                actor(),
                &id,
                "replace-linked-turn",
                1,
                Change::BindWorkerTurn {
                    execution_id: "native-execution".into(),
                    thread: ThreadExecution {
                        thread_id: "another-worker".into(),
                        turn_id: "another-turn".into()
                    },
                },
                47
            )
            .await
            .is_err()
    );
    assert!(
        reopened
            .workflow_change(
                actor(),
                &id,
                "invent-acp-id",
                1,
                Change::ObserveExecution {
                    execution_id: "native-execution".into(),
                    observation: Observation::Running {
                        run_id: Some("invented-run".into()),
                        native_session_id: None
                    },
                },
                48
            )
            .await
            .is_err()
    );
    reopened
        .workflow_change(
            actor(),
            &id,
            "original-admission-observed",
            1,
            Change::ObserveExecution {
                execution_id: "native-execution".into(),
                observation: Observation::Running {
                    run_id: None,
                    native_session_id: None,
                },
            },
            49,
        )
        .await?;
    let GoalReply::Snapshot { goal } = reopened
        .execute("local", GoalCommand::Get { goal_id: id }, 50)
        .await?
    else {
        anyhow::bail!("native Goal missing after reopen");
    };
    assert_eq!(goal.run_intent, RunIntent::Pause);
    let execution = &goal.workflow.tasks[0].executions[0];
    assert_eq!(execution.thread.as_ref(), Some(&binding));
    assert_eq!(execution.control_source_revision, original_control_source);
    assert!(goal.control_source_revision > execution.control_source_revision);
    use bitrouter_orchestrator::goal::decision::{
        Event, PermissionDecision, PermissionOption, PermissionState,
    };
    let decision = PermissionDecision {
        check: None,
        id: "native-choice".into(),
        execution_id: execution.id.clone(),
        run_id: None,
        permission_id: "native-input".into(),
        requirements_revision: 1,
        task_revision: 1,
        request: report(
            directory.path(),
            "Domain native permission material; no executor is run",
        )?,
        options: [true, false]
            .into_iter()
            .map(|allows| PermissionOption {
                id: if allows { "allow-once" } else { "deny-once" }.into(),
                label: if allows { "Allow once" } else { "Deny" }.into(),
                allows,
                persistent: false,
            })
            .collect(),
        state: PermissionState::Pending,
    };
    let mut persistent = decision.clone();
    persistent.options[0].persistent = true;
    assert!(
        reopened
            .workflow_change(
                actor(),
                &goal.id,
                "invent-native-persistence",
                1,
                Change::Decision {
                    event: Event::Observe {
                        decision: Box::new(persistent)
                    }
                },
                51
            )
            .await
            .is_err()
    );
    let mut acp = decision.clone();
    acp.run_id = Some("invented-acp-run".into());
    assert!(
        reopened
            .workflow_change(
                actor(),
                &goal.id,
                "confuse-native-and-acp",
                1,
                Change::Decision {
                    event: Event::Observe {
                        decision: Box::new(acp)
                    }
                },
                52
            )
            .await
            .is_err()
    );
    reopened
        .workflow_change(
            actor(),
            &goal.id,
            "observe-native-choice",
            1,
            Change::Decision {
                event: Event::Observe {
                    decision: Box::new(decision.clone()),
                },
            },
            53,
        )
        .await?;
    let current = snapshot(&reopened, &goal.id).await?;
    reopened
        .execute(
            "local",
            GoalCommand::Control {
                operation_id: "resume-before-old-choice".into(),
                goal_id: goal.id.clone(),
                expected_revision: current.revision,
                intent: RunIntent::Run,
            },
            54,
        )
        .await?;
    assert!(
        reopened
            .execute(
                "local",
                GoalCommand::RespondDecision {
                    operation_id: "stale-native-choice".into(),
                    goal_id: goal.id.clone(),
                    decision_id: decision.id.clone(),
                    expected_requirements_revision: 1,
                    option_id: "allow-once".into()
                },
                55
            )
            .await
            .is_err()
    );
    Ok(())
}

#[test]
fn legacy_acp_task_keeps_its_serialized_identity() -> Result<()> {
    let original = serde_json::to_value(task("existing", &[]))?;
    assert_eq!(original["harness"], "configured-acp");
    let restored: TaskSpec = serde_json::from_value(original.clone())?;
    assert_eq!(serde_json::to_value(restored)?, original);
    let mut native = task("built-in", &[]);
    native.harness = None;
    assert!(serde_json::to_value(&native)?.get("harness").is_none());
    assert!(
        Plan {
            tasks: vec![task("missing-acp", &[])],
            completion: completion_spec()
        }
        .validate(&BTreeSet::new())
        .is_err()
    );
    Ok(())
}
