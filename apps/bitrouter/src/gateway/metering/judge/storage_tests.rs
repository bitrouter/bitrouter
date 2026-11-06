use super::*;
use anyhow::Context;
use sea_orm::ConnectionTrait;
use std::sync::OnceLock;

#[tokio::test]
async fn judge_settlement_uses_shared_budget_and_fences_failed_terminal_write() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let costs = JudgeCosts::new(db.clone());
    let attempt = JudgeAttempt {
        version: VERSION.into(),
        request_id: "brjudge_storage".into(),
        job_id: "job".into(),
        identity: SessionIdentity {
            owner: "local".into(),
            source: "fixture".into(),
            native_session_id: "session".into(),
        },
        checkpoint_id: "checkpoint".into(),
        model: "fixture:judge".into(),
        reserved_at: "2026-07-30T00:00:00Z".into(),
        attempt_number: 1,
        started_at: Some("2026-07-30T00:00:01Z".into()),
        settled_at: None,
        hops: vec![],
        observed_usage_origin: None,
        outcome: None,
    };
    costs
        .store("local")?
        .initialize(KIND, &attempt.request_id, None, &attempt)
        .await?;
    let observed = JudgeAttemptObserved {
        request_id: attempt.request_id.clone(),
        owner: "local".into(),
        hops: Arc::new(Mutex::new(vec![])),
    };
    let cell = Arc::new(OnceLock::new());
    let costs = costs.with_storage_budget(Some(cell.clone()));
    assert!(
        costs
            .finish(
                &observed,
                Some(AttemptSettlement {
                    hops: vec![],
                    observed_usage_origin: None
                }),
                None
            )
            .await
            .is_err()
    );
    let tenancy = crate::tenancy::Service::open(db.clone(), None).await?;
    cell.set(tenancy.storage_budget())
        .map_err(|_| anyhow::anyhow!("cell already set"))?;
    costs
        .finish(
            &observed,
            Some(AttemptSettlement {
                hops: vec![],
                observed_usage_origin: None,
            }),
            None,
        )
        .await?;
    let (_, settled) = costs
        .store("local")?
        .get::<JudgeAttempt>(KIND, &attempt.request_id)
        .await?
        .context("attempt missing")?;
    assert!(settled.settled_at.is_some());
    db.execute_unprepared("CREATE TRIGGER reject_judge_terminal BEFORE UPDATE ON checkpoint_evolution BEGIN SELECT RAISE(ABORT, 'fixture storage failure'); END").await?;
    assert!(
        costs
            .finish(&observed, None, Some(ExecutionOutcome::Completed))
            .await
            .is_err()
    );
    let (_, saved) = costs
        .store("local")?
        .get::<JudgeAttempt>(KIND, &attempt.request_id)
        .await?
        .context("attempt missing")?;
    assert!(saved.outcome.is_none());
    assert!(matches!(
        tenancy.storage_budget().reserve(0, false),
        Err(bitrouter_ai::tenancy::operation::ErrorCode::StorageUnavailable)
    ));
    tenancy.shutdown().await?;
    Ok(())
}
