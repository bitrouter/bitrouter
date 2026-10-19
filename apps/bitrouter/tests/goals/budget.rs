//! Shared reservation authority using real transactions, not provider proof.
use super::*;
use bitrouter_orchestrator::goal::accounting::{Event, Work, WorkKind};
use bitrouter_orchestrator::goal::budget::{Event as BudgetEvent, Limits};
use bitrouter_sdk::language_model::native_accounting::NativeTokenReservation;

#[tokio::test]
async fn creation_commits_limits_before_any_coordinator_work_can_be_admitted() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let mut command = create("bounded-create", workspace.path());
    if let GoalCommand::Create { budget_limits, .. } = &mut command {
        *budget_limits = Limits {
            elapsed_ms: Some(1_000),
            managed_token_estimates_micro_usd: Some(0),
        };
    }
    let receipt = accepted(goals.execute("local", command.clone(), 100).await?)?;
    let head = snapshot(&service(db), &receipt.goal_id).await?;
    assert_eq!(head.budget.limits.elapsed_ms, Some(1_000));
    assert_eq!(
        head.budget.limits.managed_token_estimates_micro_usd,
        Some(0)
    );
    assert!(head.budget.admission(head.created_at_ms, 100).is_ok());
    assert!(head.budget.admission(head.created_at_ms, 1_100).is_err());
    claim(&goals, &head.id).await?;
    assert!(matches!(
        goals
            .workflow_change(
                actor(),
                &head.id,
                "paid-work",
                1,
                begin(workspace.path(), "paid", Some(1))?,
                100
            )
            .await,
        Err(GoalError::BudgetExceeded { .. })
    ));
    assert!(
        snapshot(&goals, &head.id)
            .await?
            .accounting
            .in_flight
            .is_empty()
    );
    assert_eq!(
        accepted(goals.execute("local", command.clone(), 200).await?)?,
        receipt
    );
    if let GoalCommand::Create { budget_limits, .. } = &mut command {
        budget_limits.elapsed_ms = None;
    }
    assert!(matches!(
        goals.execute("local", command, 200).await,
        Err(GoalError::OperationConflict)
    ));
    Ok(())
}

async fn limits(goals: &GoalService, id: &str, operation: &str, value: Limits) -> Result<()> {
    let head = snapshot(goals, id).await?;
    goals
        .execute(
            "local",
            GoalCommand::SetBudget {
                operation_id: operation.into(),
                goal_id: id.into(),
                expected_revision: head.revision,
                limits: value,
            },
            100,
        )
        .await?;
    Ok(())
}

fn quote(amount: u64) -> NativeTokenReservation {
    NativeTokenReservation {
        micro_usd: amount,
        pricing_version: "fixture-tariff".into(),
        provider: "provider".into(),
        model: "model".into(),
        input_tokens_bound: 100,
        output_tokens_bound: 100,
        evidence: serde_json::json!({"source":"fixture"}),
    }
}

fn begin(workspace: &std::path::Path, id: &str, amount: Option<u64>) -> Result<Change> {
    Ok(Change::Accounting {
        event: Event::Begin {
            work: Work {
                id: id.into(),
                kind: WorkKind::ProviderAttempt,
                requirements_revision: 1,
                request_id: Some(id.into()),
                intent: report(workspace, id)?,
                token_reservation: amount.map(quote),
            },
        },
    })
}

async fn observe(
    goals: &GoalService,
    id: &str,
    workspace: &std::path::Path,
    work: &str,
    amount: Option<u64>,
    verified: bool,
) -> Result<()> {
    goals
        .workflow_change(
            actor(),
            id,
            &format!("{work}-observed"),
            1,
            Change::Accounting {
                event: Event::Observe {
                    id: work.into(),
                    report: report(workspace, &format!("{work} outcome"))?,
                    estimated_model_tokens_micro_usd: amount,
                    reservation_verified: verified,
                },
            },
            150,
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn competing_reservations_cannot_exceed_one_goal_cap() -> Result<()> {
    let db = database().await?;
    let first = service(db.clone());
    let second = service(db);
    let workspace = tempfile::tempdir()?;
    let id = planned(&first, workspace.path()).await?;
    limits(
        &first,
        &id,
        "set-budget",
        Limits {
            elapsed_ms: None,
            managed_token_estimates_micro_usd: Some(100),
        },
    )
    .await?;
    let a = begin(workspace.path(), "a", Some(70))?;
    let b = begin(workspace.path(), "b", Some(70))?;
    let (a, b) = tokio::join!(
        first.workflow_change(actor(), &id, "a-reserve", 1, a, 120),
        second.workflow_change(actor(), &id, "b-reserve", 1, b, 120),
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert!(matches!(
        if a.is_err() { a } else { b },
        Err(GoalError::BudgetExceeded { .. })
    ));
    let head = snapshot(&first, &id).await?;
    assert_eq!(head.budget.reserved_micro_usd()?, 70);
    assert_eq!(head.accounting.provider_attempts, 1);
    let rejected = first
        .workflow_change(
            actor(),
            &id,
            "unquoted",
            1,
            begin(workspace.path(), "unknown", None)?,
            130,
        )
        .await;
    assert!(matches!(rejected, Err(GoalError::BudgetExceeded { .. })));
    Ok(())
}

#[tokio::test]
async fn unknown_usage_retains_its_full_reservation_across_restart() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    limits(
        &goals,
        &id,
        "set-budget",
        Limits {
            elapsed_ms: None,
            managed_token_estimates_micro_usd: Some(100),
        },
    )
    .await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "reserve",
            1,
            begin(workspace.path(), "unknown", Some(80))?,
            120,
        )
        .await?;
    observe(&goals, &id, workspace.path(), "unknown", None, false).await?;
    let restored = service(db);
    let head = snapshot(&restored, &id).await?;
    assert!(head.accounting.in_flight.is_empty());
    assert_eq!(head.budget.reserved_micro_usd()?, 80);
    assert_eq!(head.accounting.estimated_model_tokens_micro_usd, 0);
    assert!(matches!(
        restored
            .workflow_change(
                actor(),
                &id,
                "new-reserve",
                1,
                begin(workspace.path(), "new", Some(30))?,
                160
            )
            .await,
        Err(GoalError::BudgetExceeded { .. })
    ));
    Ok(())
}

#[tokio::test]
async fn verified_usage_releases_unused_reservation_but_deviations_seal_admission() -> Result<()> {
    let goals = service(database().await?);
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    limits(
        &goals,
        &id,
        "set-budget",
        Limits {
            elapsed_ms: None,
            managed_token_estimates_micro_usd: Some(100),
        },
    )
    .await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "reserve-a",
            1,
            begin(workspace.path(), "a", Some(80))?,
            120,
        )
        .await?;
    observe(&goals, &id, workspace.path(), "a", Some(20), true).await?;
    assert_eq!(snapshot(&goals, &id).await?.budget.reserved_micro_usd()?, 0);
    goals
        .workflow_change(
            actor(),
            &id,
            "reserve-b",
            1,
            begin(workspace.path(), "b", Some(80))?,
            160,
        )
        .await?;
    observe(&goals, &id, workspace.path(), "b", Some(90), false).await?;
    let head = snapshot(&goals, &id).await?;
    assert_eq!(head.accounting.estimated_model_tokens_micro_usd, 110);
    assert!(head.budget.resume_required);
    assert!(head.budget.halt.is_some());
    assert!(matches!(
        goals
            .workflow_change(
                actor(),
                &id,
                "new",
                1,
                begin(workspace.path(), "new", Some(0))?,
                170
            )
            .await,
        Err(GoalError::BudgetExceeded { .. })
    ));
    limits(
        &goals,
        &id,
        "increase",
        Limits {
            elapsed_ms: None,
            managed_token_estimates_micro_usd: Some(200),
        },
    )
    .await?;
    assert!(snapshot(&goals, &id).await?.budget.resume_required);
    let head = snapshot(&goals, &id).await?;
    goals
        .execute(
            "local",
            GoalCommand::Control {
                operation_id: "resume".into(),
                goal_id: id.clone(),
                expected_revision: head.revision,
                intent: RunIntent::Run,
            },
            180,
        )
        .await?;
    assert!(!snapshot(&goals, &id).await?.budget.resume_required);
    assert_eq!(
        snapshot(&goals, &id)
            .await?
            .accounting
            .estimated_model_tokens_micro_usd,
        110
    );
    Ok(())
}

#[tokio::test]
async fn elapsed_limit_includes_waits_and_cannot_reset_with_clock_or_restart() -> Result<()> {
    let db = database().await?;
    let goals = service(db.clone());
    let workspace = tempfile::tempdir()?;
    let id = planned(&goals, workspace.path()).await?;
    limits(
        &goals,
        &id,
        "set-budget",
        Limits {
            elapsed_ms: Some(1000),
            managed_token_estimates_micro_usd: None,
        },
    )
    .await?;
    goals
        .workflow_change(
            actor(),
            &id,
            "elapsed",
            1,
            Change::Budget {
                event: BudgetEvent::ObserveElapsed,
            },
            2000,
        )
        .await?;
    let head = snapshot(&service(db), &id).await?;
    assert_eq!(head.budget.elapsed_ms, 1990);
    assert_eq!(head.run_intent, RunIntent::Run);
    assert!(head.budget.resume_required);
    assert!(matches!(
        goals
            .workflow_change(
                actor(),
                &id,
                "after-clock-moved-back",
                1,
                begin(workspace.path(), "new", None)?,
                120
            )
            .await,
        Err(GoalError::BudgetExceeded { .. })
    ));
    Ok(())
}
