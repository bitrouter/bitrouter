use super::*;

#[tokio::test]
async fn native_challenger_cap_refuses_next_actual_run_before_provider_and_keeps_old_root()
-> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(45), async {
        let server = upstream().await?;
        let directory = tempfile::tempdir()?;
        let url = format!("{}/v1", server.uri());
        let (fixture, headers, _) = fixture(directory.path(), &url, 1_000_000, 1).await?;
        let checked = async {
            let mut session = open(&fixture, &headers, &url, directory.path(), "cap").await?;
            let state = session
                .run(task(), Arc::new(Approval), CancellationToken::new())
                .await?;
            ensure!(
                chain(
                    &fixture,
                    &server,
                    &state.run.as_ref().context("run")?.run_id
                )
                .await?
                    == ExperimentArm::Challenger
            );
            let snapshot = freeze(&fixture, &headers).await?;
            let prior = session
                .experiment_cohort(MODEL, &snapshot.evidence_root)
                .await?;
            let outcome = session
                .run(task(), Arc::new(Approval), CancellationToken::new())
                .await;
            // Core may durably record a rejected step instead of returning a
            // facade error. The socket, counter and full ACK are the boundary.
            if let Ok(state) = outcome {
                ensure!(state.run.as_ref().is_some_and(
                    |r| r.status != crate::storage::checkpoint::snapshot::RunStatus::Running
                ));
            }
            drained(&fixture).await?;
            ensure!(server.received_requests().await.context("requests")?.len() == 1);
            let counters = records(&fixture, "native_experiment_counter_v1").await?;
            ensure!(
                counters.len() == 1
                    && counters[0]["accepted"] == 1
                    && counters[0]["challenger"] == 1
            );
            ensure!(
                session
                    .experiment_cohort(MODEL, &snapshot.evidence_root)
                    .await?
                    == prior
            );
            tokio::time::timeout(TIMEOUT, session.close()).await??;
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

#[test]
fn zero_exposure_remains_an_invalid_signed_experiment() -> Result<()> {
    ensure!(fixture::signed_experiment("selected", 0, 1).is_err());
    Ok(())
}
