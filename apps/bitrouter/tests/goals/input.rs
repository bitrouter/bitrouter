use super::*;
use bitrouter_orchestrator::goal::input::{
    ControlProposal, Event, Interpretation, Stage, TaskUpdate,
};

async fn admit(
    goals: &GoalService,
    id: &str,
    operation_id: &str,
    reference: Option<&str>,
) -> Result<GoalOperation> {
    let head = snapshot(goals, id).await?;
    accepted(
        goals
            .execute(
                "local",
                GoalCommand::Input {
                    operation_id: operation_id.into(),
                    goal_id: id.into(),
                    expected_revision: head.revision,
                    text: "User input bound to this Goal".into(),
                    task_reference: reference.map(str::to_string),
                },
                100,
            )
            .await?,
    )
}

async fn begin(goals: &GoalService, id: &str, operation: GoalOperation) -> Result<()> {
    goals
        .workflow_change(
            actor(),
            id,
            &format!("{}-begin", operation.id),
            1,
            Change::Input {
                event: Event::Begin {
                    attempt_id: operation.id.clone(),
                    operation: Box::new(operation),
                    stage: Stage::Reasoning,
                },
            },
            110,
        )
        .await?;
    Ok(())
}

fn query(answer: &str) -> Interpretation {
    Interpretation {
        answer: Some(answer.into()),
        objective: None,
        task_updates: Vec::new(),
        control: None,
        permission_response: None,
        clarification: None,
    }
}

#[tokio::test]
async fn resource_edits_are_versioned_atomic_preserve_scope_and_do_not_resume() -> Result<()> {
    use bitrouter_orchestrator::goal::resources::Selection;
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let current = snapshot(&goals, &id).await?;
    goals
        .execute(
            "local",
            GoalCommand::Control {
                operation_id: "pause-resource-edit".into(),
                goal_id: id.clone(),
                expected_revision: current.revision,
                intent: RunIntent::Pause,
            },
            100,
        )
        .await?;
    let operation = admit(&goals, &id, "resource-input", Some("left")).await?;
    begin(&goals, &id, operation).await?;
    let mut proposal = query("Configure context at the next controlled execution boundary");
    proposal.task_updates.push(TaskUpdate {
        task_id: "left".into(),
        instruction: "Original work and added context".into(),
        resources: Some(Selection {
            context_files: vec!["context.md".into()],
            ..Default::default()
        }),
    });
    let mut invalid = proposal.clone();
    invalid.task_updates[0].resources = Some(Selection {
        context_files: vec!["../outside".into()],
        ..Default::default()
    });
    assert!(
        finish(&goals, &id, "resource-input", invalid)
            .await
            .is_err()
    );
    let unchanged = snapshot(&goals, &id).await?;
    assert_eq!(unchanged.requirements_revision, 1);
    assert!(
        unchanged
            .workflow
            .tasks
            .iter()
            .all(|task| task.spec.resources.is_empty())
    );
    finish(&goals, &id, "resource-input", proposal).await?;
    let saved = snapshot(&goals, &id).await?;
    assert_eq!(saved.run_intent, RunIntent::Pause);
    assert_eq!(saved.requirements_revision, 2);
    assert_eq!(
        saved.workflow.tasks[0].spec.resources.context_files,
        vec!["context.md"]
    );
    assert!(saved.workflow.tasks[1].spec.resources.is_empty());
    assert_eq!(saved.workflow.tasks[1].requirements_revision, 1);
    assert_eq!(saved.workflow.tasks[2].requirements_revision, 2);
    assert_eq!(
        saved
            .inputs
            .latest
            .as_ref()
            .map(|input| &input.affected_tasks),
        Some(&vec!["integrate".into(), "left".into()])
    );
    let legacy: TaskUpdate = serde_json::from_value(
        serde_json::json!({"task_id":"left","instruction":"unchanged resource selection"}),
    )?;
    assert!(legacy.resources.is_none());
    for (version, operation_id, resources, expected_files) in [
        (2, "preserve-resources", None, vec!["context.md"]),
        (
            3,
            "remove-resources",
            Some(Selection::default()),
            Vec::new(),
        ),
    ] {
        let operation = admit(&goals, &id, operation_id, Some("left")).await?;
        goals
            .workflow_change(
                actor(),
                &id,
                &format!("{operation_id}-begin"),
                version,
                Change::Input {
                    event: Event::Begin {
                        operation: Box::new(operation),
                        attempt_id: operation_id.into(),
                        stage: Stage::Reasoning,
                    },
                },
                150,
            )
            .await?;
        let mut proposal = query("Save the explicitly requested update while remaining paused");
        proposal.task_updates.push(TaskUpdate {
            task_id: "left".into(),
            instruction: "Keep original work; use saved resource selection".into(),
            resources,
        });
        goals
            .workflow_change(
                actor(),
                &id,
                &format!("{operation_id}-finish"),
                version,
                Change::Input {
                    event: Event::Finish {
                        attempt_id: operation_id.into(),
                        interpretation: proposal,
                        reasoning_request_id: Some("resource-reasoning".into()),
                    },
                },
                160,
            )
            .await?;
        let saved = snapshot(&goals, &id).await?;
        assert_eq!(
            saved.workflow.tasks[0].spec.resources.context_files,
            expected_files
        );
        assert_eq!(saved.run_intent, RunIntent::Pause);
        assert_eq!(saved.requirements_revision, version + 1);
        assert_eq!(saved.workflow.tasks[1].requirements_revision, 1);
    }
    Ok(())
}

#[tokio::test]
async fn typed_classification_cannot_directly_pause_execution() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let operation = admit(&goals, &id, "typed-input", None).await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "typed-begin",
            1,
            Change::Input {
                event: Event::Begin {
                    operation: Box::new(operation),
                    attempt_id: "typed-attempt".into(),
                    stage: Stage::Typed,
                },
            },
            110,
        )
        .await?;
    let mut proposal = query("Typed confidence is not control authority");
    proposal.control = Some(ControlProposal {
        target: "goal".into(),
        intent: RunIntent::Pause,
    });
    let result = goals
        .workflow_change(
            actor(),
            &id,
            "invalid-direct-typed-effect",
            1,
            Change::Input {
                event: Event::Finish {
                    attempt_id: "typed-attempt".into(),
                    interpretation: proposal,
                    reasoning_request_id: Some("unmatched-reasoning-request".into()),
                },
            },
            120,
        )
        .await;
    assert!(matches!(result, Err(GoalError::Invalid { .. })));
    assert_eq!(snapshot(&goals, &id).await?.run_intent, RunIntent::Run);
    assert!(snapshot(&goals, &id).await?.inputs.pending.is_some());
    Ok(())
}

async fn finish(
    goals: &GoalService,
    id: &str,
    attempt: &str,
    proposal: Interpretation,
) -> Result<()> {
    goals
        .workflow_change(
            actor(),
            id,
            &format!("{attempt}-finish"),
            1,
            Change::Input {
                event: Event::Finish {
                    attempt_id: attempt.into(),
                    interpretation: proposal,
                    reasoning_request_id: Some("observed-provider-request".into()),
                },
            },
            130,
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn query_does_not_mutate_requirements_or_run_intent() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let operation = admit(&goals, &id, "question", None).await?;
    let original_revision = operation.revision;
    begin(&goals, &id, operation).await?;
    finish(&goals, &id, "question", query("No execution has started")).await?;
    let head = snapshot(&goals, &id).await?;
    assert_eq!(head.requirements_revision, 1);
    assert_eq!(head.run_intent, RunIntent::Run);
    assert_eq!(head.inputs.after_revision, original_revision);
    let result = head.inputs.latest.context("missing processed input")?;
    assert!(!result.requirements_saved);
    assert!(result.affected_tasks.is_empty());
    assert!(result.control_saved.is_none());
    assert!(goals.next_input("local", &id).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn paused_branch_edit_invalidates_dependents_and_preserves_execution_source() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let handoff = request_handoff(&goals, &id, workspace.path(), "left").await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "left-approved",
            1,
            Change::ApproveHandoff {
                execution_id: "left-execution".into(),
                handoff,
            },
            100,
        )
        .await?;
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
            100,
        )
        .await?;
    let operation = admit(&goals, &id, "branch-edit", Some("left")).await?;
    begin(&goals, &id, operation).await?;
    let mut proposal = query("Save the added regression requirement");
    proposal.task_updates.push(TaskUpdate {
        task_id: "left".into(),
        instruction: "Fix expiry and verify the regression with the original API".into(),
        resources: None,
    });
    finish(&goals, &id, "branch-edit", proposal).await?;
    let head = snapshot(&goals, &id).await?;
    assert_eq!(head.run_intent, RunIntent::Pause);
    assert_eq!(head.requirements_revision, 2);
    assert_eq!(head.workflow.tasks[0].requirements_revision, 2);
    assert_eq!(head.workflow.tasks[1].requirements_revision, 1);
    assert_eq!(head.workflow.tasks[2].requirements_revision, 2);
    let old = &head.workflow.tasks[0].executions[0];
    assert_eq!(old.requirements_revision, 1);
    assert_eq!(old.task_revision, 1);
    assert_eq!(
        old.handoff
            .as_ref()
            .context("lost old evidence")?
            .task_revision,
        1
    );
    let result = head.inputs.latest.context("missing edit result")?;
    assert!(result.requirements_saved);
    assert_eq!(result.affected_tasks, vec!["integrate", "left"]);
    Ok(())
}

#[tokio::test]
async fn newer_explicit_pause_dominates_older_model_resume() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let operation = admit(&goals, &id, "older-resume", None).await?;
    begin(&goals, &id, operation).await?;
    let head = snapshot(&goals, &id).await?;
    goals
        .execute(
            "local",
            GoalCommand::Control {
                operation_id: "newer-pause".into(),
                goal_id: id.clone(),
                expected_revision: head.revision,
                intent: RunIntent::Pause,
            },
            120,
        )
        .await?;
    let mut proposal = query("Resume was requested by the older input");
    proposal.control = Some(ControlProposal {
        target: "goal".into(),
        intent: RunIntent::Run,
    });
    finish(&goals, &id, "older-resume", proposal).await?;
    let head = snapshot(&goals, &id).await?;
    assert_eq!(head.run_intent, RunIntent::Pause);
    let result = head.inputs.latest.context("missing input result")?;
    assert!(result.control_superseded);
    assert!(result.control_saved.is_none());
    Ok(())
}

#[tokio::test]
async fn understanding_cannot_forge_or_reorder_original_input() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let first = admit(&goals, &id, "first", None).await?;
    let second = admit(&goals, &id, "second", None).await?;
    let mut forged = first.clone();
    forged.kind = OperationKind::Input {
        text: "Forged instructions".into(),
        task_reference: None,
    };
    for (index, operation) in [second.clone(), forged].into_iter().enumerate() {
        let result = goals
            .workflow_change(
                actor(),
                &id,
                &format!("bad-begin-{index}"),
                1,
                Change::Input {
                    event: Event::Begin {
                        operation: Box::new(operation),
                        attempt_id: "invalid".into(),
                        stage: Stage::Reasoning,
                    },
                },
                110,
            )
            .await;
        assert!(matches!(result, Err(GoalError::Conflict)));
    }
    begin(&goals, &id, first).await?;
    finish(&goals, &id, "first", query("First input processed")).await?;
    assert_eq!(goals.next_input("local", &id).await?, Some(second));
    Ok(())
}

#[tokio::test]
async fn explicit_task_reference_cannot_modify_another_task_or_control_goal() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let operation = admit(&goals, &id, "scoped-edit", Some("left")).await?;
    begin(&goals, &id, operation).await?;
    let mut wrong_task = query("Change the wrong task");
    wrong_task.task_updates.push(TaskUpdate {
        task_id: "right".into(),
        instruction: "Unauthorized target".into(),
        resources: None,
    });
    let mut wrong_control = query("Pause the wrong scope");
    wrong_control.control = Some(ControlProposal {
        target: "goal".into(),
        intent: RunIntent::Pause,
    });
    let revision = snapshot(&goals, &id).await?.revision;
    for (index, interpretation) in [wrong_task, wrong_control].into_iter().enumerate() {
        let result = goals
            .workflow_change(
                actor(),
                &id,
                &format!("invalid-scope-{index}"),
                1,
                Change::Input {
                    event: Event::Finish {
                        attempt_id: "scoped-edit".into(),
                        interpretation,
                        reasoning_request_id: None,
                    },
                },
                120,
            )
            .await;
        assert!(matches!(result, Err(GoalError::Invalid { .. })));
    }
    assert_eq!(snapshot(&goals, &id).await?.revision, revision);
    finish(
        &goals,
        &id,
        "scoped-edit",
        Interpretation {
            answer: None,
            objective: None,
            task_updates: Vec::new(),
            control: None,
            permission_response: None,
            clarification: Some("Clarify the intended task control".into()),
        },
    )
    .await?;
    assert_eq!(snapshot(&goals, &id).await?.run_intent, RunIntent::Run);
    Ok(())
}

#[tokio::test]
async fn branch_pause_seals_its_dependency_closure_and_keeps_independent_work_eligible()
-> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let operation = admit(&goals, &id, "pause-left", Some("left")).await?;
    begin(&goals, &id, operation).await?;
    let mut proposal = query("Pause the left branch");
    proposal.control = Some(ControlProposal {
        target: "task:left".into(),
        intent: RunIntent::Pause,
    });
    finish(&goals, &id, "pause-left", proposal).await?;
    let head = snapshot(&goals, &id).await?;
    assert_eq!(head.run_intent, RunIntent::Run);
    assert_eq!(head.workflow.tasks[0].run_intent, RunIntent::Pause);
    assert_eq!(head.workflow.tasks[1].run_intent, RunIntent::Run);
    assert_eq!(head.workflow.tasks[2].run_intent, RunIntent::Pause);
    assert_eq!(
        head.inputs
            .latest
            .context("missing control receipt")?
            .controlled_tasks,
        vec!["integrate", "left"]
    );
    for (task, expected) in [("left", false), ("right", true)] {
        let result = goals
            .workflow_change(
                actor(),
                &id,
                &format!("{task}-launch"),
                1,
                Change::RequestExecution {
                    task_id: task.into(),
                    execution_id: format!("{task}-execution"),
                    workspace: workspace.path().join(task),
                },
                140,
            )
            .await;
        assert_eq!(result.is_ok(), expected);
    }
    Ok(())
}

#[tokio::test]
async fn branch_resume_does_not_resume_a_paused_goal() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let head = snapshot(&goals, &id).await?;
    goals
        .execute(
            "local",
            GoalCommand::Control {
                operation_id: "pause-goal".into(),
                goal_id: id.clone(),
                expected_revision: head.revision,
                intent: RunIntent::Pause,
            },
            90,
        )
        .await?;
    let operation = admit(&goals, &id, "resume-branch", Some("left")).await?;
    begin(&goals, &id, operation).await?;
    let mut proposal = query("Save branch resume intent inside the paused Goal");
    proposal.control = Some(ControlProposal {
        target: "task:left".into(),
        intent: RunIntent::Run,
    });
    finish(&goals, &id, "resume-branch", proposal).await?;
    assert_eq!(snapshot(&goals, &id).await?.run_intent, RunIntent::Pause);
    let result = goals
        .workflow_change(
            actor(),
            &id,
            "forbidden-launch",
            1,
            Change::RequestExecution {
                task_id: "left".into(),
                execution_id: "execution".into(),
                workspace: workspace.path().join("left"),
            },
            140,
        )
        .await;
    assert!(matches!(result, Err(GoalError::Conflict)));
    Ok(())
}

#[tokio::test]
async fn branch_control_cannot_retarget_an_explicit_reference_or_missing_task() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let operation = admit(&goals, &id, "scoped-control", Some("left")).await?;
    begin(&goals, &id, operation).await?;
    let revision = snapshot(&goals, &id).await?.revision;
    for (index, target) in ["task:right", "task:outside-goal"].into_iter().enumerate() {
        let mut interpretation = query("Wrong target");
        interpretation.control = Some(ControlProposal {
            target: target.into(),
            intent: RunIntent::Stop,
        });
        let result = goals
            .workflow_change(
                actor(),
                &id,
                &format!("wrong-target-{index}"),
                1,
                Change::Input {
                    event: Event::Finish {
                        attempt_id: "scoped-control".into(),
                        interpretation,
                        reasoning_request_id: Some("model-request".into()),
                    },
                },
                140,
            )
            .await;
        assert!(matches!(result, Err(GoalError::Invalid { .. })));
    }
    assert_eq!(snapshot(&goals, &id).await?.revision, revision);
    Ok(())
}

#[tokio::test]
async fn obsolete_planning_is_retained_and_only_new_requirements_can_rearm_it() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let created = accepted(
        goals
            .execute("local", create("create", workspace.path()), 10)
            .await?,
    )?;
    let id = created.goal_id;
    claim(&goals, &id).await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "plan-request",
            1,
            Change::RequestPlan {
                attempt_id: "old-plan".into(),
            },
            20,
        )
        .await?;
    let operation = admit(&goals, &id, "new-requirements", None).await?;
    begin(&goals, &id, operation).await?;
    let mut proposal = query("Save the updated Goal before any worker starts");
    proposal.objective = Some(
        "Fix expiry and verify browser refresh behavior without changing the public API".into(),
    );
    finish(&goals, &id, "new-requirements", proposal).await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "retain-old-attempt",
            1,
            Change::BlockPlan {
                attempt_id: "old-plan".into(),
                reason: "Original planning result is obsolete; no old dispatch".into(),
            },
            140,
        )
        .await?;
    let head = snapshot(&goals, &id).await?;
    assert_eq!(head.requirements_revision, 2);
    assert_eq!(head.workflow.planning_requirements_revision, 1);
    assert!(head.workflow.tasks.is_empty());
    goals
        .workflow_change(actor(), &id, "rearm-new-version", 2, Change::RearmPlan, 150)
        .await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "new-plan-request",
            2,
            Change::RequestPlan {
                attempt_id: "new-plan".into(),
            },
            160,
        )
        .await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "new-plan-blocked",
            2,
            Change::BlockPlan {
                attempt_id: "new-plan".into(),
                reason: "No new evidence after this planning failure".into(),
            },
            170,
        )
        .await?;
    let repeated = goals
        .workflow_change(
            actor(),
            &id,
            "forbidden-same-version-rearm",
            2,
            Change::RearmPlan,
            180,
        )
        .await;
    assert!(matches!(repeated, Err(GoalError::Conflict)));
    Ok(())
}
