//! Full assembled-pipeline cost tests use controlled HTTP responses, not models.

use super::*;
use crate::evolution::costs::JudgeCosts;
use crate::evolution::jobs::{JobStatus, JudgeJob, JudgeJobs};
use crate::evolution::operator::{EvolutionOperation, EvolutionReport};
use crate::evolution::{scoring, store::EvolutionStore};
use crate::metering::entities::requests;
use sea_orm::{ActiveModelTrait, EntityTrait, IntoActiveModel, Set};

async fn setup_job(fixture: &Fixture, id: &str, model: &str) -> Result<(JudgeJobs, JudgeJob)> {
    let identity = session(fixture, id, "fixture").await?;
    let canonical = CanonicalStore::new(fixture.assembled.db.clone());
    let head = canonical.transcript(&identity).await?.session.head;
    let checkpoint = canonical.freeze_checkpoint(&identity, head).await?;
    let jobs = JudgeJobs::new(EvolutionStore::new(fixture.assembled.db.clone(), "local")?);
    let job = jobs
        .enqueue(&identity, &checkpoint.checkpoint_id, model, None)
        .await?;
    Ok((jobs, job))
}

async fn status(fixture: &Fixture) -> Result<crate::evolution::operator::EvolutionStatus> {
    let EvolutionReport::Status(status) = fixture
        .assembled
        .evolution
        .operate("local", EvolutionOperation::Status)
        .await?
    else {
        anyhow::bail!("expected evolution status")
    };
    Ok(*status)
}

#[tokio::test]
async fn judge_costs_count_invalid_responses_retries_and_retired_jobs_once() -> Result<()> {
    let fixture = fixture(false).await?;
    let (jobs, job) = setup_job(&fixture, "cost-retries", "fixture:strong").await?;
    let canonical = CanonicalStore::new(fixture.assembled.db.clone());
    let head = canonical.transcript(&job.identity).await?.session.head;
    let pipeline = fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?;
    for _ in 0..3 {
        assert!(jobs.run(&job.job_id, pipeline).await.is_err());
    }
    assert!(jobs.run(&job.job_id, pipeline).await.is_err());
    let failed = jobs.get(&job.job_id).await?;
    assert_eq!(failed.status, JobStatus::Failed);
    assert_eq!(failed.request_ids.len(), 3);
    let before = status(&fixture).await?;
    assert_eq!(before.judge_costs.summary.total_cost_micro_usd, Some(84));
    assert_eq!(before.judge_costs.retries.total_cost_micro_usd, Some(56));
    assert_eq!(
        before.judge_costs.sessions[0].summary,
        before.judge_costs.summary
    );
    assert_eq!(before.judge_costs.jobs[&job.job_id].summary.requests, 3);
    for id in &failed.request_ids {
        let row = requests::Entity::find_by_id(id)
            .one(&fixture.assembled.db)
            .await?
            .context("metering missing")?;
        assert!(row.acp_session_id.is_none());
        assert!(row.controller_instance_id.is_none());
    }
    assert_eq!(
        canonical.transcript(&job.identity).await?.session.head,
        head
    );
    assert!(
        canonical
            .assessment_history(&job.identity)
            .await?
            .is_empty()
    );
    assert!(
        fixture
            .assembled
            .evolution
            .executions(&job.identity)
            .await?
            .is_empty()
    );
    jobs.supersede(&job.job_id, "fixture_retired").await?;
    fixture
        .assembled
        .evolution
        .service("local")?
        .set_mode(EvolutionMode::Off, None)
        .await?;
    assert_eq!(
        status(&fixture).await?.judge_costs.summary,
        before.judge_costs.summary
    );
    let restarted = JudgeCosts::new(fixture.assembled.db.clone());
    assert_eq!(
        restarted
            .report("local", &jobs.list().await?)
            .await?
            .summary,
        before.judge_costs.summary
    );
    canonical.delete(&job.identity).await?;
    let deleted = status(&fixture).await?;
    assert!(deleted.jobs.is_empty());
    assert_eq!(deleted.judge_costs.summary, before.judge_costs.summary);
    assert_eq!(
        deleted.judge_costs.without_job_details,
        deleted.judge_costs.summary
    );
    assert!(!deleted.judge_costs.jobs[&job.job_id].job_details_available);
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("requests missing")?
            .len(),
        3
    );
    Ok(())
}

#[tokio::test]
async fn judge_costs_preserve_unknown_fallback_and_prove_zero_before_dispatch() -> Result<()> {
    let fixture = fixture(false).await?;
    let pipeline = fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?;
    let (jobs, fallback) = setup_job(&fixture, "cost-fallback", "fallback").await?;
    assert!(jobs.run(&fallback.job_id, pipeline).await.is_err());
    let (jobs, rejected) = setup_job(&fixture, "cost-rejected", "missing-route").await?;
    assert!(jobs.run(&rejected.job_id, pipeline).await.is_err());
    let report = status(&fixture).await?.judge_costs;
    let fallback = &report.jobs[&fallback.job_id];
    assert_eq!(fallback.summary.known_cost_micro_usd, 14);
    assert_eq!(fallback.summary.total_cost_micro_usd, None);
    assert_eq!(fallback.attempts[0].upstream_attempts, Some(2));
    assert!(
        fallback.attempts[0]
            .incomplete_reasons
            .iter()
            .any(|r| r == "earlier_upstream_attempt_costs_missing")
    );
    let rejected = &report.jobs[&rejected.job_id];
    assert_eq!(rejected.summary.total_cost_micro_usd, Some(0));
    assert_eq!(rejected.attempts[0].upstream_attempts, Some(0));
    assert_eq!(report.summary.known_cost_micro_usd, 14);
    assert_eq!(report.summary.total_cost_micro_usd, None);
    Ok(())
}

#[tokio::test]
async fn judge_costs_reuse_cached_response_and_refresh_authoritative_receipts() -> Result<()> {
    use crate::cloud::settlement::{SettlementReceipt, SettlementState, SettlementUsage};
    use crate::metering::store::MeteringStore;
    let fixture = fixture(false).await?;
    let (jobs, job) = setup_job(&fixture, "cost-success", "fixture:strong").await?;
    let canonical = CanonicalStore::new(fixture.assembled.db.clone());
    let input = scoring::prepare(&canonical, &job.identity, &job.checkpoint_id).await?;
    let evaluation = crate::evolution::tests::unknown(&input.evidence);
    // Match the SDK's existing chat-completions test endpoint and response shape.
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"judge-fixture", "object":"chat.completion", "model":"strong",
            "choices":[{"index":0,"message":{"role":"assistant","content":serde_json::to_string(&evaluation)?},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}
        }))).with_priority(1).mount(&fixture.upstream).await;
    let pipeline = fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?;
    let completed = jobs.run(&job.job_id, pipeline).await?;
    assert_eq!(completed.status, JobStatus::Completed);
    jobs.run(&job.job_id, pipeline).await?;
    let id = &completed.request_ids[0];
    assert_eq!(
        status(&fixture)
            .await?
            .judge_costs
            .summary
            .total_cost_micro_usd,
        Some(28)
    );
    let row = requests::Entity::find_by_id(id)
        .one(&fixture.assembled.db)
        .await?
        .context("metering missing")?;
    let mut replay = request(&job.identity, id, "fixture:strong")?;
    replay.headers.clear();
    replay.prompt.tool_choice = Some(bitrouter_sdk::language_model::ToolChoice::None);
    assert!(pipeline.execute(replay.clone()).await.is_err());
    replay.caller = CallerContext::anonymous();
    assert!(pipeline.execute(replay).await.is_err());
    assert_eq!(
        requests::Entity::find_by_id(id)
            .one(&fixture.assembled.db)
            .await?
            .as_ref(),
        Some(&row)
    );
    let mut pending = row.into_active_model();
    pending.reconciliation_status = Set("pending".into());
    pending.update(&fixture.assembled.db).await?;
    let waiting = status(&fixture).await?.judge_costs;
    assert_eq!(waiting.summary.known_cost_micro_usd, 28);
    assert_eq!(waiting.summary.total_cost_micro_usd, None);
    let meter = MeteringStore::new(fixture.assembled.db.clone());
    meter
        .apply_authoritative_receipt_charge(&SettlementReceipt {
            request_id: id.clone(),
            state: SettlementState::Computed,
            provider_id: Some("resolved-provider".into()),
            model_id: Some("resolved-model".into()),
            usage: SettlementUsage {
                uncached_input_tokens: 10,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 2,
                reasoning_tokens: 0,
            },
            final_charge_micro_usd: Some(45),
        })
        .await?;
    let reconciled = status(&fixture).await?.judge_costs;
    assert_eq!(reconciled.summary.total_cost_micro_usd, Some(45));
    assert_eq!(reconciled.summary.requests, 1);
    assert_eq!(canonical.assessment_history(&job.identity).await?.len(), 1);
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("requests missing")?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn judge_costs_distinguish_missing_usage_from_confirmed_no_charge() -> Result<()> {
    use crate::cloud::settlement::{SettlementReceipt, SettlementState, SettlementUsage};
    use crate::metering::store::MeteringStore;
    let fixture = fixture(false).await?;
    let (jobs, job) = setup_job(&fixture, "cost-unknown", "fixture:strong").await?;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"unknown-usage", "object":"chat.completion", "model":"strong",
            "choices":[{"index":0,"message":{"role":"assistant","content":"invalid rubric"},"finish_reason":"stop"}]
        }))).with_priority(1).mount(&fixture.upstream).await;
    let pipeline = fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?;
    assert!(jobs.run(&job.job_id, pipeline).await.is_err());
    let unknown = status(&fixture).await?.judge_costs;
    assert_eq!(unknown.summary.known_cost_micro_usd, 0);
    assert_eq!(unknown.summary.total_cost_micro_usd, None);
    assert_eq!(unknown.summary.incomplete_requests, 1);
    let id = &jobs.get(&job.job_id).await?.request_ids[0];
    let row = requests::Entity::find_by_id(id)
        .one(&fixture.assembled.db)
        .await?
        .context("metering missing")?;
    assert_eq!(row.estimated_charge_micro_usd, 0);
    let mut pending = row.into_active_model();
    pending.reconciliation_status = Set("pending".into());
    pending.update(&fixture.assembled.db).await?;
    MeteringStore::new(fixture.assembled.db.clone())
        .apply_authoritative_receipt_charge(&SettlementReceipt {
            request_id: id.clone(),
            state: SettlementState::NotCharged,
            provider_id: None,
            model_id: None,
            usage: SettlementUsage {
                uncached_input_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 0,
                reasoning_tokens: 0,
            },
            final_charge_micro_usd: None,
        })
        .await?;
    let confirmed = status(&fixture).await?.judge_costs;
    assert_eq!(confirmed.summary.total_cost_micro_usd, Some(0));
    assert_eq!(confirmed.summary.incomplete_requests, 0);
    Ok(())
}

#[tokio::test]
async fn judge_costs_keep_cancelled_attempts_unknown_after_worker_restart() -> Result<()> {
    let fixture = fixture(false).await?;
    let (jobs, job) = setup_job(&fixture, "cost-cancelled", "fixture:strong").await?;
    let started = Arc::new(tokio::sync::Notify::new());
    let observed = started.clone();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |_: &wiremock::Request| {
            observed.notify_one();
            ResponseTemplate::new(503).set_delay(std::time::Duration::from_secs(10))
        })
        .with_priority(1)
        .mount(&fixture.upstream)
        .await;
    let pipeline = fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?
        .clone();
    let running_jobs = jobs.clone();
    let job_id = job.job_id.clone();
    let running = tokio::spawn(async move { running_jobs.run(&job_id, &pipeline).await });
    tokio::time::timeout(std::time::Duration::from_secs(3), started.notified()).await?;
    running.abort();
    assert!(running.await.is_err());
    let abandoned = jobs.get(&job.job_id).await?;
    assert_eq!(abandoned.request_ids.len(), 1);
    let after = JudgeCosts::new(fixture.assembled.db.clone())
        .report("local", &[abandoned])
        .await?;
    assert_eq!(after.summary.requests, 1);
    assert_eq!(after.summary.total_cost_micro_usd, None);
    assert_eq!(after.summary.incomplete_requests, 1);
    assert!(after.jobs[&job.job_id].attempts[0].started_at.is_some());
    Ok(())
}

#[tokio::test]
async fn judge_costs_do_not_promote_normalized_rejection_usage_to_observed_zero() -> Result<()> {
    let fixture = fixture(false).await?;
    let (jobs, job) = setup_job(&fixture, "cost-rate-limited", "fixture:strong").await?;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error":{"message":"fixture rate limit", "type":"rate_limit_error"}
        })))
        .with_priority(1)
        .mount(&fixture.upstream)
        .await;
    let pipeline = fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?;
    assert!(jobs.run(&job.job_id, pipeline).await.is_err());
    let report = status(&fixture).await?.judge_costs;
    assert_eq!(
        report.summary.total_cost_micro_usd, None,
        "locally inferred zero usage is not provider-observed cost evidence"
    );
    assert_eq!(report.summary.incomplete_requests, 1);
    assert!(
        report.jobs[&job.job_id].attempts[0]
            .incomplete_reasons
            .iter()
            .any(|reason| reason == "pipeline_usage_not_observed")
    );
    Ok(())
}
