//! Goal admission uses real migrations and transactions, with no model/worker effects.
#[path = "goals/accounting.rs"]
mod accounting_tests;
#[path = "goals/archive.rs"]
mod archive_tests;
#[path = "goals/budget.rs"]
mod budget_tests;
#[path = "goals/completion.rs"]
mod completion_tests;
#[path = "goals/decision.rs"]
mod decision_tests;
#[path = "goals/input.rs"]
mod input_tests;
#[path = "goals/permission_input.rs"]
mod permission_input_tests;
#[path = "goals/recovery.rs"]
mod recovery_tests;
#[path = "goals/resources.rs"]
mod resource_tests;
#[path = "goals/verification.rs"]
mod verification_tests;
use std::sync::Arc;

use anyhow::{Context, Result};
use bitrouter::goal_store::DatabaseGoalStore;
use bitrouter_orchestrator::goal::workflow::{
    ArtifactRef, Change, Handoff, Observation, Plan, Planning, StopState, TaskSpec, WorkflowActor,
};
use bitrouter_orchestrator::goal::{
    GoalCommand, GoalError, GoalOperation, GoalReply, GoalService, GoalSnapshot, GoalStore,
    OperationKind, RunIntent,
};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use std::collections::BTreeSet;

async fn database() -> Result<DatabaseConnection> {
    let db = bitrouter::db::connect("sqlite::memory:").await?;
    bitrouter::db::run_migrations(&db).await?;
    Ok(db)
}

fn service(db: DatabaseConnection) -> GoalService {
    GoalService::new(Arc::new(DatabaseGoalStore::new(db)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn independent_goal_receipts_commit_under_wal_write_contention() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("goals.db").display()
    );
    let db = bitrouter::db::connect(&url).await?;
    bitrouter::db::run_migrations(&db).await?;
    let store = Arc::new(DatabaseGoalStore::new(db));
    let barrier = Arc::new(tokio::sync::Barrier::new(16));
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..16 {
        // Supervisor/native execution assembles its own pool for the same file.
        let goals = Arc::new(GoalService::new(Arc::new(DatabaseGoalStore::new(
            bitrouter::db::connect(&url).await?,
        ))));
        let barrier = barrier.clone();
        let workspace = directory.path().to_path_buf();
        tasks.spawn(async move {
            barrier.wait().await;
            goals
                .execute(
                    "local",
                    GoalCommand::Create {
                        operation_id: format!("independent-{index}"),
                        title: format!("Goal {index}"),
                        workspace,
                        objective: "Deliver verified local work".into(),
                        budget_limits: Default::default(),
                    },
                    1,
                )
                .await
        });
    }
    let mut ids = BTreeSet::new();
    while let Some(result) = tasks.join_next().await {
        let GoalReply::Accepted { operation } = result?? else {
            anyhow::bail!("independent creation returned no original receipt")
        };
        assert!(ids.insert(operation.goal_id.clone()));
        assert_eq!(
            store.operation("local", &operation.id).await?,
            Some(operation.clone())
        );
        assert_eq!(store.get("local", &operation.goal_id).await?.revision, 1);
    }
    assert_eq!(ids.len(), 16);
    Ok(())
}

fn actor() -> WorkflowActor<'static> {
    WorkflowActor {
        owner: "local",
        coordinator_id: "test-runtime",
    }
}

async fn claim(goals: &GoalService, id: &str) -> Result<()> {
    goals
        .workflow_change(
            actor(),
            id,
            "claim-owner",
            1,
            Change::ClaimCoordinator {
                id: "test-runtime".into(),
                expires_at_ms: 30_010,
            },
            10,
        )
        .await?;
    Ok(())
}

fn report(workspace: &std::path::Path, text: &str) -> Result<ArtifactRef> {
    use sha2::{Digest, Sha256};
    let sha256 = hex::encode(Sha256::digest(text.as_bytes()));
    let path = workspace.join(format!("{sha256}.txt"));
    std::fs::write(&path, text)?;
    Ok(ArtifactRef {
        path,
        sha256,
        bytes: u64::try_from(text.len())?,
    })
}

fn task(id: &str, dependencies: &[&str]) -> TaskSpec {
    TaskSpec {
        id: id.into(),
        title: format!("Task {id}"),
        instruction: "Inspect the assigned material and report evidence".into(),
        harness: "configured-acp".into(),
        dependencies: dependencies.iter().map(|id| (*id).into()).collect(),
        checks: Vec::new(),
        resources: Default::default(),
    }
}

async fn admit_dispatch(goals: &GoalService, id: &str, execution_id: &str) -> Result<()> {
    for (suffix, change) in [
        (
            "workspace",
            Change::RegisterWorkspace {
                execution_id: execution_id.into(),
                base_commit: "a".repeat(40),
            },
        ),
        (
            "dispatch",
            Change::RequestDispatch {
                execution_id: execution_id.into(),
            },
        ),
    ] {
        goals
            .workflow_change(
                actor(),
                id,
                &format!("{execution_id}-{suffix}"),
                1,
                change,
                41,
            )
            .await?;
    }
    Ok(())
}

async fn request_handoff(
    goals: &GoalService,
    id: &str,
    workspace: &std::path::Path,
    task: &str,
) -> Result<Handoff> {
    let execution_id = format!("{task}-execution");
    let candidate = report(workspace, &format!("{task} candidate"))?;
    let handoff = Handoff {
        attempt_id: format!("{task}-review"),
        workspace: report(workspace, &format!("{task} workspace"))?,
        review: report(workspace, &format!("{task} review evidence"))?,
        review_request_id: format!("{task}-provider-request"),
        candidate_sha256: candidate.sha256.clone(),
        requirements_revision: 1,
        task_revision: 1,
    };
    for (index, change) in [
        Change::RequestExecution {
            task_id: task.into(),
            execution_id: execution_id.clone(),
            workspace: workspace.join(task),
        },
        Change::RegisterWorkspace {
            execution_id: execution_id.clone(),
            base_commit: "a".repeat(40),
        },
        Change::RequestDispatch {
            execution_id: execution_id.clone(),
        },
        Change::ObserveExecution {
            execution_id: execution_id.clone(),
            observation: Observation::Candidate {
                run_id: format!("{task}-run"),
                native_session_id: None,
                report: candidate,
            },
        },
        Change::RequestStop {
            execution_id: execution_id.clone(),
            operation_id: format!("{task}-stop"),
        },
        Change::ObserveStop {
            execution_id: execution_id.clone(),
            operation_id: format!("{task}-stop"),
            confirmed: true,
            detail: "Owned process stopped".into(),
        },
        Change::RequestReview {
            execution_id,
            attempt_id: handoff.attempt_id.clone(),
            workspace: handoff.workspace.clone(),
        },
    ]
    .into_iter()
    .enumerate()
    {
        goals
            .workflow_change(actor(), id, &format!("{task}-fact-{index}"), 1, change, 100)
            .await?;
    }
    Ok(handoff)
}

#[tokio::test]
async fn goals_handoff_requires_exact_review_and_candidate_versions() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let left = request_handoff(&goals, &id, workspace.path(), "left").await?;
    let right = request_handoff(&goals, &id, workspace.path(), "right").await?;
    let mut wrong_attempt = left.clone();
    wrong_attempt.attempt_id = "other-review".into();
    let mut wrong_candidate = left.clone();
    wrong_candidate.candidate_sha256 = "b".repeat(64);
    let mut wrong_workspace = left.clone();
    wrong_workspace.workspace = report(workspace.path(), "different workspace version")?;
    let revision = snapshot(&goals, &id).await?.revision;
    for (index, handoff) in [wrong_attempt, wrong_candidate, wrong_workspace]
        .into_iter()
        .enumerate()
    {
        let result = goals
            .workflow_change(
                actor(),
                &id,
                &format!("invalid-handoff-{index}"),
                1,
                Change::ApproveHandoff {
                    execution_id: "left-execution".into(),
                    handoff,
                },
                110,
            )
            .await;
        assert!(matches!(result, Err(GoalError::Conflict)));
    }
    assert_eq!(snapshot(&goals, &id).await?.revision, revision);
    for (task, handoff) in [("left", left), ("right", right)] {
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
                120,
            )
            .await?;
    }
    let restored = service(db);
    restored
        .workflow_change(
            actor(),
            &id,
            "integrate-launch",
            1,
            Change::RequestExecution {
                task_id: "integrate".into(),
                execution_id: "integration".into(),
                workspace: workspace.path().join("integration"),
            },
            130,
        )
        .await?;
    assert_eq!(
        snapshot(&restored, &id).await?.workflow.tasks[2]
            .executions
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn goals_rejected_review_cannot_be_promoted_to_handoff() -> Result<()> {
    let db = database().await?;
    let goals = service(db);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let handoff = request_handoff(&goals, &id, workspace.path(), "left").await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "reject-review",
            1,
            Change::RejectReview {
                execution_id: "left-execution".into(),
                attempt_id: handoff.attempt_id.clone(),
                reason: "Required machine check has no evidence".into(),
                report: Some(handoff.review.clone()),
            },
            110,
        )
        .await?;
    let result = goals
        .workflow_change(
            actor(),
            &id,
            "promote-rejected",
            1,
            Change::ApproveHandoff {
                execution_id: "left-execution".into(),
                handoff,
            },
            120,
        )
        .await;
    assert!(matches!(result, Err(GoalError::Conflict)));
    assert!(
        snapshot(&goals, &id).await?.workflow.tasks[0].executions[0]
            .handoff
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn goals_requirements_change_prevents_old_review_from_unlocking_dependents() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let handoff = request_handoff(&goals, &id, workspace.path(), "left").await?;
    let mut head = snapshot(&goals, &id).await?;
    let expected = head.revision;
    head.revision += 1;
    head.requirements_revision = 2;
    // Inject a newer committed head to isolate the handoff CAS boundary. Input
    // classification and real requirement modification are separate S3 gates.
    let operation = GoalOperation {
        id: "new-requirements".into(),
        fingerprint: "test-new-version".into(),
        goal_id: id.clone(),
        revision: head.revision,
        requirements_revision: 2,
        accepted_at_ms: 110,
        kind: OperationKind::Input {
            text: "New required check".into(),
            task_reference: None,
        },
    };
    DatabaseGoalStore::new(db)
        .commit(Some(expected), head, operation)
        .await?;
    for revision in [1, 2] {
        let result = goals
            .workflow_change(
                actor(),
                &id,
                &format!("stale-review-{revision}"),
                revision,
                Change::ApproveHandoff {
                    execution_id: "left-execution".into(),
                    handoff: handoff.clone(),
                },
                120,
            )
            .await;
        assert!(matches!(result, Err(GoalError::Conflict)));
    }
    goals
        .workflow_change(
            actor(),
            &id,
            "retain-old-review-rejection",
            1,
            Change::RejectReview {
                execution_id: "left-execution".into(),
                attempt_id: handoff.attempt_id,
                reason: "Review belongs to superseded requirements".into(),
                report: Some(handoff.review),
            },
            130,
        )
        .await?;
    let head = snapshot(&goals, &id).await?;
    assert_eq!(head.requirements_revision, 2);
    assert_eq!(head.workflow.tasks[0].requirements_revision, 1);
    assert!(head.workflow.tasks[0].executions[0].handoff.is_none());
    Ok(())
}

#[tokio::test]
async fn goals_only_current_coordinator_can_admit_effects() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let change = Change::RequestExecution {
        task_id: "left".into(),
        execution_id: "execution".into(),
        workspace: workspace.path().join("left"),
    };
    let competitor = WorkflowActor {
        owner: "local",
        coordinator_id: "other-runtime",
    };
    let rejected = goals
        .workflow_change(competitor, &id, "competing-launch", 1, change.clone(), 100)
        .await;
    assert!(matches!(rejected, Err(GoalError::Conflict)));
    goals
        .workflow_change(
            WorkflowActor {
                owner: "local",
                coordinator_id: "other-runtime",
            },
            &id,
            "take-expired-owner",
            1,
            Change::ClaimCoordinator {
                id: "other-runtime".into(),
                expires_at_ms: 60_011,
            },
            30_011,
        )
        .await?;
    let stale = goals
        .workflow_change(actor(), &id, "stale-launch", 1, change.clone(), 30_012)
        .await;
    assert!(matches!(stale, Err(GoalError::Conflict)));
    goals
        .workflow_change(
            WorkflowActor {
                owner: "local",
                coordinator_id: "other-runtime",
            },
            &id,
            "valid-launch",
            1,
            change,
            30_012,
        )
        .await?;
    assert_eq!(
        snapshot(&goals, &id).await?.workflow.tasks[0]
            .executions
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn goals_stop_confirmation_does_not_overwrite_candidate_result() -> Result<()> {
    let db = database().await?;
    let goals = service(db);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "launch",
            1,
            Change::RequestExecution {
                task_id: "left".into(),
                execution_id: "execution".into(),
                workspace: workspace.path().join("left"),
            },
            40,
        )
        .await?;
    admit_dispatch(&goals, &id, "execution").await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "candidate",
            1,
            Change::ObserveExecution {
                execution_id: "execution".into(),
                observation: Observation::Candidate {
                    run_id: "real-run".into(),
                    native_session_id: None,
                    report: report(
                        workspace.path(),
                        "Partial result that must remain reviewable",
                    )?,
                },
            },
            50,
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
            60,
        )
        .await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "stop-request",
            1,
            Change::RequestStop {
                execution_id: "execution".into(),
                operation_id: "stop-effect".into(),
            },
            70,
        )
        .await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "stop-confirmed",
            1,
            Change::ObserveStop {
                execution_id: "execution".into(),
                operation_id: "stop-effect".into(),
                confirmed: true,
                detail: "Owned ACP process stopped; external effects still require checks".into(),
            },
            80,
        )
        .await?;
    let head = snapshot(&goals, &id).await?;
    let execution = &head.workflow.tasks[0].executions[0];
    assert!(
        matches!(&execution.observation, Observation::Candidate { report, .. } if std::fs::read_to_string(&report.path).is_ok_and(|text| text == "Partial result that must remain reviewable"))
    );
    assert!(matches!(&execution.stop, Some(StopState::Confirmed { .. })));
    Ok(())
}

async fn planned(goals: &GoalService, workspace: &std::path::Path) -> Result<String> {
    planned_with_spec(goals, workspace, completion_spec()).await
}

fn completion_spec() -> bitrouter_orchestrator::goal::completion::Specification {
    bitrouter_orchestrator::goal::completion::Specification {
        conditions: vec![bitrouter_orchestrator::goal::completion::Condition {
            id: "original-goal".into(),
            description: "Deliver the original requested result with evidence".into(),
        }],
        checks: Vec::new(),
        human_acceptance_required: false,
    }
}

async fn planned_with_spec(
    goals: &GoalService,
    workspace: &std::path::Path,
    completion: bitrouter_orchestrator::goal::completion::Specification,
) -> Result<String> {
    let creation = accepted(
        goals
            .execute("local", create("create", workspace), 10)
            .await?,
    )?;
    claim(goals, &creation.goal_id).await?;
    goals
        .workflow_change(
            actor(),
            &creation.goal_id,
            "plan-request",
            1,
            Change::RequestPlan {
                attempt_id: "planning-attempt".into(),
            },
            20,
        )
        .await?;
    goals
        .workflow_change(
            actor(),
            &creation.goal_id,
            "plan-installed",
            1,
            Change::InstallPlan {
                attempt_id: "planning-attempt".into(),
                plan: Plan {
                    completion,
                    tasks: vec![
                        task("left", &[]),
                        task("right", &[]),
                        task("integrate", &["left", "right"]),
                    ],
                },
                available_harnesses: BTreeSet::from(["configured-acp".into()]),
            },
            30,
        )
        .await?;
    Ok(creation.goal_id)
}

#[tokio::test]
async fn goals_invalid_graph_never_replaces_committed_planning_attempt() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let creation = accepted(
        goals
            .execute("local", create("create", workspace.path()), 10)
            .await?,
    )?;
    claim(&goals, &creation.goal_id).await?;
    goals
        .workflow_change(
            actor(),
            &creation.goal_id,
            "request-plan",
            1,
            Change::RequestPlan {
                attempt_id: "attempt".into(),
            },
            20,
        )
        .await?;
    for (id, tasks) in [
        ("cycle", vec![task("a", &["b"]), task("b", &["a"])]),
        ("missing", vec![task("a", &["outside-goal"])]),
        ("duplicate", vec![task("a", &[]), task("a", &[])]),
    ] {
        let result = goals
            .workflow_change(
                actor(),
                &creation.goal_id,
                id,
                1,
                Change::InstallPlan {
                    attempt_id: "attempt".into(),
                    plan: Plan {
                        tasks,
                        completion: completion_spec(),
                    },
                    available_harnesses: BTreeSet::from(["configured-acp".into()]),
                },
                30,
            )
            .await;
        assert!(matches!(result, Err(GoalError::Invalid { .. })));
    }
    let goal = snapshot(&goals, &creation.goal_id).await?;
    assert_eq!(goal.revision, 3);
    assert!(goal.workflow.tasks.is_empty());
    assert!(matches!(goal.workflow.planning, Planning::Requested { .. }));
    assert_eq!(count(&db, "bro_goal_operations").await?, 3);
    Ok(())
}

#[tokio::test]
async fn goals_concurrent_task_admission_persists_only_one_execution() -> Result<()> {
    let db = database().await?;
    let first = service(db.clone());
    let second = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let id = planned(&first, workspace.path()).await?;
    let (a, b) = tokio::join!(
        first.workflow_change(
            actor(),
            &id,
            "launch-a",
            1,
            Change::RequestExecution {
                task_id: "left".into(),
                execution_id: "exec-a".into(),
                workspace: workspace.path().join("a")
            },
            40
        ),
        second.workflow_change(
            actor(),
            &id,
            "launch-b",
            1,
            Change::RequestExecution {
                task_id: "left".into(),
                execution_id: "exec-b".into(),
                workspace: workspace.path().join("b")
            },
            40
        ),
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let rejected = if a.is_err() { a } else { b };
    assert!(matches!(rejected, Err(GoalError::Conflict)));
    let restored = service(db.clone());
    let goal = snapshot(&restored, &id).await?;
    assert_eq!(goal.workflow.tasks[0].executions.len(), 1);
    assert_eq!(count(&db, "bro_goal_operations").await?, 5);
    Ok(())
}

#[tokio::test]
async fn goals_dispatch_receipt_failure_does_not_admit_start_or_run_mapping() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "launch",
            1,
            Change::RequestExecution {
                task_id: "left".into(),
                execution_id: "execution".into(),
                workspace: workspace.path().join("left"),
            },
            40,
        )
        .await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "workspace",
            1,
            Change::RegisterWorkspace {
                execution_id: "execution".into(),
                base_commit: "a".repeat(40),
            },
            50,
        )
        .await?;
    db.execute(Statement::from_string(DatabaseBackend::Sqlite,
        "CREATE TRIGGER reject_dispatch_receipt BEFORE INSERT ON bro_goal_operations WHEN NEW.id = 'dispatch-fails' BEGIN SELECT RAISE(ABORT, 'fixture dispatch persistence failure'); END".to_owned())).await?;
    let result = goals
        .workflow_change(
            actor(),
            &id,
            "dispatch-fails",
            1,
            Change::RequestDispatch {
                execution_id: "execution".into(),
            },
            60,
        )
        .await;
    assert!(matches!(result, Err(GoalError::Storage { .. })));
    assert!(!snapshot(&goals, &id).await?.workflow.tasks[0].executions[0].dispatch_requested);
    let forged = goals
        .workflow_change(
            actor(),
            &id,
            "mapping-without-dispatch",
            1,
            Change::ObserveExecution {
                execution_id: "execution".into(),
                observation: Observation::Running {
                    run_id: "unadmitted-run".into(),
                    native_session_id: None,
                },
            },
            70,
        )
        .await;
    assert!(matches!(forged, Err(GoalError::Invalid { .. })));
    Ok(())
}

#[tokio::test]
async fn goals_only_known_pre_dispatch_failures_have_bounded_automatic_retries() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    for attempt in 0..3 {
        let execution_id = format!("execution-{attempt}");
        goals
            .workflow_change(
                actor(),
                &id,
                &format!("launch-{attempt}"),
                1,
                Change::RequestExecution {
                    task_id: "left".into(),
                    execution_id: execution_id.clone(),
                    workspace: workspace.path().join(&execution_id),
                },
                40,
            )
            .await?;
        goals
            .workflow_change(
                actor(),
                &id,
                &format!("failed-{attempt}"),
                1,
                Change::ObserveExecution {
                    execution_id,
                    observation: Observation::Failed {
                        run_id: None,
                        reason: "Workspace preparation failed before any ACP dispatch".into(),
                    },
                },
                50,
            )
            .await?;
    }
    let exhausted = goals
        .workflow_change(
            actor(),
            &id,
            "fourth-attempt",
            1,
            Change::RequestExecution {
                task_id: "left".into(),
                execution_id: "fourth".into(),
                workspace: workspace.path().join("fourth"),
            },
            60,
        )
        .await;
    assert!(matches!(exhausted, Err(GoalError::Conflict)));
    goals
        .workflow_change(
            actor(),
            &id,
            "right-launch",
            1,
            Change::RequestExecution {
                task_id: "right".into(),
                execution_id: "right-execution".into(),
                workspace: workspace.path().join("right"),
            },
            70,
        )
        .await?;
    admit_dispatch(&goals, &id, "right-execution").await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "right-unknown",
            1,
            Change::ObserveExecution {
                execution_id: "right-execution".into(),
                observation: Observation::Unknown {
                    run_id: None,
                    reason: "ACP start may have occurred; mapping is unknown".into(),
                },
            },
            80,
        )
        .await?;
    let unsafe_retry = goals
        .workflow_change(
            actor(),
            &id,
            "right-replay",
            1,
            Change::RequestExecution {
                task_id: "right".into(),
                execution_id: "unsafe-retry".into(),
                workspace: workspace.path().join("unsafe"),
            },
            90,
        )
        .await;
    assert!(matches!(unsafe_retry, Err(GoalError::Conflict)));
    Ok(())
}

#[tokio::test]
async fn goals_pause_and_unverified_candidate_cannot_admit_dependents() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "launch",
            1,
            Change::RequestExecution {
                task_id: "left".into(),
                execution_id: "execution".into(),
                workspace: workspace.path().join("left"),
            },
            40,
        )
        .await?;
    admit_dispatch(&goals, &id, "execution").await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "result",
            1,
            Change::ObserveExecution {
                execution_id: "execution".into(),
                observation: Observation::Candidate {
                    run_id: "actual-run".into(),
                    native_session_id: Some("harness-session".into()),
                    report: report(
                        workspace.path(),
                        "Worker reports done; verification has not run",
                    )?,
                },
            },
            50,
        )
        .await?;
    let dependent = goals
        .workflow_change(
            actor(),
            &id,
            "dependent",
            1,
            Change::RequestExecution {
                task_id: "integrate".into(),
                execution_id: "dependent-execution".into(),
                workspace: workspace.path().join("integrate"),
            },
            60,
        )
        .await;
    assert!(matches!(dependent, Err(GoalError::Conflict)));
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
            70,
        )
        .await?;
    let right = goals
        .workflow_change(
            actor(),
            &id,
            "right-launch",
            1,
            Change::RequestExecution {
                task_id: "right".into(),
                execution_id: "right-execution".into(),
                workspace: workspace.path().join("right"),
            },
            80,
        )
        .await;
    assert!(matches!(right, Err(GoalError::Conflict)));
    assert!(
        snapshot(&goals, &id).await?.workflow.tasks[1]
            .executions
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn goals_late_execution_facts_keep_original_requirements_version() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "launch",
            1,
            Change::RequestExecution {
                task_id: "left".into(),
                execution_id: "execution".into(),
                workspace: workspace.path().join("left"),
            },
            40,
        )
        .await?;
    admit_dispatch(&goals, &id, "execution").await?;
    // Simulate a concurrently committed new requirement through the real store.
    let mut head = snapshot(&goals, &id).await?;
    let expected = head.revision;
    head.revision += 1;
    head.requirements_revision = 2;
    head.objective = "New requirements for the same Goal".into();
    let operation = GoalOperation {
        id: "new-requirement".into(),
        fingerprint: "fixture-concurrent-update".into(),
        goal_id: id.clone(),
        revision: head.revision,
        requirements_revision: 2,
        accepted_at_ms: 50,
        kind: OperationKind::Input {
            text: head.objective.clone(),
            task_reference: None,
        },
    };
    DatabaseGoalStore::new(db)
        .commit(Some(expected), head, operation)
        .await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "late-result",
            1,
            Change::ObserveExecution {
                execution_id: "execution".into(),
                observation: Observation::Candidate {
                    run_id: "actual-run".into(),
                    native_session_id: None,
                    report: report(workspace.path(), "Result from requirement revision one")?,
                },
            },
            60,
        )
        .await?;
    let restored = snapshot(&goals, &id).await?;
    assert_eq!(restored.requirements_revision, 2);
    assert_eq!(restored.workflow.tasks[0].requirements_revision, 1);
    assert!(matches!(
        restored.workflow.tasks[0].executions[0].observation,
        Observation::Candidate { .. }
    ));
    Ok(())
}

fn create(key: &str, workspace: &std::path::Path) -> GoalCommand {
    GoalCommand::Create {
        operation_id: key.into(),
        title: "Verify login expiry".into(),
        workspace: workspace.into(),
        objective: "Fix expiry without changing the public API".into(),
        budget_limits: Default::default(),
    }
}

fn accepted(reply: GoalReply) -> Result<GoalOperation> {
    match reply {
        GoalReply::Accepted { operation } => Ok(operation),
        _ => anyhow::bail!("expected an admission receipt"),
    }
}

async fn snapshot(service: &GoalService, id: &str) -> Result<GoalSnapshot> {
    match service
        .execute("local", GoalCommand::Get { goal_id: id.into() }, 0)
        .await?
    {
        GoalReply::Snapshot { goal } => Ok(*goal),
        _ => anyhow::bail!("expected a Goal snapshot"),
    }
}

async fn count(db: &DatabaseConnection, table: &str) -> Result<i64> {
    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            format!("SELECT COUNT(*) AS count FROM {table}"),
        ))
        .await?
        .context("missing count row")?;
    Ok(row.try_get("", "count")?)
}

#[tokio::test]
async fn goals_lost_response_replays_receipt_without_changing_later_state() -> Result<()> {
    let db = database().await?;
    let workspace = tempfile::tempdir()?;
    let first_service = service(db.clone());
    let creation = accepted(
        first_service
            .execute("local", create("create-1", workspace.path()), 10)
            .await?,
    )?;
    let input = GoalCommand::Input {
        operation_id: "input-1".into(),
        goal_id: creation.goal_id.clone(),
        expected_revision: 1,
        text: "Why did Safari fail?".into(),
        task_reference: None,
    };
    let original = accepted(first_service.execute("local", input.clone(), 20).await?)?;
    first_service
        .execute(
            "local",
            GoalCommand::Control {
                operation_id: "pause-1".into(),
                goal_id: creation.goal_id.clone(),
                expected_revision: 2,
                intent: RunIntent::Pause,
            },
            30,
        )
        .await?;
    drop(first_service);
    let restored = service(db.clone());
    let replay = accepted(restored.execute("local", input, 40).await?)?;
    assert_eq!(original, replay);
    let goal = snapshot(&restored, &creation.goal_id).await?;
    assert_eq!(goal.revision, 3);
    assert_eq!(goal.requirements_revision, 1);
    assert_eq!(goal.run_intent, RunIntent::Pause);
    assert_eq!(goal.objective, "Fix expiry without changing the public API");
    assert_eq!(count(&db, "bro_goal_operations").await?, 3);
    Ok(())
}

#[tokio::test]
async fn goals_competing_create_identity_does_not_leave_orphan_heads() -> Result<()> {
    let db = database().await?;
    let a = service(db.clone());
    let b = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let command = create("same-create", workspace.path());
    let (first, second) = tokio::join!(
        a.execute("local", command.clone(), 10),
        b.execute("local", command, 20),
    );
    assert_eq!(accepted(first?)?, accepted(second?)?);
    assert_eq!(count(&db, "bro_goals").await?, 1);
    assert_eq!(count(&db, "bro_goal_operations").await?, 1);
    Ok(())
}

#[tokio::test]
async fn goals_operation_identity_cannot_be_reused_for_different_input() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let creation = accepted(
        goals
            .execute("local", create("create", workspace.path()), 10)
            .await?,
    )?;
    let control = GoalCommand::Control {
        operation_id: "control".into(),
        goal_id: creation.goal_id.clone(),
        expected_revision: 1,
        intent: RunIntent::Pause,
    };
    goals.execute("local", control, 20).await?;
    let result = goals
        .execute(
            "local",
            GoalCommand::Control {
                operation_id: "control".into(),
                goal_id: creation.goal_id.clone(),
                expected_revision: 2,
                intent: RunIntent::Run,
            },
            30,
        )
        .await;
    assert!(matches!(result, Err(GoalError::OperationConflict)));
    assert_eq!(
        snapshot(&goals, &creation.goal_id).await?.run_intent,
        RunIntent::Pause
    );
    assert_eq!(count(&db, "bro_goal_operations").await?, 2);
    Ok(())
}

#[tokio::test]
async fn goals_failed_receipt_insert_rolls_back_head_and_does_not_accept_input() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let creation = accepted(
        goals
            .execute("local", create("create", workspace.path()), 10)
            .await?,
    )?;
    db.execute(Statement::from_string(DatabaseBackend::Sqlite,
        "CREATE TRIGGER reject_goal_receipt BEFORE INSERT ON bro_goal_operations WHEN NEW.id = 'fail-input' BEGIN SELECT RAISE(ABORT, 'fixture persistence failure'); END".to_owned(),
    )).await?;
    let result = goals
        .execute(
            "local",
            GoalCommand::Input {
                operation_id: "fail-input".into(),
                goal_id: creation.goal_id.clone(),
                expected_revision: 1,
                text: "Change the API".into(),
                task_reference: None,
            },
            20,
        )
        .await;
    assert!(matches!(result, Err(GoalError::Storage { .. })));
    assert_eq!(snapshot(&goals, &creation.goal_id).await?.revision, 1);
    assert_eq!(count(&db, "bro_goal_operations").await?, 1);
    let store = DatabaseGoalStore::new(db);
    assert!(store.operation("local", "fail-input").await?.is_none());
    Ok(())
}

#[tokio::test]
async fn goals_revision_conflict_does_not_overwrite_concurrent_control() -> Result<()> {
    let db = database().await?;
    let a = service(db.clone());
    let b = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let creation = accepted(
        a.execute("local", create("create", workspace.path()), 10)
            .await?,
    )?;
    let control = |id: &str, intent| GoalCommand::Control {
        operation_id: id.into(),
        goal_id: creation.goal_id.clone(),
        expected_revision: 1,
        intent,
    };
    let (first, second) = tokio::join!(
        a.execute("local", control("pause", RunIntent::Pause), 20),
        b.execute("local", control("stop", RunIntent::Stop), 20),
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    let rejected = if first.is_err() { first } else { second };
    assert!(matches!(rejected, Err(GoalError::Conflict)));
    let goal = snapshot(&a, &creation.goal_id).await?;
    assert_eq!(goal.revision, 2);
    assert!(matches!(
        goal.run_intent,
        RunIntent::Pause | RunIntent::Stop
    ));
    assert_eq!(count(&db, "bro_goal_operations").await?, 2);
    Ok(())
}

#[tokio::test]
async fn goals_cold_queries_are_owner_scoped_and_membership_cutoff_is_stable() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let first = accepted(
        goals
            .execute("local", create("a", workspace.path()), 10)
            .await?,
    )?;
    goals
        .execute("other", create("private", workspace.path()), 20)
        .await?;
    goals
        .execute("local", create("b", workspace.path()), 30)
        .await?;
    let store = DatabaseGoalStore::new(db.clone());
    assert!(matches!(
        store.get("other", &first.goal_id).await,
        Err(GoalError::NotFound)
    ));
    assert!(store.operation("other", "a").await?.is_none());
    let page = store.list("local", 0, None, 1).await?;
    assert_eq!(page.goals.len(), 1);
    assert_eq!(page.goals[0].id, first.goal_id);
    let cursor = page.next_after.context("expected another directory page")?;
    goals
        .execute("local", create("c", workspace.path()), 40)
        .await?;
    let before = count(&db, "bro_goal_operations").await?;
    let restored = DatabaseGoalStore::new(db.clone());
    let page2 = restored.list("local", cursor, Some(page.cutoff), 1).await?;
    assert_eq!(page2.goals.len(), 1);
    assert!(page2.next_after.is_none());
    let history = restored
        .history("local", &first.goal_id, 0, None, 16)
        .await?;
    assert_eq!(history.operations.len(), 1);
    assert_eq!(history.operations[0].kind, OperationKind::Created);
    assert_eq!(count(&db, "bro_goal_operations").await?, before);
    Ok(())
}
