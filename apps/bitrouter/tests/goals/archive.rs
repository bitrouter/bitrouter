//! Real SQLite retirement/fencing. Execution facts are domain fixtures, not harness proof.
use super::*;
use bitrouter_orchestrator::goal::input::{Event as InputEvent, Interpretation, Stage, TaskUpdate};
use bitrouter_orchestrator::goal::workflow::{
    Execution, LocalReconciliation, archive_operation_id,
};

async fn change(goals: &GoalService, id: &str, key: &str, change: Change) -> Result<GoalOperation> {
    let head = snapshot(goals, id).await?;
    Ok(goals
        .workflow_change(actor(), id, key, head.requirements_revision, change, 200)
        .await?)
}

async fn start(
    goals: &GoalService,
    id: &str,
    execution_id: &str,
    workspace: &std::path::Path,
) -> Result<()> {
    change(
        goals,
        id,
        &format!("{execution_id}-start"),
        Change::RequestExecution {
            task_id: "left".into(),
            execution_id: execution_id.into(),
            workspace: workspace.join(execution_id),
        },
    )
    .await?;
    Ok(())
}

async fn settle_and_edit(
    goals: &GoalService,
    id: &str,
    execution_id: &str,
    workspace: &std::path::Path,
) -> Result<()> {
    for (suffix, mutation) in [
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
        (
            "candidate",
            Change::ObserveExecution {
                execution_id: execution_id.into(),
                observation: Observation::Candidate {
                    run_id: format!("domain-{execution_id}"),
                    native_session_id: Some(format!("session-{execution_id}")),
                    report: report(
                        workspace,
                        "domain fixture candidate; not actual harness work",
                    )?,
                },
            },
        ),
        (
            "stop",
            Change::RequestStop {
                execution_id: execution_id.into(),
                operation_id: format!("{execution_id}-stop"),
            },
        ),
        (
            "stopped",
            Change::ObserveStop {
                execution_id: execution_id.into(),
                operation_id: format!("{execution_id}-stop"),
                confirmed: true,
                detail: format!(
                    "domain fixture stopped {execution_id}: {}",
                    "detail ".repeat(7000)
                ),
            },
        ),
    ] {
        change(goals, id, &format!("{execution_id}-{suffix}"), mutation).await?;
    }
    let head = snapshot(goals, id).await?;
    let operation = accepted(
        goals
            .execute(
                "local",
                GoalCommand::Input {
                    operation_id: format!("{execution_id}-edit"),
                    goal_id: id.into(),
                    expected_revision: head.revision,
                    text: "Revise the left task while preserving the original Goal".into(),
                    task_reference: Some("left".into()),
                },
                200,
            )
            .await?,
    )?;
    let attempt = operation.id.clone();
    change(
        goals,
        id,
        &format!("{execution_id}-input-begin"),
        Change::Input {
            event: InputEvent::Begin {
                operation: Box::new(operation),
                attempt_id: attempt.clone(),
                stage: Stage::Reasoning,
            },
        },
    )
    .await?;
    change(
        goals,
        id,
        &format!("{execution_id}-input-finish"),
        Change::Input {
            event: InputEvent::Finish {
                attempt_id: attempt,
                interpretation: Interpretation {
                    task_updates: vec![TaskUpdate {
                        task_id: "left".into(),
                        instruction: format!(
                            "Original work with the next {execution_id} requirement"
                        ),
                        resources: None,
                    }],
                    ..Default::default()
                },
                reasoning_request_id: Some(format!("domain-{execution_id}-reasoning-request")),
            },
        },
    )
    .await?;
    let head = snapshot(goals, id).await?;
    let task = head
        .workflow
        .tasks
        .iter()
        .find(|task| task.spec.id == "left")
        .context("left task missing")?;
    change(
        goals,
        id,
        &format!("{execution_id}-reconciled"),
        Change::ReconcileStoppedExecution {
            execution_id: execution_id.into(),
            reconciliation: LocalReconciliation::Verified {
                task_revision: task.requirements_revision,
                control_source_revision: head
                    .control_source_revision
                    .max(task.control_source_revision),
                workspace: report(workspace, "domain fixture stopped workspace")?,
                transcript: report(
                    workspace,
                    "domain fixture ended tools; runtime proof is separate",
                )?,
            },
        },
    )
    .await?;
    Ok(())
}

async fn archive_change(
    goals: &GoalService,
    id: &str,
    execution_id: &str,
    workspace: &std::path::Path,
) -> Result<Change> {
    let head = snapshot(goals, id).await?;
    let task = head
        .workflow
        .tasks
        .iter()
        .find(|task| task.spec.id == "left")
        .context("left task missing")?;
    let execution = task
        .executions
        .iter()
        .find(|execution| execution.id == execution_id)
        .context("execution missing")?
        .clone();
    let decisions = head
        .workflow
        .decisions
        .iter()
        .filter(|decision| decision.execution_id == execution_id)
        .cloned()
        .collect::<Vec<_>>();
    let record = report(
        workspace,
        &serde_json::to_string(&serde_json::json!({
            "goal_id": head.id, "owner": head.owner, "task_id": task.spec.id,
            "execution": execution, "decisions": decisions, "previous": task.latest_archive,
        }))?,
    )?;
    Ok(Change::ArchiveExecution {
        task_id: "left".into(),
        execution: Box::new(execution),
        decisions,
        record,
    })
}

async fn pair(goals: &GoalService, workspace: &std::path::Path) -> Result<String> {
    let id = planned(goals, workspace).await?;
    start(goals, &id, "original", workspace).await?;
    settle_and_edit(goals, &id, "original", workspace).await?;
    start(goals, &id, "replacement", workspace).await?;
    Ok(id)
}

#[tokio::test]
async fn execution_archive_is_atomic_idempotent_cold_and_owner_scoped() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let url = format!(
        "sqlite://{}?mode=rwc",
        workspace.path().join("goals.db").display()
    );
    let db = bitrouter::db::connect(&url).await?;
    bitrouter::db::run_migrations(&db).await?;
    let goals = service(db.clone());
    let id = pair(&goals, workspace.path()).await?;
    let mut source = snapshot(&goals, &id).await?;
    source
        .workflow
        .decisions
        .push(bitrouter_orchestrator::goal::decision::PermissionDecision {
            id: "retired-permission".into(),
            execution_id: "original".into(),
            run_id: "domain-original".into(),
            permission_id: "old-request".into(),
            requirements_revision: 1,
            task_revision: 1,
            request: report(workspace.path(), "domain fixture original permission")?,
            options: vec![bitrouter_orchestrator::goal::decision::PermissionOption {
                id: "once".into(),
                label: "Allow once".into(),
                allows: true,
                persistent: false,
            }],
            state: bitrouter_orchestrator::goal::decision::PermissionState::Obsolete {
                reason: "superseded domain fixture permission".into(),
            },
        });
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE bro_goals SET payload = ? WHERE owner = ? AND id = ?",
        vec![
            serde_json::to_vec(&source)?.into(),
            "local".into(),
            id.clone().into(),
        ],
    ))
    .await?;
    let accounting_proof = report(
        workspace.path(),
        "domain fixture accounting; no live charge claim",
    )?;
    change(
        &goals,
        &id,
        "accounting-start",
        Change::Accounting {
            event: bitrouter_orchestrator::goal::accounting::Event::Begin {
                work: bitrouter_orchestrator::goal::accounting::Work {
                    id: "recorded-work".into(),
                    kind: bitrouter_orchestrator::goal::accounting::WorkKind::Preparation,
                    requirements_revision: 2,
                    request_id: None,
                    intent: accounting_proof.clone(),
                    token_reservation: None,
                },
            },
        },
    )
    .await?;
    change(
        &goals,
        &id,
        "accounting-observed",
        Change::Accounting {
            event: bitrouter_orchestrator::goal::accounting::Event::Observe {
                id: "recorded-work".into(),
                report: accounting_proof,
                estimated_model_tokens_micro_usd: Some(11),
                reservation_verified: false,
            },
        },
    )
    .await?;
    let head = snapshot(&goals, &id).await?;
    goals
        .execute(
            "local",
            GoalCommand::SetBudget {
                operation_id: "halt-before-archive".into(),
                goal_id: id.clone(),
                expected_revision: head.revision,
                limits: bitrouter_orchestrator::goal::budget::Limits {
                    elapsed_ms: Some(1),
                    managed_token_estimates_micro_usd: None,
                },
            },
            200,
        )
        .await?;
    let before = snapshot(&goals, &id).await?;
    assert!(before.budget.halt.is_some());
    assert_eq!(before.accounting.estimated_model_tokens_micro_usd, 11);
    let mutation = archive_change(&goals, &id, "original", workspace.path()).await?;
    let key = archive_operation_id(&id, "original");
    let mut incomplete = mutation.clone();
    if let Change::ArchiveExecution { decisions, .. } = &mut incomplete {
        decisions.clear();
    }
    assert!(change(&goals, &id, &key, incomplete).await.is_err());
    assert_eq!(snapshot(&goals, &id).await?, before);
    let saved = change(&goals, &id, &key, mutation.clone()).await?;
    let head = snapshot(&goals, &id).await?;
    let task = head
        .workflow
        .tasks
        .iter()
        .find(|task| task.spec.id == "left")
        .context("left task missing")?;
    assert_eq!(task.executions.len(), 1);
    assert_eq!(task.archived_executions, 1);
    assert!(head.workflow.decisions.is_empty());
    assert_eq!(head.budget, before.budget);
    assert_eq!(head.accounting, before.accounting);
    assert_eq!(head.inputs, before.inputs);
    assert_eq!(head.run_intent, before.run_intent);
    let replay = change(&goals, &id, &key, mutation.clone()).await?;
    assert_eq!(replay, saved);
    assert_eq!(snapshot(&goals, &id).await?, head);
    let cold_db = bitrouter::db::connect(&url).await?;
    let cold_store = DatabaseGoalStore::new(cold_db.clone());
    let cold = cold_store
        .operation("local", &key)
        .await?
        .context("cold archive missing")?;
    assert_eq!(cold, saved);
    let OperationKind::Workflow { change: archived } = cold.kind else {
        anyhow::bail!("archive was not a workflow receipt")
    };
    assert_eq!(*archived, mutation);
    assert!(cold_store.operation("another-owner", &key).await?.is_none());
    assert_eq!(snapshot(&service(cold_db), &id).await?, head);
    assert!(matches!(
        goals
            .workflow_change(
                actor(),
                &id,
                "reuse-in-other-task",
                head.requirements_revision,
                Change::RequestExecution {
                    task_id: "right".into(),
                    execution_id: "original".into(),
                    workspace: workspace.path().join("wrong")
                },
                300
            )
            .await,
        Err(GoalError::Invalid { .. })
    ));
    Ok(())
}

#[tokio::test]
async fn archive_keeps_unknown_effects_active_work_permissions_and_delivery_sources_in_head()
-> Result<()> {
    use bitrouter_orchestrator::goal::accounting::{Work, WorkKind};
    use bitrouter_orchestrator::goal::completion::{Attempt, Source, State};
    use bitrouter_orchestrator::goal::decision::{
        PermissionDecision, PermissionOption, PermissionState,
    };
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let id = pair(&goals, workspace.path()).await?;
    let original = snapshot(&goals, &id).await?;
    let proof = report(workspace.path(), "domain reference fixture")?;
    let key = archive_operation_id(&id, "original");
    for case in 0..4 {
        // Inject late/reference metadata into a real persisted head, not effects.
        let mut head = original.clone();
        match case {
            0 => {
                let execution = head
                    .workflow
                    .tasks
                    .iter_mut()
                    .flat_map(|task| &mut task.executions)
                    .find(|execution| execution.id == "original")
                    .context("original execution missing")?;
                execution.stop = Some(StopState::Unknown {
                    operation_id: "unconfirmed-stop".into(),
                    detail: "effect ending unknown".into(),
                });
            }
            1 => head.accounting.in_flight.push(Work {
                id: "old-source-work".into(),
                kind: WorkKind::ProviderAttempt,
                requirements_revision: 1,
                request_id: Some("old-provider-request".into()),
                intent: proof.clone(),
                token_reservation: None,
            }),
            2 => head.workflow.decisions.push(PermissionDecision {
                id: "old-permission".into(),
                execution_id: "original".into(),
                run_id: "domain-original".into(),
                permission_id: "actual-request".into(),
                requirements_revision: 1,
                task_revision: 1,
                request: proof.clone(),
                options: vec![PermissionOption {
                    id: "once".into(),
                    label: "Allow once".into(),
                    allows: true,
                    persistent: false,
                }],
                state: PermissionState::Unknown {
                    operation_id: "permission-send".into(),
                    option_id: "once".into(),
                    reason: "reply transmission unknown".into(),
                },
            }),
            _ => {
                head.workflow.completion.attempt = Some(Attempt {
                    id: "retained-delivery".into(),
                    requirements_revision: 1,
                    sources: vec![Source {
                        task_id: "left".into(),
                        task_revision: 1,
                        execution_id: "original".into(),
                        workspace: proof.clone(),
                        review: proof.clone(),
                    }],
                    workspace: workspace.path().into(),
                    base_commit: None,
                    delivery: None,
                    checks: Vec::new(),
                    assessment: None,
                    accepted: false,
                    state: State::Preparing,
                })
            }
        }
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE bro_goals SET payload = ? WHERE owner = ? AND id = ?",
            vec![
                serde_json::to_vec(&head)?.into(),
                "local".into(),
                id.clone().into(),
            ],
        ))
        .await?;
        let mutation = archive_change(&goals, &id, "original", workspace.path()).await?;
        assert!(
            change(&goals, &id, &key, mutation).await.is_err(),
            "active reference case {case} archived"
        );
        assert_eq!(snapshot(&goals, &id).await?, head);
        assert!(
            DatabaseGoalStore::new(db.clone())
                .operation("local", &key)
                .await?
                .is_none()
        );
    }
    Ok(())
}

#[tokio::test]
async fn goal_history_page_includes_envelope_in_byte_bound_and_advances_without_losing_inputs()
-> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    let head = snapshot(&goals, &id).await?;
    let prototype = accepted(
        goals
            .execute(
                "local",
                GoalCommand::Input {
                    operation_id: "page-000".into(),
                    goal_id: id.clone(),
                    expected_revision: head.revision,
                    text: "X".into(),
                    task_reference: None,
                },
                200,
            )
            .await?,
    )?;
    let overhead = serde_json::to_vec(&prototype)?.len() - 1;
    let text = "X".repeat(bitrouter_orchestrator::goal::MAX_RECORD_BYTES / 16 - overhead - 1);
    for index in 1..=16 {
        let head = snapshot(&goals, &id).await?;
        goals
            .execute(
                "local",
                GoalCommand::Input {
                    operation_id: format!("page-{index:03}"),
                    goal_id: id.clone(),
                    expected_revision: head.revision,
                    text: text.clone(),
                    task_reference: None,
                },
                200,
            )
            .await?;
    }
    let store = DatabaseGoalStore::new(db);
    let mut after = prototype.revision;
    let mut cutoff = None;
    let mut ids = Vec::new();
    loop {
        let page = store.history("local", &id, after, cutoff, 16).await?;
        cutoff = Some(page.cutoff);
        assert!(serde_json::to_vec(&page)?.len() <= bitrouter_orchestrator::goal::MAX_RECORD_BYTES);
        ids.extend(page.operations.into_iter().map(|operation| operation.id));
        let Some(next) = page.next_after else { break };
        assert!(next > after);
        after = next;
    }
    assert_eq!(
        ids,
        (1..=16)
            .map(|index| format!("page-{index:03}"))
            .collect::<Vec<_>>()
    );
    Ok(())
}

#[tokio::test]
async fn archive_receipt_failure_rolls_back_head_and_can_retry_original_identity() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let id = pair(&goals, workspace.path()).await?;
    let before = snapshot(&goals, &id).await?;
    let mutation = archive_change(&goals, &id, "original", workspace.path()).await?;
    let key = archive_operation_id(&id, "original");
    db.execute(Statement::from_string(DatabaseBackend::Sqlite, format!(
        "CREATE TRIGGER reject_archive BEFORE INSERT ON bro_goal_operations WHEN NEW.id = '{key}' BEGIN SELECT RAISE(ABORT, 'fixture archive persistence failure'); END"
    ))).await?;
    assert!(change(&goals, &id, &key, mutation.clone()).await.is_err());
    assert_eq!(snapshot(&goals, &id).await?, before);
    assert!(
        DatabaseGoalStore::new(db.clone())
            .operation("local", &key)
            .await?
            .is_none()
    );
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "DROP TRIGGER reject_archive".to_owned(),
    ))
    .await?;
    change(&goals, &id, &key, mutation).await?;
    assert_eq!(
        snapshot(&goals, &id)
            .await?
            .workflow
            .tasks
            .iter()
            .find(|task| task.spec.id == "left")
            .context("left task missing")?
            .archived_executions,
        1
    );
    Ok(())
}

#[tokio::test]
async fn archive_does_not_reset_current_version_retry_admission() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = pair(&goals, workspace.path()).await?;
    let mutation = archive_change(&goals, &id, "original", workspace.path()).await?;
    change(
        &goals,
        &id,
        &archive_operation_id(&id, "original"),
        mutation,
    )
    .await?;
    for (index, execution_id) in ["replacement", "retry-2", "retry-3"]
        .into_iter()
        .enumerate()
    {
        if index != 0 {
            start(&goals, &id, execution_id, workspace.path()).await?;
        }
        change(
            &goals,
            &id,
            &format!("{execution_id}-no-dispatch"),
            Change::ObserveExecution {
                execution_id: execution_id.into(),
                observation: Observation::Failed {
                    run_id: None,
                    reason: "known pre-dispatch fixture failure".into(),
                },
            },
        )
        .await?;
    }
    assert!(
        start(&goals, &id, "fourth-attempt", workspace.path())
            .await
            .is_err()
    );
    let head = snapshot(&goals, &id).await?;
    let task = head
        .workflow
        .tasks
        .iter()
        .find(|task| task.spec.id == "left")
        .context("left task missing")?;
    assert_eq!(task.archived_executions, 1);
    assert_eq!(task.executions.len(), 3);
    assert!(
        task.executions
            .iter()
            .all(|execution| execution.task_revision == 2)
    );
    Ok(())
}

#[tokio::test]
async fn archived_history_crosses_snapshot_bound_without_resetting_versions_or_budget() -> Result<()>
{
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    start(&goals, &id, "execution-0", workspace.path()).await?;
    let mut originals = Vec::<Execution>::new();
    for index in 0..24 {
        let current = format!("execution-{index}");
        settle_and_edit(&goals, &id, &current, workspace.path()).await?;
        start(
            &goals,
            &id,
            &format!("execution-{}", index + 1),
            workspace.path(),
        )
        .await?;
        let before = snapshot(&goals, &id).await?;
        let mutation = archive_change(&goals, &id, &current, workspace.path()).await?;
        if let Change::ArchiveExecution { execution, .. } = &mutation {
            originals.push(*execution.clone());
        }
        change(&goals, &id, &archive_operation_id(&id, &current), mutation).await?;
        let after = snapshot(&goals, &id).await?;
        assert_eq!(after.requirements_revision, before.requirements_revision);
        assert_eq!(after.budget, before.budget);
        assert_eq!(after.accounting, before.accounting);
        assert!(serde_json::to_vec(&after)?.len() < bitrouter_orchestrator::goal::MAX_RECORD_BYTES);
    }
    let head = snapshot(&goals, &id).await?;
    let task = head
        .workflow
        .tasks
        .iter()
        .find(|task| task.spec.id == "left")
        .context("left task missing")?;
    assert_eq!(task.archived_executions, 24);
    assert_eq!(task.executions.len(), 1);
    assert_eq!(head.requirements_revision, 25);
    let mut after = 0;
    let mut cutoff = None;
    let cold = DatabaseGoalStore::new(db);
    let mut restored = Vec::new();
    let mut archived_bytes = 0;
    loop {
        let page = cold.history("local", &id, after, cutoff, 16).await?;
        cutoff = Some(page.cutoff);
        assert!(serde_json::to_vec(&page)?.len() <= bitrouter_orchestrator::goal::MAX_RECORD_BYTES);
        for operation in page.operations {
            if let OperationKind::Workflow { change } = operation.kind
                && let Change::ArchiveExecution {
                    execution, record, ..
                } = *change
            {
                archived_bytes += serde_json::to_vec(&execution)?.len();
                let body: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&record.path)?)?;
                assert_eq!(
                    serde_json::from_value::<Execution>(body["execution"].clone())?,
                    *execution
                );
                restored.push(*execution);
            }
        }
        let Some(next) = page.next_after else { break };
        assert!(next > after);
        after = next;
    }
    assert!(archived_bytes > bitrouter_orchestrator::goal::MAX_RECORD_BYTES);
    assert_eq!(restored, originals);
    Ok(())
}
