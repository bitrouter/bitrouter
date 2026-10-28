//! Actual public native paid consumer: no seeded quality, receipt, proof or ACP row.
use super::*;
use crate::acp_trajectory::checkpoint::types::{AssessmentSource, RevisionInput};
mod authority;
mod custody;
mod fixture;
mod recovery;
mod revision_budget;
use fixture::{Judge, ready, restart};

async fn finish(
    fixture: &Fixture,
    session: Option<NativeSession>,
    judge: Judge,
    result: Result<()>,
) -> Result<()> {
    judge.release.cancel();
    let closed = match session {
        Some(session) => tokio::time::timeout(TIMEOUT, session.close())
            .await
            .map_err(anyhow::Error::from)
            .and_then(|v| v.map_err(Into::into)),
        None => Ok(()),
    };
    let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
    let judged = judge.finish().await;
    result
        .and(closed)
        .and(stopped.map_err(anyhow::Error::from).and_then(|v| v))
        .and(judged)
}

#[tokio::test]
async fn actual_native_paid_revision_is_conclusive_replay_charges_once_and_next_job_is_bounded()
-> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(90), Box::pin(async {
        let subject = upstream().await?;
        let mut judge = Judge::start().await?;
        let directory = tempfile::tempdir()?;
        let url = format!("{}/v1", subject.uri());
        let (fixture, headers) = ready(directory.path(), &url, &judge.url).await?;
        let mut session = None;
        let checked = async {
            session = Some(open(&fixture, &headers, &url, directory.path(), "actual-paid").await?);
            let current = session.as_mut().context("native owner")?;
            current.run(human::observed_task(), Arc::new(Approval), CancellationToken::new()).await?;
            let review = current.review_run().await?;
            let frozen = freeze(&fixture, &headers).await?;
            judge.answer(&review)?; judge.release.cancel();
            current.assess_run(&review.scoring.evidence.checkpoint_id, None).await?;
            let request = judge.received().await?;
            ensure!(request["model"] == "judge-model");
            let wire = serde_json::to_string(&request)?;
            ensure!(wire.contains("recorded-native-quality-evidence-v1") && !wire.contains("sessionId")
                && !wire.contains("owned-upstream-secret") && !wire.contains("explicit-judge-secret"));
            let paid = current.review_run().await?;
            let revision = paid.history.first().context("actual paid revision")?;
            ensure!(paid.history.len() == 1 && revision.input.source == AssessmentSource::Agentic
                && revision.input.evaluator_version == crate::core::evolution::judge::NATIVE_JUDGE_VERSION);
            let report = current.experiment_cohort(MODEL, &frozen.evidence_root).await?;
            ensure!(report.challenger.conclusive == 1 && report.challenger.pass == 1);
            ensure!(freeze(&fixture, &headers).await? == frozen, "paid quality rewrote operational facts");
            current.assess_run(&review.scoring.evidence.checkpoint_id, None).await?;
            ensure!(!judge.extra_request(), "accepted logical replay sent another paid request");
            let withdrawn = current.retract_assessment(RevisionInput {
                submission_id: "human-retract-paid".into(), checkpoint_id: review.scoring.evidence.checkpoint_id.clone(),
                expected_revision: Some(revision.revision_id.clone()), source: AssessmentSource::Human,
                evaluator_id: String::new(), evaluator_version: String::new(), assessment: None,
                reason: "Human withdraws this assessment while the immutable paid budget remains spent".into(),
            }).await?;
            // The assessment head is a selection CAS, not part of the paid
            // input identity. The same evidence/evaluator is still one job.
            current.assess_run(&review.scoring.evidence.checkpoint_id, Some(&withdrawn.revision_id)).await?;
            let repeated = current.review_run().await?;
            ensure!(repeated.history.len() == 2
                && repeated.scoring.expected_revision.as_deref() == Some(withdrawn.revision_id.as_str())
                && repeated.previous.is_none());
            // A new actual run supplies a distinct source and therefore a new
            // logical job. The immutable revision's one-job budget is spent.
            let mut next = human::observed_task();
            let limits = next.limits.as_mut().context("second actual run limits")?;
            limits.checkpoint_bytes = 256 * 1024;
            limits.unacknowledged_bytes = 512 * 1024;
            let state = current.run(next, Arc::new(Approval), CancellationToken::new()).await?;
            ensure!(state.run.as_ref().context("second actual run")?.status
                == crate::storage::checkpoint::snapshot::RunStatus::Completed);
            let second = current.review_run().await?;
            ensure!(second.run_id != review.run_id && second.history.is_empty()
                && second.scoring.evidence.checkpoint_id != review.scoring.evidence.checkpoint_id);
            let refused = current.assess_run(&second.scoring.evidence.checkpoint_id, None).await
                .err().context("second logical job exceeded immutable budget")?;
            ensure!(refused.code == bitrouter_ai::core::protocol::ErrorCode::LimitExceeded
                && refused.commit_status == bitrouter_ai::core::protocol::CommitStatus::NotCommitted);
            ensure!(current.review_run().await?.history.is_empty());
            ensure!(!judge.extra_request());
            let budgets = records(&fixture, "owned_judge_budget").await?;
            let jobs = records(&fixture, "owned_judge_job").await?;
            ensure!(budgets.len() == 1 && budgets[0]["accepted"] == 1 && jobs.len() == 1);
            ensure!(jobs[0]["version"] == 4 && jobs[0]["recovery"]["seed"]["native"].is_object()
                && jobs[0]["recovery"]["seed"].get("descriptor").is_none()
                && jobs[0]["native_evidence"]["projection_version"] == "recorded-native-quality-evidence-v1");
            let execution = jobs[0]["execution"].as_str().context("paid execution")?.try_into()?;
            let receipt = crate::tenancy::receipts::store::lookup(&fixture.db, &fixture.installation, &fixture.tenant, &execution).await?.context("actual035 terminal")?;
            ensure!(receipt.state == bitrouter_ai::tenancy::execution::ReceiptState::Committed
                && receipt.usage.input_tokens == Some(4) && receipt.usage.output_tokens == Some(2));
            ensure!(fixture.service.current().is_ok() && subject.received_requests().await.context("subject requests")?.len() == 2);
            Ok::<_, anyhow::Error>(())
        }.await;
        finish(&fixture, session, judge, checked).await
    })).await?
}

#[tokio::test]
async fn actual_held_native_judge_cannot_overwrite_new_human_head() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(90), Box::pin(async {
        let subject = upstream().await?;
        let mut judge = Judge::start().await?;
        let directory = tempfile::tempdir()?;
        let url = format!("{}/v1", subject.uri());
        let (fixture, headers) = ready(directory.path(), &url, &judge.url).await?;
        let mut session = None;
        let checked = async {
            session = Some(open(&fixture, &headers, &url, directory.path(), "human-paid-race").await?);
            let current = session.as_mut().context("native owner")?;
            current.run(human::observed_task(), Arc::new(Approval), CancellationToken::new()).await?;
            let review = current.review_run().await?;
            judge.answer(&review)?;
            let mut paid = Box::pin(current.assess_run(&review.scoring.evidence.checkpoint_id, None));
            let request = tokio::select! {
                result = &mut paid => { result?; anyhow::bail!("judge returned before held actual request"); }
                request = judge.received() => request?,
            };
            ensure!(request["model"] == "judge-model");
            let human = current.submit_assessment(human::label(&review, "real-human-during-judge", false)?).await?;
            let pending = records(&fixture, "owned_judge_pending").await?;
            ensure!(pending.len() == 1 && pending[0]["history_reservation"]["state"]["state"] == "held");
            let visible = current.review_run().await?;
            ensure!(visible.scoring.expected_revision.as_deref() == Some(human.revision.revision_id.as_str()));
            judge.release.cancel(); paid.await?;
            let final_view = current.review_run().await?;
            ensure!(final_view.history.len() == 2 && final_view.scoring.expected_revision.as_deref() == Some(human.revision.revision_id.as_str()));
            let paid_revision = final_view.history.iter().find(|r| r.input.source == AssessmentSource::Agentic).context("paid history retained")?;
            ensure!(!paid_revision.selected_on_submission && paid_revision.selection_reason == "head_changed");
            let jobs = records(&fixture, "owned_judge_job").await?;
            ensure!(jobs.len() == 1 && jobs[0]["status"] == "unselected" && jobs[0]["output"]["usage"]["prompt_tokens"] == 4);
            let frozen = freeze(&fixture, &headers).await?;
            let report = current.experiment_cohort(MODEL, &frozen.evidence_root).await?;
            ensure!(report.challenger.conclusive == 1 && report.challenger.fail == 1 && report.challenger.pass == 0);
            ensure!(records(&fixture, "owned_judge_pending").await?.is_empty() && fixture.service.current().is_ok());
            Ok::<_, anyhow::Error>(())
        }.await;
        finish(&fixture, session, judge, checked).await
    })).await?
}

#[tokio::test]
async fn actual_native_paid_terminal_survives_later_run_and_file_bootstrap_without_resend()
-> Result<()> {
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        Box::pin(async {
            let subject = upstream().await?;
            let mut judge = Judge::start().await?;
            let directory = tempfile::tempdir()?;
            let url = format!("{}/v1", subject.uri());
            let (mut fixture, headers) = ready(directory.path(), &url, &judge.url).await?;
            let mut session = None;
            let checked = async {
                session = Some(
                    open(
                        &fixture,
                        &headers,
                        &url,
                        directory.path(),
                        "paid-old-source",
                    )
                    .await?,
                );
                let current = session.as_mut().context("native owner")?;
                current
                    .run(
                        human::observed_task(),
                        Arc::new(Approval),
                        CancellationToken::new(),
                    )
                    .await?;
                let original = current.review_run().await?;
                judge.answer(&original)?;
                judge.release.cancel();
                current
                    .assess_run(&original.scoring.evidence.checkpoint_id, None)
                    .await?;
                judge.received().await?;
                let mut next = human::observed_task();
                let limits = next.limits.as_mut().context("explicit second run limits")?;
                limits.checkpoint_bytes = 256 * 1024;
                limits.unacknowledged_bytes = 512 * 1024;
                current
                    .run(next, Arc::new(Approval), CancellationToken::new())
                    .await?;
                let later = current.review_run().await?;
                ensure!(later.run_id != original.run_id && later.history.is_empty());
                ensure!(
                    current
                        .assess_run(&original.scoring.evidence.checkpoint_id, None)
                        .await
                        .is_err()
                );
                ensure!(fixture.service.current().is_ok());
                Ok::<_, anyhow::Error>(())
            }
            .await;
            judge.release.cancel();
            let closed = match session.take() {
                Some(session) => tokio::time::timeout(TIMEOUT, session.close())
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|v| v.map_err(Into::into)),
                None => Ok(()),
            };
            if let Err(error) = checked.and(closed) {
                return finish(&fixture, None, judge, Err(error)).await;
            }
            let prior = async {
                Ok::<_, anyhow::Error>((
                    records(&fixture, "owned_judge_job").await?,
                    records(&fixture, "owned_judge_budget").await?,
                ))
            }
            .await;
            let (prior_jobs, prior_counter) = match prior {
                Ok(prior) => prior,
                Err(error) => return finish(&fixture, None, judge, Err(error)).await,
            };
            fixture = match restart(fixture, directory.path()).await {
                Ok(fixture) => fixture,
                Err(error) => return Err::<(), _>(error).and(judge.finish().await),
            };
            let checked = async {
                ensure!(
                    records(&fixture, "owned_judge_job").await? == prior_jobs
                        && records(&fixture, "owned_judge_budget").await? == prior_counter
                        && records(&fixture, "owned_judge_pending").await?.is_empty()
                );
                ensure!(
                    !judge.extra_request()
                        && subject
                            .received_requests()
                            .await
                            .context("subject requests")?
                            .len()
                            == 2
                );
                ensure!(fixture.service.current().is_ok());
                Ok::<_, anyhow::Error>(())
            }
            .await;
            finish(&fixture, None, judge, checked).await
        }),
    )
    .await?
}
