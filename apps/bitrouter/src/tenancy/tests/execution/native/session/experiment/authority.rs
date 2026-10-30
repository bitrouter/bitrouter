use super::*;
use crate::tenancy::transaction::{Disposition, Phase};

#[tokio::test]
async fn cancelled_actual_cohort_read_keeps_native_command_and_actor_read_until_rollback()
-> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(45), async {
        let server = upstream().await?; let directory = tempfile::tempdir()?;
        let url = format!("{}/v1", server.uri());
        let (fixture, headers, _) = fixture(directory.path(), &url, 1_000_000, 4).await?;
        let checked = async {
            let mut session = open(&fixture, &headers, &url, directory.path(), "cohort-cancel").await?;
            session.run(task(), Arc::new(Approval), CancellationToken::new()).await?;
            drained(&fixture).await?;
            let snapshot = freeze(&fixture, &headers).await?;
            let permits = fixture.service.reads.available_permits();
            let gate = fixture.service.cleanup.hold(Phase::Active);
            {
                let read = session.experiment_cohort(MODEL, &snapshot.evidence_root);
                tokio::pin!(read);
                tokio::select! {
                    result = &mut read => anyhow::bail!("cohort bypassed active transaction gate: {result:?}"),
                    _ = gate.entered.notified() => {},
                }
            }
            ensure!(fixture.service.cleanup.disposition() == Disposition::Pending);
            ensure!(fixture.service.reads.available_permits() + 1 == permits);
            ensure!(session.experiment_cohort(MODEL, &snapshot.evidence_root).await.is_err(), "cancelled waiter released the native command owner");
            fixture.service.cleanup.release_gate();
            // A real protected actor operation is a serialization barrier.
            let same = freeze(&fixture, &headers).await?;
            ensure!(same.evidence_root == snapshot.evidence_root);
            ensure!(fixture.service.reads.available_permits() == permits);
            let report = session.experiment_cohort(MODEL, &snapshot.evidence_root).await?;
            ensure!(report.challenger.observed == 1);
            ensure!(fixture.service.cleanup.disposition() == Disposition::RolledBack);
            tokio::time::timeout(TIMEOUT, session.close()).await??;
            ensure!(server.received_requests().await.context("requests")?.len() == 1);
            Ok(())
        }.await;
        fixture.service.cleanup.release_gate();
        let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
        checked?; stopped??; Ok(())
    }).await?
}

#[tokio::test]
async fn actual_revocation_denies_cohort_and_next_run_but_preserves_native_cleanup() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(45), async {
        let server = upstream().await?;
        let directory = tempfile::tempdir()?;
        let url = format!("{}/v1", server.uri());
        let (fixture, headers, key) = fixture(directory.path(), &url, 1_000_000, 4).await?;
        let checked = async {
            let mut session = open(&fixture, &headers, &url, directory.path(), "revoked").await?;
            session
                .run(task(), Arc::new(Approval), CancellationToken::new())
                .await?;
            drained(&fixture).await?;
            let snapshot = freeze(&fixture, &headers).await?;
            ensure!(
                session
                    .experiment_cohort(MODEL, &snapshot.evidence_root)
                    .await?
                    .challenger
                    .observed
                    == 1
            );
            result(
                fixture
                    .service
                    .execute(
                        fixture.operator.clone(),
                        Command::Revoke(
                            fixture.tenant.clone(),
                            key.credential_id,
                            Mutation {
                                operation_id: "revoke-experiment".try_into()?,
                                expected_revision: key.credential_revision,
                                installation_id: fixture.installation.clone(),
                            },
                        ),
                    )
                    .await
                    .map_err(failure)?,
            )?;
            ensure!(
                session
                    .experiment_cohort(MODEL, &snapshot.evidence_root)
                    .await
                    .is_err()
            );
            ensure!(
                session
                    .run(task(), Arc::new(Approval), CancellationToken::new())
                    .await
                    .is_err()
            );
            tokio::time::timeout(TIMEOUT, session.close()).await??;
            ensure!(server.received_requests().await.context("requests")?.len() == 1);
            ensure!(fixture.service.current().is_ok());
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
