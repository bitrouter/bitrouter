use super::*;

#[tokio::test]
async fn actual_native_assignment_receipt_ack_freeze_and_cohort_survive_key_rotation() -> Result<()>
{
    tokio::time::timeout(std::time::Duration::from_secs(45), async {
        let server = upstream().await?;
        let directory = tempfile::tempdir()?;
        let url = format!("{}/v1", server.uri());
        let (fixture, headers, _) = fixture(directory.path(), &url, 1_000_000, 4).await?;
        let checked = async {
            let mut session = open(&fixture, &headers, &url, directory.path(), "root").await?;
            let state = tokio::time::timeout(
                TIMEOUT,
                session.run(task(), Arc::new(Approval), CancellationToken::new()),
            )
            .await??;
            let run = state.run.as_ref().context("run")?;
            ensure!(run.status == crate::storage::checkpoint::snapshot::RunStatus::Completed);
            ensure!(chain(&fixture, &server, &run.run_id).await? == ExperimentArm::Challenger);
            let snapshot = freeze(&fixture, &headers).await?;
            ensure!(snapshot.entries.len() == 1);
            let report = session
                .experiment_cohort(MODEL, &snapshot.evidence_root)
                .await?;
            ensure!(report.challenger.observed == 1 && report.control.observed == 0);
            ensure!(
                report.challenger.conclusive == 0
                    && report.verdict == CohortGateVerdict::InsufficientEvidence
            );
            let session_id = state.session_id;
            tokio::time::timeout(TIMEOUT, session.close()).await??;
            let rotated = fixture.issue("native-experiment-rotated").await?;
            let revision = headers
                .get("bitrouter-execution-revision")
                .context("revision")?
                .to_str()?
                .try_into()?;
            let next = crate::tenancy::tests::execution::headers(
                &fixture,
                rotated.secret.as_deref().context("rotated secret")?,
                &revision,
            )?;
            let restored = open(&fixture, &next, &url, directory.path(), "root").await?;
            ensure!(restored.core().snapshot().await.session_id == session_id);
            ensure!(
                restored
                    .experiment_cohort(MODEL, &snapshot.evidence_root)
                    .await?
                    == report
            );
            ensure!(
                server.received_requests().await.context("requests")?.len() == 1,
                "read/reopen resent paid work"
            );
            tokio::time::timeout(TIMEOUT, restored.close()).await??;
            ensure!(
                records(&fixture, "native_experiment_unit_pending_v1")
                    .await?
                    .is_empty()
            );
            ensure!(
                records(&fixture, "native_experiment_step_pending_v1")
                    .await?
                    .is_empty()
            );
            Ok(())
        }
        .await;
        let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
        checked?;
        stopped??;
        Ok(())
    })
    .await?
}

// Fixed campaign: every one of the 128 actual runs must complete, even after
// both arms first appear. No caller chooses IDs, redraws or replaces a failure.
#[tokio::test]
async fn native_mixed_arm_campaign_reconciles_every_real_unit_in_one_frozen_cohort() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(180), async {
        let server = upstream().await?;
        let directory = tempfile::tempdir()?;
        let url = format!("{}/v1", server.uri());
        let (fixture, headers, _) = fixture(directory.path(), &url, 500_000, 128).await?;
        let checked = async {
            let mut control = 0;
            let mut challenger = 0;
            for index in 0..128 {
                let mut session = open(
                    &fixture,
                    &headers,
                    &url,
                    directory.path(),
                    &format!("campaign-{index}"),
                )
                .await?;
                let state = tokio::time::timeout(
                    TIMEOUT,
                    session.run(task(), Arc::new(Approval), CancellationToken::new()),
                )
                .await??;
                let run = state.run.as_ref().context("campaign run")?;
                ensure!(
                    run.status == crate::storage::checkpoint::snapshot::RunStatus::Completed,
                    "campaign run {index} incomplete"
                );
                match chain(&fixture, &server, &run.run_id).await? {
                    ExperimentArm::Control => control += 1,
                    ExperimentArm::Challenger => challenger += 1,
                }
                if index == 127 {
                    ensure!(
                        control > 0 && challenger > 0,
                        "bounded campaign failed to observe both arms"
                    );
                    let snapshot = freeze(&fixture, &headers).await?;
                    ensure!(snapshot.entries.len() == 128);
                    let report = session
                        .experiment_cohort(MODEL, &snapshot.evidence_root)
                        .await?;
                    ensure!(
                        report.control.observed == control
                            && report.challenger.observed == challenger
                    );
                    ensure!(report.verdict == CohortGateVerdict::InsufficientEvidence);
                }
                tokio::time::timeout(TIMEOUT, session.close()).await??;
            }
            ensure!(
                server
                    .received_requests()
                    .await
                    .context("campaign requests")?
                    .len()
                    == 128
            );
            let counters = records(&fixture, "native_experiment_counter_v1").await?;
            ensure!(
                counters.len() == 1
                    && counters[0]["accepted"] == 128
                    && counters[0]["challenger"] == challenger
            );
            Ok(())
        }
        .await;
        let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
        checked?;
        stopped??;
        Ok(())
    })
    .await?
}
