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
#[ignore = "bounded capacity campaign; run from the shared CI archive or capacity workflow"]
async fn native_mixed_arm_campaign_reconciles_every_real_unit_in_one_frozen_cohort() -> Result<()> {
    #[derive(Clone, Copy, Debug)]
    enum Phase {
        Setup,
        Fixture,
        Open,
        Run,
        Reconcile,
        Freeze,
        Cohort,
        Close,
        Requests,
        Counters,
        Shutdown,
    }

    struct Progress {
        phase: Phase,
        phase_started: std::time::Instant,
        index: Option<usize>,
        reconciled: usize,
        closed: usize,
        last_completed_unit_ms: u128,
        prior_failure_phase: Option<Phase>,
    }

    impl Progress {
        fn enter(&mut self, phase: Phase) {
            self.phase = phase;
            self.phase_started = std::time::Instant::now();
        }
    }

    // This record survives cancellation of the aggregate future without querying
    // Core or the actor after expiry. Last-unit time is present only if closed > 0.
    let started = std::time::Instant::now();
    let mut progress = Progress {
        phase: Phase::Setup,
        phase_started: started,
        index: None,
        reconciled: 0,
        closed: 0,
        last_completed_unit_ms: 0,
        prior_failure_phase: None,
    };
    let mut control = 0;
    let mut challenger = 0;
    let result = tokio::time::timeout(std::time::Duration::from_secs(180), async {
        let server = upstream().await.context("mixed-arm upstream setup")?;
        let directory = tempfile::tempdir().context("mixed-arm temporary directory")?;
        let url = format!("{}/v1", server.uri());
        progress.enter(Phase::Fixture);
        let (fixture, headers, _) = fixture(directory.path(), &url, 500_000, 128)
            .await
            .context("mixed-arm fixture setup")?;
        let checked = async {
            for index in 0..128 {
                let unit_started = std::time::Instant::now();
                progress.index = Some(index);
                progress.enter(Phase::Open);
                let mut session = open(
                    &fixture,
                    &headers,
                    &url,
                    directory.path(),
                    &format!("campaign-{index}"),
                )
                .await
                .context("mixed-arm open")?;
                progress.enter(Phase::Run);
                let state = tokio::time::timeout(
                    TIMEOUT,
                    session.run(task(), Arc::new(Approval), CancellationToken::new()),
                )
                .await
                .context("mixed-arm run timeout")?
                .context("mixed-arm run")?;
                let run = state.run.as_ref().context("campaign run")?;
                ensure!(
                    run.status == crate::storage::checkpoint::snapshot::RunStatus::Completed,
                    "campaign run {index} incomplete"
                );
                progress.enter(Phase::Reconcile);
                match chain(&fixture, &server, &run.run_id)
                    .await
                    .context("mixed-arm reconcile")?
                {
                    ExperimentArm::Control => control += 1,
                    ExperimentArm::Challenger => challenger += 1,
                }
                progress.reconciled += 1;
                if index == 127 {
                    ensure!(
                        control > 0 && challenger > 0,
                        "bounded campaign failed to observe both arms"
                    );
                    progress.enter(Phase::Freeze);
                    let snapshot = freeze(&fixture, &headers)
                        .await
                        .context("mixed-arm freeze")?;
                    ensure!(snapshot.entries.len() == 128);
                    progress.enter(Phase::Cohort);
                    let report = session
                        .experiment_cohort(MODEL, &snapshot.evidence_root)
                        .await
                        .context("mixed-arm cohort")?;
                    ensure!(
                        report.control.observed == control
                            && report.challenger.observed == challenger
                    );
                    ensure!(report.verdict == CohortGateVerdict::InsufficientEvidence);
                }
                progress.enter(Phase::Close);
                tokio::time::timeout(TIMEOUT, session.close())
                    .await
                    .context("mixed-arm close timeout")?
                    .context("mixed-arm close")?;
                progress.closed += 1;
                progress.last_completed_unit_ms = unit_started.elapsed().as_millis();
            }
            progress.enter(Phase::Requests);
            ensure!(
                server
                    .received_requests()
                    .await
                    .context("campaign requests")?
                    .len()
                    == 128
            );
            progress.enter(Phase::Counters);
            let counters = records(&fixture, "native_experiment_counter_v1")
                .await
                .context("mixed-arm final counters")?;
            ensure!(
                counters.len() == 1
                    && counters[0]["accepted"] == 128
                    && counters[0]["challenger"] == challenger
            );
            Ok(())
        }
        .await;
        progress.prior_failure_phase = checked.is_err().then_some(progress.phase);
        progress.enter(Phase::Shutdown);
        let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
        checked?;
        stopped
            .context("mixed-arm shutdown timeout")?
            .context("mixed-arm shutdown")?;
        Ok(())
    })
    .await;
    if !matches!(&result, Ok(Ok(()))) {
        // An outer timeout may have dropped the body before or during shutdown;
        // this summary is observation only and does not establish a clean drain.
        eprintln!(
            "mixed-arm campaign progress: boundary={}, phase={:?}, index={:?}, reconciled={}, closed={}, control={}, challenger={}, last_completed_unit_ms={}, phase_elapsed_ms={}, total_elapsed_ms={}, prior_failure_phase={:?}",
            if result.is_err() {
                "Aggregate"
            } else {
                "Inner"
            },
            progress.phase,
            progress.index,
            progress.reconciled,
            progress.closed,
            control,
            challenger,
            progress.last_completed_unit_ms,
            progress.phase_started.elapsed().as_millis(),
            started.elapsed().as_millis(),
            progress.prior_failure_phase,
        );
    }
    result.context("mixed-arm aggregate campaign timeout (180s)")?
}
