//! Real database authority tests. Fixture evidence is not provider/behavior proof.
use super::*;
use bitrouter_orchestrator::goal::completion::{
    self, ConditionResult, Event, Satisfaction, State, Verdict,
};
use bitrouter_orchestrator::goal::input::{
    ClarificationKind, Event as InputEvent, Interpretation, Resolution, ResolutionKind, Stage,
};
use bitrouter_orchestrator::goal::verification::{CheckOutput, CheckRecipe, CheckSpec};

async fn change(
    goals: &GoalService,
    id: &str,
    key: &str,
    revision: u64,
    event: Event,
) -> Result<GoalOperation, GoalError> {
    goals
        .workflow_change(
            actor(),
            id,
            key,
            revision,
            Change::Completion { event },
            200,
        )
        .await
}

async fn delivery(
    goals: &GoalService,
    workspace: &std::path::Path,
    human: bool,
    checked: bool,
) -> Result<(String, ArtifactRef)> {
    let mut spec = completion_spec();
    spec.human_acceptance_required = human;
    if checked {
        spec.checks = vec![CheckSpec {
            id: "integration".into(),
            description: "Actual integrated regression must pass".into(),
            recipe: CheckRecipe::CargoTest,
            timeout_seconds: 1200,
        }];
    }
    let id = planned_with_spec(goals, workspace, spec).await?;
    for task in ["left", "right", "integrate"] {
        let handoff = request_handoff(goals, &id, workspace, task).await?;
        goals
            .workflow_change(
                actor(),
                &id,
                &format!("{task}-handoff"),
                1,
                Change::ApproveHandoff {
                    execution_id: format!("{task}-execution"),
                    handoff,
                },
                110,
            )
            .await?;
    }
    let sources = completion::sources(&snapshot(goals, &id).await?)?;
    change(
        goals,
        &id,
        "completion-begin",
        1,
        Event::Begin {
            attempt_id: "delivery".into(),
            sources,
            workspace: workspace.join("delivery"),
        },
    )
    .await?;
    let artifact = report(workspace, "integrated fixture artifact")?;
    change(
        goals,
        &id,
        "delivery-artifact",
        1,
        Event::Delivery {
            attempt_id: "delivery".into(),
            base_commit: "a".repeat(40),
            artifact: artifact.clone(),
        },
    )
    .await?;
    Ok((id, artifact))
}

fn verdict() -> Verdict {
    Verdict {
        conditions: vec![ConditionResult {
            id: "original-goal".into(),
            status: Satisfaction::Met,
            reason: "Fixture semantic observation for database validation".into(),
            evidence: vec!["integrated_material".into()],
        }],
        unmet_original_requirements: Vec::new(),
        constraints_preserved: true,
        human_acceptance_required: false,
        summary: "Reviewable fixture delivery; no real-provider claim".into(),
    }
}

async fn assess(
    goals: &GoalService,
    id: &str,
    artifact: &ArtifactRef,
    workspace: &std::path::Path,
    value: Verdict,
) -> Result<()> {
    change(
        goals,
        id,
        "assessment-request",
        1,
        Event::RequestAssessment {
            attempt_id: "delivery".into(),
        },
    )
    .await?;
    change(
        goals,
        id,
        "assessment-result",
        1,
        Event::ObserveAssessment {
            attempt_id: "delivery".into(),
            report: report(workspace, "fixture assessment")?,
            request_id: "fixture-request".into(),
            verdict: value,
            workspace_after: artifact.clone(),
        },
    )
    .await?;
    Ok(())
}

fn commit(artifact: &ArtifactRef) -> Event {
    Event::Commit {
        attempt_id: "delivery".into(),
        workspace_after: artifact.clone(),
    }
}

#[tokio::test]
async fn completion_is_autonomous_and_immutable_for_the_current_version() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let (id, artifact) = delivery(&goals, workspace.path(), false, false).await?;
    assess(&goals, &id, &artifact, workspace.path(), verdict()).await?;
    let first = change(&goals, &id, "complete", 1, commit(&artifact)).await?;
    assert_eq!(
        first,
        change(&goals, &id, "complete", 1, commit(&artifact)).await?
    );
    let restored = snapshot(&service(db), &id).await?;
    assert!(restored.workflow.completion.completed_for(1));
    let record = restored
        .workflow
        .completion
        .last_completed
        .context("missing completion")?;
    assert_eq!(record.delivery, artifact);
    assert!(!record.human_accepted);
    assert_eq!(record.sources.len(), 3);
    assert_eq!(
        record.assessment.verdict.conditions[0].status,
        Satisfaction::Met
    );
    Ok(())
}

#[tokio::test]
async fn already_verified_delivery_can_complete_at_the_budget_boundary() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let (id, artifact) = delivery(&goals, workspace.path(), false, false).await?;
    assess(&goals, &id, &artifact, workspace.path(), verdict()).await?;
    let head = snapshot(&goals, &id).await?;
    goals
        .execute(
            "local",
            GoalCommand::SetBudget {
                operation_id: "budget-reached".into(),
                goal_id: id.clone(),
                expected_revision: head.revision,
                limits: bitrouter_orchestrator::goal::budget::Limits {
                    elapsed_ms: Some(0),
                    managed_token_estimates_micro_usd: Some(0),
                },
            },
            190,
        )
        .await?;
    assert!(snapshot(&goals, &id).await?.budget.resume_required);
    change(&goals, &id, "complete", 1, commit(&artifact)).await?;
    assert!(
        snapshot(&goals, &id)
            .await?
            .workflow
            .completion
            .completed_for(1)
    );
    Ok(())
}

#[tokio::test]
async fn explicit_acceptance_is_bound_to_the_exact_delivery() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let (id, artifact) = delivery(&goals, workspace.path(), true, false).await?;
    // The model cannot remove a previously required human acceptance.
    assess(&goals, &id, &artifact, workspace.path(), verdict()).await?;
    assert!(matches!(
        change(&goals, &id, "premature", 1, commit(&artifact)).await,
        Err(GoalError::Conflict)
    ));
    let command = |hash: String| GoalCommand::AcceptDelivery {
        operation_id: "accept".into(),
        goal_id: id.clone(),
        expected_requirements_revision: 1,
        attempt_id: "delivery".into(),
        delivery_sha256: hash,
    };
    assert!(matches!(
        goals.execute("local", command("b".repeat(64)), 210).await,
        Err(GoalError::Conflict)
    ));
    let receipt = accepted(
        goals
            .execute("local", command(artifact.sha256.clone()), 220)
            .await?,
    )?;
    assert_eq!(
        receipt,
        accepted(
            goals
                .execute("local", command(artifact.sha256.clone()), 230)
                .await?
        )?
    );
    change(&goals, &id, "complete", 1, commit(&artifact)).await?;
    assert!(
        snapshot(&goals, &id)
            .await?
            .workflow
            .completion
            .last_completed
            .is_some_and(|record| record.human_accepted)
    );
    Ok(())
}

#[tokio::test]
async fn missing_required_check_and_false_success_cannot_reach_assessment() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let (id, artifact) = delivery(&goals, workspace.path(), false, true).await?;
    assert!(matches!(
        change(
            &goals,
            &id,
            "missing-check",
            1,
            Event::RequestAssessment {
                attempt_id: "delivery".into()
            }
        )
        .await,
        Err(GoalError::Conflict)
    ));
    change(
        &goals,
        &id,
        "check-request",
        1,
        Event::RequestCheck {
            attempt_id: "delivery".into(),
            check_id: "integration".into(),
            run_id: "check".into(),
        },
    )
    .await?;
    change(
        &goals,
        &id,
        "check-result",
        1,
        Event::ObserveCheck {
            attempt_id: "delivery".into(),
            run_id: "check".into(),
            report: report(workspace.path(), "fixture exit 7")?,
            output: CheckOutput {
                completed: true,
                passed: true,
                exit_status: Some(7),
                timed_out: false,
                environment: serde_json::json!({"source":"fixture"}),
                output: serde_json::json!({"stdout":"all green"}),
            },
            workspace_after: artifact,
        },
    )
    .await?;
    assert!(matches!(
        change(
            &goals,
            &id,
            "failed-check",
            1,
            Event::RequestAssessment {
                attempt_id: "delivery".into()
            }
        )
        .await,
        Err(GoalError::Conflict)
    ));
    Ok(())
}

#[tokio::test]
async fn unverified_condition_or_original_requirement_prevents_completion() -> Result<()> {
    for unverified in [true, false] {
        let goals = service(database().await?);
        let workspace = tempfile::tempdir()?;
        let (id, artifact) = delivery(&goals, workspace.path(), false, false).await?;
        let mut value = verdict();
        if unverified {
            value.conditions[0].status = Satisfaction::Unverified;
        } else {
            value
                .unmet_original_requirements
                .push("Required behavior missing from the proposed contract".into());
        }
        assess(&goals, &id, &artifact, workspace.path(), value).await?;
        assert!(matches!(
            snapshot(&goals, &id)
                .await?
                .workflow
                .completion
                .attempt
                .map(|attempt| attempt.state),
            Some(State::Blocked { .. })
        ));
        assert!(matches!(
            change(&goals, &id, "complete", 1, commit(&artifact)).await,
            Err(GoalError::Conflict)
        ));
    }
    Ok(())
}

#[tokio::test]
async fn accepted_input_and_new_requirements_fence_old_completion() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let (id, artifact) = delivery(&goals, workspace.path(), false, false).await?;
    assess(&goals, &id, &artifact, workspace.path(), verdict()).await?;
    let head = snapshot(&goals, &id).await?;
    let operation = accepted(
        goals
            .execute(
                "local",
                GoalCommand::Input {
                    operation_id: "new-input".into(),
                    goal_id: id.clone(),
                    expected_revision: head.revision,
                    text: "Also deliver a required regression".into(),
                    task_reference: None,
                },
                240,
            )
            .await?,
    )?;
    assert!(matches!(
        change(&goals, &id, "before-input-understood", 1, commit(&artifact)).await,
        Err(GoalError::Conflict)
    ));
    for (key, event) in [
        (
            "input-begin",
            InputEvent::Begin {
                operation: Box::new(operation),
                attempt_id: "understanding".into(),
                stage: Stage::Reasoning,
            },
        ),
        (
            "input-result",
            InputEvent::Finish {
                attempt_id: "understanding".into(),
                interpretation: Interpretation {
                    answer: None,
                    objective: Some("Deliver original work plus required regression".into()),
                    task_updates: Vec::new(),
                    control: None,
                    permission_response: None,
                    clarification_kind: None,
                    resolutions: Vec::new(),
                    clarification: None,
                },
                reasoning_request_id: Some("fixture-understanding-request".into()),
            },
        ),
    ] {
        goals
            .workflow_change(actor(), &id, key, 1, Change::Input { event }, 250)
            .await?;
    }
    assert_eq!(snapshot(&goals, &id).await?.requirements_revision, 2);
    assert!(matches!(
        change(&goals, &id, "old-version-complete", 1, commit(&artifact)).await,
        Err(GoalError::Conflict)
    ));
    assert!(
        !snapshot(&goals, &id)
            .await?
            .workflow
            .completion
            .completed_for(2)
    );
    Ok(())
}

#[tokio::test]
async fn unresolved_change_survives_a_query_and_fences_ready_completion() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let (id, artifact) = delivery(&goals, workspace.path(), false, false).await?;
    assess(&goals, &id, &artifact, workspace.path(), verdict()).await?;
    for (key, text, answer, clarification) in [
        (
            "ambiguous-change",
            "Also add the other required regression",
            None,
            Some("Which required regression should be added?"),
        ),
        (
            "unrelated-query",
            "What has already been verified?",
            Some("The original delivery was verified"),
            None,
        ),
    ] {
        let head = snapshot(&goals, &id).await?;
        let operation = accepted(
            goals
                .execute(
                    "local",
                    GoalCommand::Input {
                        operation_id: key.into(),
                        goal_id: id.clone(),
                        expected_revision: head.revision,
                        text: text.into(),
                        task_reference: None,
                    },
                    240,
                )
                .await?,
        )?;
        for (suffix, event) in [
            (
                "begin",
                InputEvent::Begin {
                    operation: Box::new(operation),
                    attempt_id: key.into(),
                    stage: Stage::Reasoning,
                },
            ),
            (
                "finish",
                InputEvent::Finish {
                    attempt_id: key.into(),
                    interpretation: Interpretation {
                        answer: answer.map(str::to_string),
                        objective: None,
                        task_updates: Vec::new(),
                        control: None,
                        permission_response: None,
                        clarification_kind: None,
                        resolutions: Vec::new(),
                        clarification: clarification.map(str::to_string),
                    },
                    reasoning_request_id: Some(format!("{key}-request")),
                },
            ),
        ] {
            goals
                .workflow_change(
                    actor(),
                    &id,
                    &format!("{key}-{suffix}"),
                    1,
                    Change::Input { event },
                    250,
                )
                .await?;
        }
    }
    let restored = snapshot(&service(db), &id).await?;
    assert_eq!(restored.requirements_revision, 1);
    assert_eq!(restored.inputs.unresolved_inputs().len(), 1);
    assert_eq!(
        restored.inputs.unresolved_inputs()[0].input_id,
        "ambiguous-change"
    );
    let context = goals
        .clarification_context("local", &id, Some("ambiguous-change"))
        .await?;
    assert!(
        matches!(&context[0].original_input.kind, OperationKind::Input { text, .. } if text == "Also add the other required regression")
    );
    assert!(matches!(
        change(
            &goals,
            &id,
            "clarification-cannot-complete",
            1,
            commit(&artifact)
        )
        .await,
        Err(GoalError::Conflict)
    ));
    process_clarification_reply(
        &goals,
        &id,
        "withdraw-change",
        "/input ambiguous-change Withdraw that additional request",
        Interpretation {
            answer: Some(
                "The additional request was withdrawn; original requirements remain".into(),
            ),
            resolutions: vec![Resolution {
                input_id: "ambiguous-change".into(),
                source_requirements_revision: 1,
                kind: ResolutionKind::Withdrawn,
            }],
            ..Default::default()
        },
    )
    .await?;
    assert!(
        snapshot(&goals, &id)
            .await?
            .inputs
            .unresolved_inputs()
            .is_empty()
    );
    change(
        &goals,
        &id,
        "complete-after-withdrawal",
        1,
        commit(&artifact),
    )
    .await?;
    assert!(
        snapshot(&goals, &id)
            .await?
            .workflow
            .completion
            .completed_for(1)
    );
    Ok(())
}

#[tokio::test]
async fn explicit_withdrawal_allows_already_verified_completion_after_budget_exhaustion()
-> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let (id, artifact) = delivery(&goals, workspace.path(), false, false).await?;
    assess(&goals, &id, &artifact, workspace.path(), verdict()).await?;
    process_clarification_reply(
        &goals,
        &id,
        "uncommitted-change",
        "Add the other required regression",
        Interpretation {
            clarification: Some("Which other regression do you require?".into()),
            clarification_kind: Some(ClarificationKind::Change),
            ..Default::default()
        },
    )
    .await?;
    let head = snapshot(&goals, &id).await?;
    goals
        .execute(
            "local",
            GoalCommand::SetBudget {
                operation_id: "exhaust-after-verification".into(),
                goal_id: id.clone(),
                expected_revision: head.revision,
                limits: bitrouter_orchestrator::goal::budget::Limits {
                    elapsed_ms: Some(1),
                    managed_token_estimates_micro_usd: None,
                },
            },
            260,
        )
        .await?;
    let before = snapshot(&goals, &id).await?;
    assert!(before.budget.resume_required);
    assert!(matches!(
        change(
            &goals,
            &id,
            "cannot-complete-unresolved",
            1,
            commit(&artifact)
        )
        .await,
        Err(GoalError::Conflict)
    ));
    goals
        .execute(
            "local",
            GoalCommand::WithdrawInput {
                operation_id: "finite-withdrawal".into(),
                goal_id: id.clone(),
                expected_revision: before.revision,
                input_id: "uncommitted-change".into(),
                source_requirements_revision: 1,
            },
            270,
        )
        .await?;
    let after = snapshot(&goals, &id).await?;
    assert_eq!(after.budget, before.budget);
    assert_eq!(after.accounting, before.accounting);
    assert_eq!(after.requirements_revision, 1);
    change(&goals, &id, "complete-existing-proof", 1, commit(&artifact)).await?;
    assert!(
        snapshot(&goals, &id)
            .await?
            .workflow
            .completion
            .completed_for(1)
    );
    Ok(())
}

async fn process_clarification_reply(
    goals: &GoalService,
    id: &str,
    key: &str,
    text: &str,
    interpretation: Interpretation,
) -> Result<()> {
    let head = snapshot(goals, id).await?;
    let operation = accepted(
        goals
            .execute(
                "local",
                GoalCommand::Input {
                    operation_id: key.into(),
                    goal_id: id.into(),
                    expected_revision: head.revision,
                    text: text.into(),
                    task_reference: None,
                },
                240,
            )
            .await?,
    )?;
    for (suffix, event) in [
        (
            "begin",
            InputEvent::Begin {
                operation: Box::new(operation),
                attempt_id: key.into(),
                stage: Stage::Reasoning,
            },
        ),
        (
            "finish",
            InputEvent::Finish {
                attempt_id: key.into(),
                interpretation,
                reasoning_request_id: Some(format!("{key}-request")),
            },
        ),
    ] {
        goals
            .workflow_change(
                actor(),
                id,
                &format!("{key}-{suffix}"),
                head.requirements_revision,
                Change::Input { event },
                250,
            )
            .await?;
    }
    Ok(())
}

#[tokio::test]
async fn query_only_clarification_is_retained_without_becoming_a_completion_condition() -> Result<()>
{
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let (id, artifact) = delivery(&goals, workspace.path(), false, false).await?;
    assess(&goals, &id, &artifact, workspace.path(), verdict()).await?;
    process_clarification_reply(
        &goals,
        &id,
        "query-detail",
        "Which part did you mean?",
        Interpretation {
            clarification: Some("Which already verified component are you asking about?".into()),
            clarification_kind: Some(ClarificationKind::Query),
            ..Default::default()
        },
    )
    .await?;
    let head = snapshot(&goals, &id).await?;
    assert_eq!(head.requirements_revision, 1);
    assert_eq!(head.inputs.unresolved_inputs().len(), 1);
    assert!(!head.inputs.completion_waits_for_input());
    change(&goals, &id, "complete-with-query", 1, commit(&artifact)).await?;
    process_clarification_reply(
        &goals,
        &id,
        "answer-query",
        "/input query-detail I meant the integrated output",
        Interpretation {
            answer: Some("The actual integrated output passed its recorded checks".into()),
            resolutions: vec![Resolution {
                input_id: "query-detail".into(),
                source_requirements_revision: 1,
                kind: ResolutionKind::Answered,
            }],
            ..Default::default()
        },
    )
    .await?;
    let restored = snapshot(&goals, &id).await?;
    assert!(restored.inputs.unresolved_inputs().is_empty());
    assert!(restored.workflow.completion.completed_for(1));
    Ok(())
}

#[tokio::test]
async fn in_flight_work_prevents_completion_until_its_outcome_is_observed() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let (id, artifact) = delivery(&goals, workspace.path(), false, false).await?;
    assess(&goals, &id, &artifact, workspace.path(), verdict()).await?;
    let intent = report(workspace.path(), "fixture model work still in flight")?;
    goals
        .workflow_change(
            actor(),
            &id,
            "work-begin",
            1,
            Change::Accounting {
                event: bitrouter_orchestrator::goal::accounting::Event::Begin {
                    work: bitrouter_orchestrator::goal::accounting::Work {
                        id: "in-flight-work".into(),
                        kind: bitrouter_orchestrator::goal::accounting::WorkKind::ProviderAttempt,
                        requirements_revision: 1,
                        request_id: Some("fixture-request".into()),
                        intent,
                        token_reservation: None,
                    },
                },
            },
            260,
        )
        .await?;
    assert!(matches!(
        change(&goals, &id, "while-work-in-flight", 1, commit(&artifact)).await,
        Err(GoalError::Conflict)
    ));
    goals
        .workflow_change(
            actor(),
            &id,
            "work-observed",
            1,
            Change::Accounting {
                event: bitrouter_orchestrator::goal::accounting::Event::Observe {
                    id: "in-flight-work".into(),
                    report: report(workspace.path(), "fixture actual outcome")?,
                    estimated_model_tokens_micro_usd: None,
                    reservation_verified: false,
                },
            },
            270,
        )
        .await?;
    change(&goals, &id, "complete", 1, commit(&artifact)).await?;
    let head = snapshot(&goals, &id).await?;
    assert!(head.workflow.completion.completed_for(1));
    assert_eq!(head.accounting.unpriced_outcomes, 1);
    Ok(())
}

#[tokio::test]
async fn artifact_change_and_commit_storage_failure_preserve_unfinished_goal() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let (id, artifact) = delivery(&goals, workspace.path(), false, false).await?;
    assess(&goals, &id, &artifact, workspace.path(), verdict()).await?;
    let changed = report(workspace.path(), "new artifact after assessment")?;
    assert!(matches!(
        change(&goals, &id, "changed-artifact", 1, commit(&changed)).await,
        Err(GoalError::Conflict)
    ));
    db.execute(Statement::from_string(DatabaseBackend::Sqlite,
        "CREATE TRIGGER reject_completion BEFORE INSERT ON bro_goal_operations WHEN NEW.id = 'complete' BEGIN SELECT RAISE(ABORT, 'fixture completion persistence failure'); END".to_owned())).await?;
    assert!(matches!(
        change(&goals, &id, "complete", 1, commit(&artifact)).await,
        Err(GoalError::Storage { .. })
    ));
    let restored = snapshot(&service(db), &id).await?;
    assert!(!restored.workflow.completion.completed_for(1));
    assert_eq!(
        restored
            .workflow
            .completion
            .attempt
            .map(|attempt| attempt.state),
        Some(State::Ready)
    );
    Ok(())
}
