use super::*;
use bitrouter_orchestrator::goal::input::{
    ClarificationKind, ControlProposal, Event, Interpretation, Resolution, ResolutionKind, Stage,
    TaskUpdate,
};

#[tokio::test]
async fn worker_change_versions_work_without_relabeling_or_replaying_its_old_attempt() -> Result<()>
{
    use bitrouter_orchestrator::goal::workflow::{ExecutionAdmission, LocalReconciliation, Worker};
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    for (key, change) in [
        (
            "original",
            Change::RequestExecution {
                task_id: "left".into(),
                execution_id: "original".into(),
                workspace: workspace.path().join("original"),
            },
        ),
        (
            "registered",
            Change::RegisterWorkspace {
                execution_id: "original".into(),
                base_commit: "a".repeat(40),
            },
        ),
        (
            "dispatch",
            Change::RequestDispatch {
                execution_id: "original".into(),
            },
        ),
        (
            "running",
            Change::ObserveExecution {
                execution_id: "original".into(),
                observation: Observation::Running {
                    run_id: Some("old-run".into()),
                    native_session_id: Some("old-session".into()),
                },
            },
        ),
    ] {
        goals
            .workflow_change(actor(), &id, key, 1, change, 40)
            .await
            .with_context(|| format!("original worker setup: {key}"))?;
    }
    let before = snapshot(&goals, &id).await?;
    let old = before.workflow.tasks[0].executions[0].clone();
    commit_task_update(
        &goals,
        &id,
        "replace-left-worker",
        TaskUpdate {
            task_id: "left".into(),
            instruction: before.workflow.tasks[0].spec.instruction.clone(),
            worker: Some(Worker::BuiltIn),
            resources: None,
            additional_checks: Vec::new(),
        },
    )
    .await
    .context("save replacement request")?;
    let saved = snapshot(&goals, &id).await?;
    let left = &saved.workflow.tasks[0];
    let control_source = saved
        .control_source_revision
        .max(left.control_source_revision);
    assert!(left.spec.harness.is_none());
    assert_eq!(left.executions[0], old);
    assert_eq!(left.execution_harness(&old), Some("configured-acp"));
    assert_eq!(
        left.execution_admission(saved.requirements_revision, saved.control_source_revision),
        ExecutionAdmission::Blocked
    );
    assert_eq!(saved.workflow.tasks[1], before.workflow.tasks[1]);
    assert_eq!(saved.workflow.tasks[2].requirements_revision, 2);
    assert_eq!(saved.run_intent, before.run_intent);
    let replacement = Change::RequestExecution {
        task_id: "left".into(),
        execution_id: "new-native".into(),
        workspace: workspace.path().join("new-native"),
    };
    assert!(
        goals
            .workflow_change(
                actor(),
                &id,
                "premature-replacement",
                2,
                replacement.clone(),
                150
            )
            .await
            .is_err()
    );
    // Late original-source facts remain ACP facts despite the new requested worker.
    goals
        .workflow_change(
            actor(),
            &id,
            "old-candidate",
            1,
            Change::ObserveExecution {
                execution_id: "original".into(),
                observation: Observation::Candidate {
                    run_id: Some("old-run".into()),
                    native_session_id: Some("old-session".into()),
                    report: report(
                        workspace.path(),
                        "domain old candidate; not harness evidence",
                    )?,
                },
            },
            151,
        )
        .await
        .context("observe original candidate")?;
    for (key, change) in [
        (
            "stop",
            Change::RequestStop {
                execution_id: "original".into(),
                operation_id: "old-stop".into(),
            },
        ),
        (
            "stopped",
            Change::ObserveStop {
                execution_id: "original".into(),
                operation_id: "old-stop".into(),
                confirmed: true,
                detail: "domain fixture old stopped boundary".into(),
            },
        ),
    ] {
        goals
            .workflow_change(actor(), &id, key, 2, change, 152)
            .await
            .with_context(|| format!("original stop boundary: {key}"))?;
    }
    assert!(
        goals
            .workflow_change(
                actor(),
                &id,
                "still-no-effect-proof",
                2,
                replacement.clone(),
                153
            )
            .await
            .is_err()
    );
    goals
        .workflow_change(
            actor(),
            &id,
            "reconcile",
            2,
            Change::ReconcileStoppedExecution {
                execution_id: "original".into(),
                reconciliation: LocalReconciliation::Verified {
                    task_revision: 2,
                    control_source_revision: control_source,
                    workspace: report(workspace.path(), "domain fixture frozen old workspace")?,
                    transcript: report(
                        workspace.path(),
                        "domain fixture no unknown effects; actual runtime proof is separate",
                    )?,
                },
            },
            154,
        )
        .await
        .context("reconcile stopped original worker")?;
    goals
        .workflow_change(actor(), &id, "new-native", 2, replacement, 155)
        .await
        .context("request replacement execution")?;
    goals
        .workflow_change(
            actor(),
            &id,
            "new-native-workspace",
            2,
            Change::RegisterWorkspace {
                execution_id: "new-native".into(),
                base_commit: "a".repeat(40),
            },
            155,
        )
        .await?;
    let admitted = snapshot(&goals, &id).await?;
    let attempts = &admitted.workflow.tasks[0].executions;
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].id, old.id);
    assert_eq!(attempts[0].worker, old.worker);
    assert_eq!(attempts[1].worker, Some(Worker::BuiltIn));
    assert!(attempts[1].thread.is_none());
    assert_eq!(attempts[1].observation, Observation::Requested);
    assert_eq!(snapshot(&goals, &id).await?, admitted);
    let source = admitted.workflow.tasks[0]
        .worker_handoff(1, saved.control_source_revision)?
        .context("retained handoff missing")?;
    let prepared = bitrouter_orchestrator::goal::resources::Prepared {
        selection: Default::default(),
        prompt: report(workspace.path(), "domain prepared worker input")?,
        materials: Vec::new(),
        checked_dependencies: None,
        worker_handoff: Some(source.clone()),
        mcp_configuration_sha256: "b".repeat(64),
    };
    for (key, handoff) in [
        ("missing-transfer", None),
        (
            "foreign-transfer",
            Some(bitrouter_orchestrator::goal::resources::WorkerHandoff {
                execution_id: "another-task".into(),
                ..source.clone()
            }),
        ),
    ] {
        let mut invalid = prepared.clone();
        invalid.worker_handoff = handoff;
        assert!(
            goals
                .workflow_change(
                    actor(),
                    &id,
                    key,
                    2,
                    Change::PrepareResources {
                        execution_id: "new-native".into(),
                        resources: invalid
                    },
                    156
                )
                .await
                .is_err()
        );
    }
    assert_eq!(snapshot(&goals, &id).await?, admitted);
    goals
        .workflow_change(
            actor(),
            &id,
            "prepared-transfer",
            2,
            Change::PrepareResources {
                execution_id: "new-native".into(),
                resources: prepared,
            },
            157,
        )
        .await
        .context("prepare replacement resources")?;
    goals
        .workflow_change(
            actor(),
            &id,
            "preparation-failed",
            2,
            Change::ObserveExecution {
                execution_id: "new-native".into(),
                observation: Observation::Failed {
                    run_id: None,
                    reason: "domain fixture: no dispatch occurred".into(),
                },
            },
            158,
        )
        .await
        .context("observe undispatched preparation failure")?;
    goals
        .workflow_change(
            actor(),
            &id,
            "retry-preparation",
            2,
            Change::RequestExecution {
                task_id: "left".into(),
                execution_id: "same-worker-next-attempt".into(),
                workspace: workspace.path().join("same-worker-next-attempt"),
            },
            159,
        )
        .await
        .context("retry replacement preparation")?;
    let retry = snapshot(&goals, &id).await?;
    assert_eq!(
        retry.workflow.tasks[0].worker_handoff(2, saved.control_source_revision)?,
        Some(source)
    );
    Ok(())
}

pub(super) async fn commit_task_update(
    goals: &GoalService,
    id: &str,
    key: &str,
    update: TaskUpdate,
) -> Result<()> {
    let operation = admit(goals, id, key, Some(&update.task_id)).await?;
    let source = operation.requirements_revision;
    for (suffix, event) in [
        (
            "begin",
            Event::Begin {
                attempt_id: key.into(),
                operation: Box::new(operation),
                stage: Stage::Reasoning,
            },
        ),
        (
            "finish",
            Event::Finish {
                attempt_id: key.into(),
                interpretation: Interpretation {
                    task_updates: vec![update],
                    ..Default::default()
                },
                reasoning_request_id: Some(format!("{key}-observed-model")),
            },
        ),
    ] {
        goals
            .workflow_change(
                actor(),
                id,
                &format!("{key}-{suffix}"),
                source,
                Change::Input { event },
                130,
            )
            .await?;
    }
    Ok(())
}

#[tokio::test]
async fn goal_wide_requirement_change_invalidates_task_sources_without_instruction_rewrites()
-> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let original = snapshot(&goals, &id).await?;
    let operation = admit(&goals, &id, "global-constraint", None).await?;
    begin(&goals, &id, operation).await?;
    let mut proposal = query("Save the API compatibility constraint for the entire Goal");
    proposal.objective = Some(format!("{}; preserve the public API", original.objective));
    finish(&goals, &id, "global-constraint", proposal).await?;
    let saved = snapshot(&goals, &id).await?;
    assert_eq!(
        saved.requirements_revision,
        original.requirements_revision + 1
    );
    for task in &saved.workflow.tasks {
        let before = original
            .workflow
            .tasks
            .iter()
            .find(|before| before.spec.id == task.spec.id)
            .context("missing original task")?;
        assert_eq!(task.requirements_revision, saved.requirements_revision);
        assert_eq!(task.spec, before.spec);
        assert_eq!(task.executions, before.executions);
    }
    assert_eq!(
        saved
            .inputs
            .latest
            .context("missing input result")?
            .affected_tasks,
        vec!["integrate", "left", "right"]
    );
    Ok(())
}

#[tokio::test]
async fn task_check_additions_are_atomic_bounded_and_preserve_original_checks() -> Result<()> {
    use bitrouter_orchestrator::goal::verification::{CheckRecipe, CheckSpec};
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let original = CheckSpec {
        id: "original-check".into(),
        description: "Original required check".into(),
        recipe: CheckRecipe::FileExists {
            path: "left.txt".into(),
        },
        timeout_seconds: 10,
    };
    let update = |checks| TaskUpdate {
        worker: None,
        task_id: "left".into(),
        instruction: "Keep original work and required verification".into(),
        resources: None,
        additional_checks: checks,
    };
    commit_task_update(&goals, &id, "first-check", update(vec![original.clone()])).await?;
    let operation = admit(&goals, &id, "new-check", Some("left")).await?;
    let source = operation.requirements_revision;
    goals
        .workflow_change(
            actor(),
            &id,
            "new-check-begin",
            source,
            Change::Input {
                event: Event::Begin {
                    attempt_id: "new-check".into(),
                    operation: Box::new(operation),
                    stage: Stage::Reasoning,
                },
            },
            140,
        )
        .await?;
    let before = snapshot(&goals, &id).await?;
    let marker = CheckSpec {
        id: "marker-check".into(),
        description: "Actual additional marker".into(),
        recipe: CheckRecipe::FileContains {
            path: "left.txt".into(),
            text: "new-marker".into(),
        },
        timeout_seconds: 10,
    };
    for (key, checks) in [
        (
            "replace",
            vec![CheckSpec {
                recipe: CheckRecipe::FileExists {
                    path: "other.txt".into(),
                },
                ..original.clone()
            }],
        ),
        ("duplicate", vec![marker.clone(), marker.clone()]),
        (
            "escape",
            vec![CheckSpec {
                recipe: CheckRecipe::FileExists {
                    path: "../outside".into(),
                },
                ..marker.clone()
            }],
        ),
        (
            "timeout",
            vec![CheckSpec {
                timeout_seconds: 0,
                ..marker.clone()
            }],
        ),
        (
            "overflow",
            (0..16)
                .map(|index| CheckSpec {
                    id: format!("new-{index}"),
                    ..marker.clone()
                })
                .collect(),
        ),
    ] {
        let result = goals
            .workflow_change(
                actor(),
                &id,
                &format!("invalid-{key}"),
                source,
                Change::Input {
                    event: Event::Finish {
                        attempt_id: "new-check".into(),
                        interpretation: Interpretation {
                            task_updates: vec![update(checks)],
                            ..Default::default()
                        },
                        reasoning_request_id: Some("observed-model".into()),
                    },
                },
                150,
            )
            .await;
        assert!(result.is_err(), "invalid additional checks accepted: {key}");
        assert_eq!(snapshot(&service(db.clone()), &id).await?, before);
    }
    goals
        .workflow_change(
            actor(),
            &id,
            "new-check-finish",
            source,
            Change::Input {
                event: Event::Finish {
                    attempt_id: "new-check".into(),
                    interpretation: Interpretation {
                        task_updates: vec![update(vec![original.clone(), marker.clone()])],
                        ..Default::default()
                    },
                    reasoning_request_id: Some("observed-model".into()),
                },
            },
            150,
        )
        .await?;
    let saved = snapshot(&service(db), &id).await?;
    assert_eq!(saved.workflow.tasks[0].spec.checks, vec![original, marker]);
    assert_eq!(saved.requirements_revision, 3);
    assert_eq!(saved.workflow.tasks[0].requirements_revision, 3);
    assert_eq!(saved.workflow.tasks[1].requirements_revision, 1);
    assert_eq!(saved.workflow.tasks[2].requirements_revision, 3);
    assert_eq!(saved.run_intent, RunIntent::Run);
    assert!(
        saved
            .inputs
            .latest
            .as_ref()
            .is_some_and(|input| input.requirements_saved && input.control_saved.is_none())
    );
    Ok(())
}

#[tokio::test]
async fn independent_candidate_reopens_review_in_new_goal_context_without_worker_replay()
-> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let old = request_handoff(&goals, &id, workspace.path(), "right").await?;
    commit_task_update(
        &goals,
        &id,
        "change-left-only",
        TaskUpdate {
            worker: None,
            task_id: "left".into(),
            instruction: "Updated left requirement".into(),
            resources: None,
            additional_checks: Vec::new(),
        },
    )
    .await?;
    let current = snapshot(&goals, &id).await?;
    assert_eq!(current.workflow.tasks[1].requirements_revision, 1);
    let request = Change::RequestReview {
        execution_id: "right-execution".into(),
        attempt_id: "fresh-right-review".into(),
        workspace: old.workspace.clone(),
    };
    assert!(
        goals
            .workflow_change(
                actor(),
                &id,
                "in-flight-review-replacement",
                2,
                request.clone(),
                150
            )
            .await
            .is_err()
    );
    goals.workflow_change(actor(), &id, "retain-cancelled-review", 1, Change::RejectReview {
        execution_id: "right-execution".into(), attempt_id: old.attempt_id.clone(),
        reason: "Original review ended at the changed Goal authority boundary; original fees remain recorded".into(), report: None,
    }, 150).await?;
    assert!(
        goals
            .workflow_change(
                actor(),
                &id,
                "reuse-rejected-result",
                2,
                Change::ApproveHandoff {
                    execution_id: "right-execution".into(),
                    handoff: old.clone()
                },
                150
            )
            .await
            .is_err()
    );
    goals
        .workflow_change(
            actor(),
            &id,
            "fresh-context-review",
            2,
            request.clone(),
            150,
        )
        .await?;
    assert!(
        goals
            .workflow_change(
                actor(),
                &id,
                "duplicate-current-review",
                2,
                request.clone(),
                150
            )
            .await
            .is_err()
    );
    let fresh = Handoff {
        attempt_id: "fresh-right-review".into(),
        requirements_revision: 2,
        review: report(workspace.path(), "fresh Goal 2 assessment")?,
        review_request_id: "fresh-right-provider-request".into(),
        ..old
    };
    goals
        .workflow_change(
            actor(),
            &id,
            "fresh-context-handoff",
            2,
            Change::ApproveHandoff {
                execution_id: "right-execution".into(),
                handoff: fresh,
            },
            150,
        )
        .await?;
    let saved = snapshot(&goals, &id).await?;
    let right = &saved.workflow.tasks[1];
    assert_eq!(right.executions.len(), 1);
    assert_eq!(right.executions[0].id, "right-execution");
    assert_eq!(right.executions[0].task_revision, 1);
    assert!(right.executions[0].handoff.as_ref().is_some_and(|handoff| handoff.requirements_revision==2 && handoff.task_revision==1));
    assert!(
        goals
            .workflow_change(
                actor(),
                &id,
                "invalidate-approved-independent-task",
                2,
                request,
                150
            )
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn clarification_resolution_preserves_scope_and_rejects_false_or_stale_closure() -> Result<()>
{
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let operation = admit(&goals, &id, "left-question", Some("left")).await?;
    begin(&goals, &id, operation).await?;
    finish(
        &goals,
        &id,
        "left-question",
        Interpretation {
            clarification: Some("Which left regression should be added?".into()),
            clarification_kind: Some(ClarificationKind::Change),
            ..Default::default()
        },
    )
    .await?;
    let operation = admit(&goals, &id, "resolution-reply", Some("left")).await?;
    begin(&goals, &id, operation).await?;
    let head = snapshot(&goals, &id).await?;
    let resolution = Resolution {
        input_id: "left-question".into(),
        source_requirements_revision: 1,
        kind: ResolutionKind::Applied,
    };
    for candidate in [
        Resolution {
            input_id: "absent-question".into(),
            ..resolution.clone()
        },
        Resolution {
            source_requirements_revision: 2,
            ..resolution.clone()
        },
        Resolution {
            kind: ResolutionKind::Answered,
            ..resolution.clone()
        },
        resolution.clone(),
    ] {
        let mut proposal = query("A factual answer is not implementation of a requested change");
        proposal.resolutions = vec![candidate];
        assert!(
            finish(&goals, &id, "resolution-reply", proposal)
                .await
                .is_err()
        );
        let unchanged = snapshot(&service(db.clone()), &id).await?;
        assert_eq!(unchanged.revision, head.revision);
        assert_eq!(unchanged.inputs.unresolved_inputs().len(), 1);
    }
    let mut proposal = query("Save the specifically requested left requirement");
    proposal.resolutions = vec![resolution];
    proposal.task_updates = vec![TaskUpdate {
        worker: None,
        task_id: "left".into(),
        instruction: "Original work plus the clarified left regression".into(),
        additional_checks: Vec::new(),
        resources: None,
    }];
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    db.execute(Statement::from_string(DatabaseBackend::Sqlite,
        "CREATE TRIGGER reject_resolution BEFORE INSERT ON bro_goal_operations WHEN NEW.id = 'resolution-reply-finish' BEGIN SELECT RAISE(ABORT, 'fixture resolution persistence failure'); END".to_owned())).await?;
    assert!(matches!(
        finish(&goals, &id, "resolution-reply", proposal.clone()).await,
        Err(error) if matches!(error.downcast_ref::<GoalError>(), Some(GoalError::Storage { .. }))
    ));
    assert_eq!(snapshot(&service(db.clone()), &id).await?, head);
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "DROP TRIGGER reject_resolution".to_owned(),
    ))
    .await?;
    finish(&goals, &id, "resolution-reply", proposal).await?;
    let saved = snapshot(&service(db), &id).await?;
    assert_eq!(saved.requirements_revision, 2);
    assert!(saved.inputs.unresolved_inputs().is_empty());
    assert_eq!(saved.workflow.tasks[0].requirements_revision, 2);
    assert_eq!(saved.workflow.tasks[1].requirements_revision, 1);
    assert_eq!(saved.workflow.tasks[2].requirements_revision, 2);
    assert_eq!(
        saved
            .inputs
            .latest
            .context("missing resolution result")?
            .resolutions[0]
            .input_id,
        "left-question"
    );
    Ok(())
}

#[tokio::test]
async fn explicit_clarification_reference_closes_only_that_request_and_never_another_goal()
-> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    for (key, task) in [("first-question", "left"), ("second-question", "right")] {
        let operation = admit(&goals, &id, key, Some(task)).await?;
        begin(&goals, &id, operation).await?;
        finish(
            &goals,
            &id,
            key,
            Interpretation {
                clarification: Some(format!("Which {task} requirement should change?")),
                clarification_kind: Some(ClarificationKind::Change),
                ..Default::default()
            },
        )
        .await?;
    }
    let head = snapshot(&goals, &id).await?;
    let operation = accepted(
        goals
            .execute(
                "local",
                GoalCommand::Input {
                    operation_id: "explicit-withdrawal".into(),
                    goal_id: id.clone(),
                    expected_revision: head.revision,
                    text: "/input first-question Withdraw that request only".into(),
                    task_reference: None,
                },
                100,
            )
            .await?,
    )?;
    begin(&goals, &id, operation).await?;
    let mut proposal = query("This request was withdrawn; other work is unchanged");
    proposal.resolutions = vec![Resolution {
        input_id: "second-question".into(),
        source_requirements_revision: 1,
        kind: ResolutionKind::Withdrawn,
    }];
    assert!(
        finish(&goals, &id, "explicit-withdrawal", proposal.clone())
            .await
            .is_err()
    );
    assert_eq!(
        snapshot(&goals, &id)
            .await?
            .inputs
            .unresolved_inputs()
            .len(),
        2
    );
    proposal.resolutions[0].input_id = "first-question".into();
    finish(&goals, &id, "explicit-withdrawal", proposal).await?;
    let saved = snapshot(&goals, &id).await?;
    assert_eq!(saved.inputs.unresolved_inputs().len(), 1);
    assert_eq!(
        saved.inputs.unresolved_inputs()[0].input_id,
        "second-question"
    );
    assert_eq!(saved.requirements_revision, 1);
    assert_eq!(saved.run_intent, RunIntent::Run);
    let other = accepted(
        goals
            .execute(
                "local",
                GoalCommand::Create {
                    operation_id: "other-goal".into(),
                    title: "Other Goal".into(),
                    workspace: workspace.path().into(),
                    objective: "Independent work".into(),
                    budget_limits: Default::default(),
                },
                120,
            )
            .await?,
    )?
    .goal_id;
    assert!(matches!(
        goals
            .execute(
                "local",
                GoalCommand::Input {
                    operation_id: "foreign-reference".into(),
                    goal_id: other.clone(),
                    expected_revision: 1,
                    text: "/input second-question Withdraw the other Goal request".into(),
                    task_reference: None,
                },
                130
            )
            .await,
        Err(GoalError::NotFound)
    ));
    assert!(matches!(
        goals
            .clarification_context("local", &other, Some("second-question"))
            .await,
        Err(GoalError::NotFound)
    ));
    Ok(())
}

#[tokio::test]
async fn legacy_latest_only_question_retains_its_original_scope_after_query_and_reload()
-> Result<()> {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let operation = admit(&goals, &id, "legacy-question", Some("left")).await?;
    begin(&goals, &id, operation).await?;
    finish(
        &goals,
        &id,
        "legacy-question",
        Interpretation {
            clarification: Some("Original unresolved left request".into()),
            ..Default::default()
        },
    )
    .await?;
    let mut legacy = serde_json::to_value(snapshot(&goals, &id).await?)?;
    legacy["inputs"]
        .as_object_mut()
        .context("inputs must be an object")?
        .remove("unresolved");
    let latest = legacy["inputs"]["latest"]
        .as_object_mut()
        .context("latest must be an object")?;
    latest.remove("clarification_kind");
    latest.remove("resolutions");
    let result = db
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE bro_goals SET payload = ? WHERE owner = ? AND id = ?",
            vec![
                serde_json::to_vec(&legacy)?.into(),
                "local".into(),
                id.clone().into(),
            ],
        ))
        .await?;
    assert_eq!(result.rows_affected(), 1);
    let restored = service(db.clone());
    assert!(
        snapshot(&restored, &id)
            .await?
            .inputs
            .completion_waits_for_input()
    );
    let operation = admit(&restored, &id, "legacy-unrelated-query", None).await?;
    begin(&restored, &id, operation).await?;
    finish(
        &restored,
        &id,
        "legacy-unrelated-query",
        query("An unrelated factual answer"),
    )
    .await?;
    let saved = snapshot(&service(db), &id).await?;
    assert_eq!(saved.inputs.unresolved.len(), 1);
    assert_eq!(saved.inputs.unresolved[0].input_id, "legacy-question");
    assert_eq!(
        saved.inputs.unresolved[0].task_reference.as_deref(),
        Some("left")
    );
    assert!(saved.inputs.completion_waits_for_input());
    assert_eq!(saved.requirements_revision, 1);
    Ok(())
}

#[tokio::test]
async fn direct_withdrawal_works_without_model_or_budget_and_preserves_other_inputs() -> Result<()>
{
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    for key in ["withdraw-first", "retain-second"] {
        let operation = admit(&goals, &id, key, None).await?;
        begin(&goals, &id, operation).await?;
        finish(
            &goals,
            &id,
            key,
            Interpretation {
                clarification: Some("Which required addition?".into()),
                ..Default::default()
            },
        )
        .await?;
    }
    let queued = admit(&goals, &id, "retain-queued-input", None).await?;
    let head = snapshot(&goals, &id).await?;
    goals
        .execute(
            "local",
            GoalCommand::Control {
                operation_id: "pause-before-withdrawal".into(),
                goal_id: id.clone(),
                expected_revision: head.revision,
                intent: RunIntent::Pause,
            },
            200,
        )
        .await?;
    let head = snapshot(&goals, &id).await?;
    goals
        .execute(
            "local",
            GoalCommand::SetBudget {
                operation_id: "cap-before-withdrawal".into(),
                goal_id: id.clone(),
                expected_revision: head.revision,
                limits: bitrouter_orchestrator::goal::budget::Limits {
                    elapsed_ms: Some(1),
                    managed_token_estimates_micro_usd: None,
                },
            },
            210,
        )
        .await?;
    let before = snapshot(&goals, &id).await?;
    let command = GoalCommand::WithdrawInput {
        operation_id: "direct-withdrawal".into(),
        goal_id: id.clone(),
        expected_revision: before.revision,
        input_id: "withdraw-first".into(),
        source_requirements_revision: 1,
    };
    let mut wrong = command.clone();
    if let GoalCommand::WithdrawInput {
        source_requirements_revision,
        ..
    } = &mut wrong
    {
        *source_requirements_revision = 2;
    }
    assert!(matches!(
        goals.execute("local", wrong, 220).await,
        Err(GoalError::Conflict)
    ));
    db.execute(Statement::from_string(DatabaseBackend::Sqlite,
        "CREATE TRIGGER reject_direct_withdrawal BEFORE INSERT ON bro_goal_operations WHEN NEW.id = 'direct-withdrawal' BEGIN SELECT RAISE(ABORT, 'fixture withdrawal persistence failure'); END".to_owned())).await?;
    assert!(matches!(
        goals.execute("local", command.clone(), 220).await,
        Err(GoalError::Storage { .. })
    ));
    assert_eq!(snapshot(&service(db.clone()), &id).await?, before);
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "DROP TRIGGER reject_direct_withdrawal".to_owned(),
    ))
    .await?;
    let first = goals.execute("local", command.clone(), 230).await?;
    let replay = goals.execute("local", command, 240).await?;
    assert_eq!(serde_json::to_value(first)?, serde_json::to_value(replay)?);
    let after = snapshot(&service(db), &id).await?;
    assert_eq!(after.inputs.unresolved_inputs().len(), 1);
    assert_eq!(
        after.inputs.unresolved_inputs()[0].input_id,
        "retain-second"
    );
    assert_eq!(after.inputs.after_revision, before.inputs.after_revision);
    assert_eq!(goals.next_input("local", &id).await?, Some(queued));
    assert_eq!(after.requirements_revision, before.requirements_revision);
    assert_eq!(after.run_intent, RunIntent::Pause);
    assert_eq!(
        after.control_source_revision,
        before.control_source_revision
    );
    assert_eq!(after.budget, before.budget);
    assert_eq!(after.accounting, before.accounting);
    assert!(
        after
            .inputs
            .latest
            .context("missing direct disposition")?
            .reasoning_request_id
            .is_none()
    );
    Ok(())
}

#[test]
fn only_an_exact_unquoted_withdrawal_uses_the_direct_path() -> Result<()> {
    use bitrouter_orchestrator::goal::input::withdrawal_reference;
    assert_eq!(
        withdrawal_reference(" /input existing-id /withdraw ")?,
        Some("existing-id")
    );
    for input in [
        "/input existing-id please withdraw it",
        "/input existing-id /withdraw please",
        "\"/input existing-id /withdraw\"",
        "If I use /input existing-id /withdraw what happens?",
        "/input existing-id /withdraw\nthen resume",
    ] {
        assert_eq!(withdrawal_reference(input)?, None);
    }
    Ok(())
}

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
        clarification_kind: None,
        resolutions: Vec::new(),
        modification_scope: None,
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
        worker: None,
        task_id: "left".into(),
        instruction: "Original work and added context".into(),
        additional_checks: Vec::new(),
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
            worker: None,
            task_id: "left".into(),
            instruction: "Keep original work; use saved resource selection".into(),
            resources,
            additional_checks: Vec::new(),
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
async fn selected_modification_scope_rejects_widening_and_keeps_independent_tasks() -> Result<()> {
    use bitrouter_orchestrator::goal::input::scope::{Binding, Selection};
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let operation = admit(&goals, &id, "named-left-change", None).await?;
    begin(&goals, &id, operation).await?;
    let before = snapshot(&goals, &id).await?;
    let binding = Binding {
        input_id: "named-left-change".into(),
        requirements_revision: 1,
        request_id: "scope-model-request".into(),
        evidence: report(
            workspace.path(),
            "fixture selected only left; not a real model judgment",
        )?,
        selection: Selection::Tasks {
            task_ids: vec!["left".into()],
        },
    };
    let proposal = Interpretation {
        task_updates: vec![TaskUpdate {
            worker: None,
            task_id: "left".into(),
            instruction: "Reproduce the left failure without implementation changes".into(),
            additional_checks: Vec::new(),
            resources: None,
        }],
        modification_scope: Some(Box::new(binding.clone())),
        ..Default::default()
    };
    for mutation in [
        "objective",
        "independent",
        "different-input",
        "different-version",
        "missing-task",
        "duplicate-task",
        "no-change",
        "unknown",
    ] {
        let mut candidate = proposal.clone();
        match mutation {
            "objective" => candidate.objective = Some("Change the entire Goal".into()),
            "independent" => candidate.task_updates.push(TaskUpdate {
                worker: None,
                task_id: "right".into(),
                instruction: "New independent constraints".into(),
                additional_checks: Vec::new(),
                resources: None,
            }),
            "different-input" => {
                candidate
                    .modification_scope
                    .as_mut()
                    .context("scope missing")?
                    .input_id = "other".into()
            }
            "different-version" => {
                candidate
                    .modification_scope
                    .as_mut()
                    .context("scope missing")?
                    .requirements_revision = 2
            }
            "missing-task" => {
                candidate
                    .modification_scope
                    .as_mut()
                    .context("scope missing")?
                    .selection = Selection::Tasks {
                    task_ids: vec!["missing".into()],
                }
            }
            "duplicate-task" => {
                candidate
                    .modification_scope
                    .as_mut()
                    .context("scope missing")?
                    .selection = Selection::Tasks {
                    task_ids: vec!["left".into(), "left".into()],
                }
            }
            "no-change" => {
                candidate
                    .modification_scope
                    .as_mut()
                    .context("scope missing")?
                    .selection = Selection::NoChange
            }
            "unknown" => {
                candidate
                    .modification_scope
                    .as_mut()
                    .context("scope missing")?
                    .selection = Selection::Unknown {
                    reason: "Which task?".into(),
                }
            }
            _ => anyhow::bail!("unexpected scope mutation"),
        }
        assert!(
            finish(&goals, &id, "named-left-change", candidate)
                .await
                .is_err(),
            "{mutation}"
        );
        assert_eq!(snapshot(&goals, &id).await?, before);
    }
    finish(&goals, &id, "named-left-change", proposal).await?;
    let saved = snapshot(&goals, &id).await?;
    assert_eq!(saved.requirements_revision, 2);
    assert_eq!(saved.workflow.tasks[0].requirements_revision, 2);
    assert_eq!(saved.workflow.tasks[1], before.workflow.tasks[1]);
    assert_eq!(saved.workflow.tasks[2].requirements_revision, 2);
    assert_eq!(
        saved
            .inputs
            .latest
            .context("scope result missing")?
            .modification_scope,
        Some(binding)
    );
    Ok(())
}

#[tokio::test]
async fn unresolved_modification_scope_preserves_independent_explicit_goal_pause() -> Result<()> {
    use bitrouter_orchestrator::goal::input::scope::{Binding, Selection};
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let original = snapshot(&goals, &id).await?;
    let operation = accepted(
        goals
            .execute(
                "local",
                GoalCommand::Input {
                    operation_id: "pause-and-ambiguous-modification".into(),
                    goal_id: id.clone(),
                    expected_revision: original.revision,
                    text: "Pause the Goal and change that branch's requirements".into(),
                    task_reference: None,
                },
                100,
            )
            .await?,
    )?;
    begin(&goals, &id, operation).await?;
    finish(
        &goals,
        &id,
        "pause-and-ambiguous-modification",
        Interpretation {
            control: Some(ControlProposal {
                target: "goal".into(),
                intent: RunIntent::Pause,
            }),
            clarification: Some("Which branch's requirements should change?".into()),
            clarification_kind: Some(ClarificationKind::Change),
            modification_scope: Some(Box::new(Binding {
                input_id: "pause-and-ambiguous-modification".into(),
                requirements_revision: 1,
                request_id: "scope-model-request".into(),
                evidence: report(workspace.path(), "fixture unresolved modification scope")?,
                selection: Selection::Unknown {
                    reason: "Which branch?".into(),
                },
            })),
            ..Default::default()
        },
    )
    .await?;
    let saved = snapshot(&goals, &id).await?;
    assert_eq!(saved.run_intent, RunIntent::Pause);
    assert_eq!(saved.requirements_revision, original.requirements_revision);
    assert_eq!(saved.workflow.tasks, original.workflow.tasks);
    assert!(saved.inputs.completion_waits_for_input());
    assert_eq!(saved.inputs.unresolved_inputs().len(), 1);
    Ok(())
}

#[tokio::test]
async fn query_clarification_preserves_explicit_pause_and_rejects_change_ambiguity() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let original = snapshot(&goals, &id).await?;
    let operation = accepted(
        goals
            .execute(
                "local",
                GoalCommand::Input {
                    operation_id: "query-and-pause".into(),
                    goal_id: id.clone(),
                    expected_revision: original.revision,
                    text: "为什么失败？先暂停当前 Goal。".into(),
                    task_reference: None,
                },
                100,
            )
            .await?,
    )?;
    begin(&goals, &id, operation).await?;
    let proposal = Interpretation {
        control: Some(ControlProposal {
            target: "goal".into(),
            intent: RunIntent::Pause,
        }),
        clarification: Some(
            "There is no recorded failure. Which attempt or log do you mean?".into(),
        ),
        clarification_kind: Some(ClarificationKind::Query),
        ..Default::default()
    };
    finish(&goals, &id, "query-and-pause", proposal).await?;
    let saved = snapshot(&goals, &id).await?;
    assert_eq!(saved.run_intent, RunIntent::Pause);
    assert_eq!(saved.requirements_revision, original.requirements_revision);
    assert_eq!(saved.workflow.tasks, original.workflow.tasks);
    assert_eq!(saved.workflow.completion, original.workflow.completion);
    assert!(!saved.inputs.completion_waits_for_input());
    assert_eq!(saved.inputs.unresolved_inputs().len(), 1);
    let result = saved.inputs.latest.context("mixed input result missing")?;
    assert_eq!(result.clarification_kind, ClarificationKind::Query);
    assert_eq!(
        result.control_saved.map(|control| control.intent),
        Some(RunIntent::Pause)
    );
    assert!(!result.requirements_saved);
    for (index, kind) in [
        Some(ClarificationKind::Change),
        Some(ClarificationKind::Unknown),
        None,
    ]
    .into_iter()
    .enumerate()
    {
        let attempt = format!("ambiguous-change-{index}");
        let operation = admit(&goals, &id, &attempt, None).await?;
        begin(&goals, &id, operation).await?;
        let before = snapshot(&goals, &id).await?;
        let ambiguous = Interpretation {
            control: Some(ControlProposal {
                target: "goal".into(),
                intent: RunIntent::Run,
            }),
            clarification: Some("The requested execution change has an unresolved target".into()),
            clarification_kind: kind,
            ..Default::default()
        };
        assert!(
            finish(&goals, &id, &attempt, ambiguous.clone())
                .await
                .is_err()
        );
        assert_eq!(snapshot(&goals, &id).await?, before);
        finish(
            &goals,
            &id,
            &attempt,
            Interpretation {
                control: None,
                ..ambiguous
            },
        )
        .await?;
        assert_eq!(snapshot(&goals, &id).await?.run_intent, RunIntent::Pause);
    }
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
        worker: None,
        task_id: "left".into(),
        instruction: "Fix expiry and verify the regression with the original API".into(),
        additional_checks: Vec::new(),
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
async fn newer_pause_supersedes_old_resume_but_keeps_its_valid_scoped_edit() -> Result<()> {
    use bitrouter_orchestrator::goal::input::scope::{Binding, Selection};
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let original = snapshot(&goals, &id).await?;
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
    proposal.task_updates.push(TaskUpdate {
        worker: None,
        task_id: "left".into(),
        instruction: "Preserve original left work and add negative input coverage".into(),
        additional_checks: Vec::new(),
        resources: None,
    });
    proposal.modification_scope = Some(Box::new(Binding {
        input_id: "older-resume".into(),
        requirements_revision: 1,
        request_id: "fixture-scope-request".into(),
        evidence: report(workspace.path(), "fixture scope selects left")?,
        selection: Selection::Tasks {
            task_ids: vec!["left".into()],
        },
    }));
    finish(&goals, &id, "older-resume", proposal).await?;
    let head = snapshot(&goals, &id).await?;
    assert_eq!(head.run_intent, RunIntent::Pause);
    assert_eq!(head.requirements_revision, 2);
    assert_eq!(head.objective, original.objective);
    for task in &head.workflow.tasks {
        let before = original
            .workflow
            .tasks
            .iter()
            .find(|before| before.spec.id == task.spec.id)
            .context("original task missing")?;
        if task.spec.id == "right" {
            assert_eq!(task, before);
        } else {
            assert_eq!(task.requirements_revision, 2);
            assert_eq!(task.executions, before.executions);
        }
    }
    let result = head.inputs.latest.context("missing input result")?;
    assert!(result.requirements_saved);
    assert_eq!(result.input_id, "older-resume");
    assert!(result.control_superseded);
    assert!(result.control_saved.is_none());
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
        worker: None,
        task_id: "right".into(),
        instruction: "Unauthorized target".into(),
        additional_checks: Vec::new(),
        resources: None,
    });
    let mut wrong_control = query("Pause the wrong scope");
    wrong_control.control = Some(ControlProposal {
        target: "goal".into(),
        intent: RunIntent::Pause,
    });
    let before = snapshot(&goals, &id).await?;
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
                        reasoning_request_id: Some("fixture-understanding-request".into()),
                    },
                },
                120,
            )
            .await;
        assert!(matches!(result, Err(GoalError::Invalid { .. })));
        assert_eq!(snapshot(&goals, &id).await?, before);
    }
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
            clarification_kind: None,
            resolutions: Vec::new(),
            modification_scope: None,
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
