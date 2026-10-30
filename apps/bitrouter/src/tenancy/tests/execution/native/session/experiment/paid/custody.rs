//! Dropping the public waiter cannot drop accepted work or replace its source.
use super::*;

#[tokio::test]
async fn abandoned_native_paid_waiter_keeps_real_request_source_pin_and_human_head() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(90), Box::pin(async {
        let subject = upstream().await?;
        let mut judge = Judge::start().await?;
        let directory = tempfile::tempdir()?;
        let url = format!("{}/v1", subject.uri());
        let (fixture, headers) = ready(directory.path(), &url, &judge.url).await?;
        let mut session = None;
        let checked = async {
            session = Some(open(&fixture, &headers, &url, directory.path(), "abandoned-paid-waiter").await?);
            let current = session.as_mut().context("original native host")?;
            current.run(human::observed_task(), Arc::new(Approval), CancellationToken::new()).await?;
            let before = current.review_run().await?;
            let frozen = freeze(&fixture, &headers).await?;
            judge.answer(&before)?;
            {
                let paid = current.assess_run(&before.scoring.evidence.checkpoint_id, None);
                tokio::pin!(paid);
                let request = tokio::select! {
                    result = &mut paid => { result?; anyhow::bail!("paid waiter completed before held provider request"); }
                    request = judge.received() => request?,
                };
                ensure!(request["model"] == "judge-model");
            }
            // The genuine request is held at its socket. The public future has
            // gone away, but the same host must still own the accepted source.
            let pending = records(&fixture, "owned_judge_pending").await?;
            ensure!(pending.len() == 1 && pending[0]["history_reservation"]["state"]["state"] == "held");
            ensure!(current.run(human::observed_task(), Arc::new(Approval), CancellationToken::new()).await.is_err(),
                "losing the paid waiter released its immutable source pin");
            let human = current.submit_assessment(human::label(&before, "human-after-paid-waiter-loss", false)?).await?;
            let held = current.review_run().await?;
            ensure!(held.run_id == before.run_id && held.head == before.head
                && held.scoring.expected_revision.as_deref() == Some(human.revision.revision_id.as_str()));
            ensure!(subject.received_requests().await.context("subject requests")?.len() == 1);
            judge.release.cancel();
            tokio::time::timeout(TIMEOUT, async {
                loop {
                    if records(&fixture, "owned_judge_pending").await?.is_empty() {
                        break Ok::<_, anyhow::Error>(());
                    }
                    tokio::task::yield_now().await;
                }
            }).await??;
            let after = current.review_run().await?;
            ensure!(after.run_id == before.run_id && after.history.len() == 2
                && after.scoring.expected_revision.as_deref() == Some(human.revision.revision_id.as_str()));
            let paid = after.history.iter().find(|row| row.input.source == AssessmentSource::Agentic).context("actual paid history")?;
            ensure!(!paid.selected_on_submission && paid.selection_reason == "head_changed");
            let jobs = records(&fixture, "owned_judge_job").await?;
            let budgets = records(&fixture, "owned_judge_budget").await?;
            ensure!(jobs.len() == 1 && jobs[0]["status"] == "unselected"
                && budgets.len() == 1 && budgets[0]["accepted"] == 1);
            let execution = jobs[0]["execution"].as_str().context("actual paid execution")?.try_into()?;
            let receipt = crate::tenancy::receipts::store::lookup(&fixture.db, &fixture.installation, &fixture.tenant, &execution).await?.context("actual terminal receipt")?;
            ensure!(receipt.state == bitrouter_ai::tenancy::execution::ReceiptState::Committed
                && receipt.usage.input_tokens == Some(4) && receipt.usage.output_tokens == Some(2));
            ensure!(freeze(&fixture, &headers).await? == frozen && !judge.extra_request());
            ensure!(fixture.service.current().is_ok());
            Ok::<_, anyhow::Error>(())
        }.await;
        finish(&fixture, session, judge, checked).await
    })).await?
}
