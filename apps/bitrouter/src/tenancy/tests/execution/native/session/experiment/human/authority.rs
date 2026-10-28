//! Caller loss never detaches the real actor or bypasses current grants.
use super::*;
use crate::tenancy::transaction::{Disposition, Phase};

#[tokio::test]
async fn cancelled_native_human_waiter_keeps_command_and_h_until_known_commit() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let server = upstream().await?;
        let directory = tempfile::tempdir()?;
        let url = format!("{}/v1", server.uri());
        let (fixture, headers, _) = fixture(directory.path(), &url, 1_000_000, 4).await?;
        let mut session = None;
        let checked = async {
            session = Some(open(&fixture, &headers, &url, directory.path(), "human-cancel").await?);
            let current = session.as_mut().context("actual native owner")?;
            current.run(observed_task(), Arc::new(Approval), CancellationToken::new()).await?;
            drained(&fixture).await?;
            let view = current.review_run().await?;
            let input = label(&view, "cancelled-human-waiter", true)?;
            let reads = fixture.service.reads.available_permits();
            let gate = fixture.service.cleanup.hold(Phase::Committing);
            {
                let work = current.submit_assessment(input);
                tokio::pin!(work);
                tokio::select! {
                    result = &mut work => anyhow::bail!("Human submit bypassed actual COMMIT gate: {result:?}"),
                    entered = tokio::time::timeout(TIMEOUT, gate.entered.notified()) => { entered?; }
                }
            }
            ensure!(fixture.service.cleanup.disposition() == Disposition::Pending);
            ensure!(fixture.service.reads.available_permits() + 1 == reads);
            ensure!(current.review_run().await.is_err(), "cancelled waiter released the command owner");
            fixture.service.cleanup.release_gate();
            // A real actor request proves the earlier transaction finished.
            let frozen = freeze(&fixture, &headers).await?;
            ensure!(fixture.service.reads.available_permits() == reads);
            let after = current.review_run().await?;
            ensure!(after.history.len() == 1
                && after.history[0].input.submission_id == "cancelled-human-waiter"
                && after.scoring.expected_revision.as_deref() == Some(after.history[0].revision_id.as_str()));
            ensure!(current.experiment_cohort(MODEL, &frozen.evidence_root).await?.challenger.conclusive == 1);
            ensure!(server.received_requests().await.context("requests")?.len() == 1);
            Ok::<_, anyhow::Error>(())
        }.await;
        fixture.service.cleanup.release_gate();
        let closed = match session {
            Some(session) => tokio::time::timeout(TIMEOUT, session.close()).await
                .map_err(anyhow::Error::from).and_then(|value| value.map_err(anyhow::Error::from)),
            None => Ok(()),
        };
        let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
        checked?;
        closed?;
        stopped??;
        Ok(())
    }).await?
}

#[tokio::test]
async fn revoked_native_human_cannot_review_submit_or_retract_but_owner_cleanup_remains()
-> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let server = upstream().await?;
        let directory = tempfile::tempdir()?;
        let url = format!("{}/v1", server.uri());
        let (fixture, headers, key) = fixture(directory.path(), &url, 1_000_000, 4).await?;
        let mut session = None;
        let checked = async {
            session = Some(open(&fixture, &headers, &url, directory.path(), "human-revoke").await?);
            let current = session.as_mut().context("actual native owner")?;
            current
                .run(
                    observed_task(),
                    Arc::new(Approval),
                    CancellationToken::new(),
                )
                .await?;
            let view = current.review_run().await?;
            current
                .submit_assessment(label(&view, "human-before-revoke", true)?)
                .await?;
            let view = current.review_run().await?;
            result(
                fixture
                    .service
                    .execute(
                        fixture.operator.clone(),
                        Command::Revoke(
                            fixture.tenant.clone(),
                            key.credential_id,
                            Mutation {
                                operation_id: "revoke-native-human".try_into()?,
                                expected_revision: key.credential_revision,
                                installation_id: fixture.installation.clone(),
                            },
                        ),
                    )
                    .await
                    .map_err(failure)?,
            )?;
            ensure!(current.review_run().await.is_err());
            ensure!(
                current
                    .submit_assessment(label(&view, "human-after-revoke", false)?)
                    .await
                    .is_err()
            );
            ensure!(
                current
                    .retract_assessment(RevisionInput {
                        submission_id: "retract-after-revoke".into(),
                        checkpoint_id: view.scoring.evidence.checkpoint_id,
                        expected_revision: view.scoring.expected_revision,
                        source: AssessmentSource::Human,
                        evaluator_id: "ignored".into(),
                        evaluator_version: "ignored".into(),
                        assessment: None,
                        reason: "revoked Human cannot alter the selected assessment".into(),
                    })
                    .await
                    .is_err()
            );
            ensure!(fixture.service.current().is_ok());
            ensure!(server.received_requests().await.context("requests")?.len() == 1);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let closed = match session {
            Some(session) => tokio::time::timeout(TIMEOUT, session.close())
                .await
                .map_err(anyhow::Error::from)
                .and_then(|value| value.map_err(anyhow::Error::from)),
            None => Ok(()),
        };
        let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
        checked?;
        closed?;
        stopped??;
        Ok(())
    })
    .await?
}
