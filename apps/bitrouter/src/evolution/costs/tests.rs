use super::*;
use crate::evolution::jobs::JobStatus;

fn job(owner: &str, id: &str) -> JudgeJob {
    JudgeJob {
        job_id: id.into(),
        identity: SessionIdentity {
            owner: owner.into(),
            source: "fixture".into(),
            native_session_id: id.into(),
        },
        checkpoint_id: format!("checkpoint-{id}"),
        expected_revision: None,
        model: "fixture:judge".into(),
        input_digest: "fixture-input".into(),
        mode_epoch: None,
        status: JobStatus::Failed,
        lease_owner: None,
        lease_until: None,
        request_ids: vec![],
        cached_response: None,
        outcome_revision: None,
        error_code: None,
    }
}

#[tokio::test]
async fn judge_cost_reservations_are_atomic_scoped_and_not_zero_when_unobserved() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let costs = JudgeCosts::new(db.clone());
    let mut local = job("local", "one");
    let foreign = job("another-owner", "two");
    let tx = db.begin().await?;
    costs.reserve(&tx, &local, "brjudge_rolled-back").await?;
    tx.rollback().await?;
    assert!(
        costs
            .store("local")?
            .get::<JudgeAttempt>(KIND, "brjudge_rolled-back")
            .await?
            .is_none()
    );
    let tx = db.begin().await?;
    costs.reserve(&tx, &local, "brjudge_reserved").await?;
    tx.commit().await?;
    local.request_ids = vec![
        "brjudge_reserved".into(),
        "brjudge_reserved".into(),
        "legacy-attempt".into(),
    ];
    let report = costs.report("local", std::slice::from_ref(&local)).await?;
    assert_eq!(report.summary.requests, 2);
    assert_eq!(report.summary.incomplete_requests, 2);
    assert_eq!(report.summary.total_cost_micro_usd, None);
    assert_eq!(report.summary.known_cost_micro_usd, 0);
    assert_eq!(report.retries.requests, 1);
    assert_eq!(report.sessions[0].summary, report.summary);
    assert!(
        report.jobs["one"]
            .attempts
            .iter()
            .find(|a| a.request_id == "brjudge_reserved")
            .context("reserved attempt missing")?
            .incomplete_reasons
            .iter()
            .any(|r| r == "dispatch_not_confirmed")
    );
    assert!(
        report.jobs["one"]
            .attempts
            .iter()
            .find(|a| a.request_id == "legacy-attempt")
            .context("legacy attempt missing")?
            .incomplete_reasons
            .iter()
            .any(|r| r == "dispatch_inventory_missing")
    );
    assert!(
        costs
            .report("local", std::slice::from_ref(&foreign))
            .await
            .is_err()
    );
    let mut conflicting = foreign;
    conflicting.request_ids = vec!["brjudge_reserved".into()];
    assert!(costs.report("another-owner", &[conflicting]).await.is_err());
    local.status = JobStatus::Superseded;
    assert_eq!(
        costs.report("local", &[local]).await?.summary,
        report.summary
    );
    assert_eq!(
        costs
            .report("another-owner", &[])
            .await?
            .summary
            .total_cost_micro_usd,
        Some(0)
    );
    Ok(())
}
