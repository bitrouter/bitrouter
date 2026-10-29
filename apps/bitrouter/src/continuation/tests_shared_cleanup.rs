use super::super::tests::{bind, fixture_lease, isolated_database, publication, read};
use super::*;

fn intent(row: &continuation_entity::Model) -> NativeIntent {
    NativeIntent {
        identity: row.continuation_identity.clone(),
        generation: row.publication_generation.clone(),
        producer: row.publication_instance_id.clone(),
    }
}

async fn active(native: &SharedNative, request: &str) -> Result<continuation_entity::Model> {
    let bound = bind(native, request).await?;
    let token = publication(&bound.row);
    native
        .transition(&token, ForwardTransition::Delivering)
        .await?;
    native.transition(&token, ForwardTransition::Active).await
}

fn claimed(observation: Observation) -> Result<Claim> {
    match observation {
        Observation::Claimed(claim) => Ok(*claim),
        _ => anyhow::bail!("cleanup authority did not produce a claim"),
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn cancelled_active_demotion_survives_lost_claim_result_and_changed_lease() -> Result<()> {
    let database = isolated_database().await?;
    let result = async {
        let native = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
        let row = active(&native, "lost-cleanup-result").await?;
        let original = intent(&row);
        let short = fixture_lease(&native, &row, 5_000).await?;
        // A renewal can commit while its acknowledgement is lost. Stable
        // generation/producer identity still owns the compensation obligation.
        let renewed = native.renew(&publication(&short)).await?;
        ensure!(renewed.publication_lease_until != short.publication_lease_until);
        let claim = claimed(
            native
                .claim_cleanup(Authority::Cancelled(&original))
                .await?,
        )?;
        let demoted = read(&native, &row.continuation_identity).await?;
        ensure!(demoted.publication_state == PUBLICATION_CLEANUP);
        ensure!(demoted.publication_generation == renewed.publication_generation);
        ensure!(demoted.publication_instance_id == renewed.publication_instance_id);
        ensure!(demoted.publication_lease_until == renewed.publication_lease_until);
        ensure!(demoted.created_at == renewed.created_at);
        ensure!(
            demoted.expires_at == renewed.expires_at && demoted.purge_after == renewed.purge_after
        );
        ensure!(native.decrypt(&demoted)? == native.decrypt(&renewed)?);
        ensure!(native.renew(&publication(&demoted)).await.is_err());
        ensure!(
            native
                .transition(&publication(&demoted), ForwardTransition::Active)
                .await
                .is_err()
        );
        ensure!(bind(&native, "lost-cleanup-result").await.is_err());
        ensure!(matches!(
            native.resolve("owner", "lost-cleanup-result").await?,
            ContinuationResolution::Missing
        ));
        // Losing the acknowledged claim value models a caller-side crash
        // boundary, not a PostgreSQL wire fault. Durable cleanup is sufficient
        // for a different producer to finish without the old receipt/lease.
        drop(claim);
        let peer = SharedNative::new(native.authority.fixture_peer().await?);
        ensure!(peer.authority.producer_id() != native.authority.producer_id());
        let recovery = peer.recover_stale().await?;
        ensure!(recovery.scanned == 1 && recovery.removed == 1 && recovery.failed == 0);
        ensure!(matches!(
            native
                .claim_cleanup(Authority::Cancelled(&original))
                .await?,
            Observation::Missing
        ));
        Ok(())
    }
    .await;
    let closed = database.close().await;
    result?;
    closed?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn closure_does_not_authorize_active_rollback_or_foreign_cleanup() -> Result<()> {
    let database = isolated_database().await?;
    let result = async {
        let native = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
        let active = active(&native, "retiring-active").await?;
        let active = fixture_lease(&native, &active, -1).await?;
        let pending = bind(&native, "retiring-provisional").await?.row;
        let peer = SharedNative::new(native.authority.fixture_peer().await?);
        let foreign_closure = peer.authority.close_binds().await?;
        let pending_intent = intent(&pending);
        ensure!(
            native
                .claim_cleanup(Authority::Closed {
                    intent: &pending_intent,
                    closure: &foreign_closure,
                })
                .await
                .is_err()
        );
        let closure = native.authority.close_binds().await?;
        let active_intent = intent(&active);
        ensure!(matches!(
            native
                .claim_cleanup(Authority::Closed {
                    intent: &active_intent,
                    closure: &closure,
                })
                .await?,
            Observation::Retained
        ));
        let mut foreign_generation = active_intent.clone();
        foreign_generation.generation = publication_generation()?;
        ensure!(matches!(
            native
                .claim_cleanup(Authority::Cancelled(&foreign_generation))
                .await?,
            Observation::Foreign
        ));
        ensure!(read(&native, &active.continuation_identity).await? == active);

        let claim = claimed(
            native
                .claim_cleanup(Authority::Closed {
                    intent: &pending_intent,
                    closure: &closure,
                })
                .await?,
        )?;
        ensure!(native.finish_cleanup(&claim).await? == Finished::Removed);
        let absent = native.intent("owner", "never-bound")?;
        ensure!(matches!(
            native
                .claim_cleanup(Authority::Closed {
                    intent: &absent,
                    closure: &closure,
                })
                .await?,
            Observation::Missing
        ));
        ensure!(native.recover_stale().await?.scanned == 0);
        ensure!(read(&native, &active.continuation_identity).await? == active);
        Ok(())
    }
    .await;
    let closed = database.close().await;
    result?;
    closed?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn stale_recovery_uses_database_lease_without_revoking_active_retention() -> Result<()> {
    let database = isolated_database().await?;
    let result = async {
        let native = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
        let active = active(&native, "active-with-expired-lease").await?;
        let active = fixture_lease(&native, &active, -1).await?;
        let provisional = bind(&native, "stale-provisional").await?.row;
        fixture_lease(&native, &provisional, -1).await?;
        let delivering = bind(&native, "stale-delivering").await?.row;
        let delivering = native
            .transition(&publication(&delivering), ForwardTransition::Delivering)
            .await?;
        fixture_lease(&native, &delivering, -1).await?;
        let live = bind(&native, "still-publishing").await?.row;
        let tx = native
            .authority
            .begin_cleanup(&live.continuation_identity)
            .await?;
        let live_snapshot = native
            .lock_row(&tx, &live.continuation_identity)
            .await?
            .context("live native row missing")?;
        ensure!(
            native
                .save_cleanup(&tx, &live_snapshot, &native.decrypt(&live)?, true)
                .await
                .is_err()
        );
        tx.rollback().await?;
        let peer = SharedNative::new(native.authority.fixture_peer().await?);
        let pass = peer.recover_stale().await?;
        ensure!(pass.scanned == 2 && pass.removed == 2 && pass.failed == 0);
        ensure!(read(&native, &active.continuation_identity).await? == active);
        ensure!(read(&native, &live.continuation_identity).await? == live);
        ensure!(matches!(
            peer.resolve("owner", "active-with-expired-lease").await?,
            ContinuationResolution::Active(_)
        ));
        ensure!(matches!(
            peer.resolve("owner", "stale-provisional").await?,
            ContinuationResolution::Missing
        ));
        Ok(())
    }
    .await;
    let closed = database.close().await;
    result?;
    closed?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn cleanup_claim_cannot_remove_a_replacement_at_the_same_revision() -> Result<()> {
    let database = isolated_database().await?;
    let result = async {
        let native = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
        let first = bind(&native, "cleanup-aba").await?.row;
        let first_intent = intent(&first);
        let old_claim = claimed(
            native
                .claim_cleanup(Authority::Cancelled(&first_intent))
                .await?,
        )?;
        let peer = SharedNative::new(native.authority.fixture_peer().await?);
        ensure!(peer.finish_cleanup(&old_claim).await? == Finished::Removed);
        ensure!(peer.finish_cleanup(&old_claim).await? == Finished::Missing);
        let second = bind(&native, "cleanup-aba").await?.row;
        let second_intent = intent(&second);
        let new_claim = claimed(
            native
                .claim_cleanup(Authority::Cancelled(&second_intent))
                .await?,
        )?;
        ensure!(old_claim.snapshot.revision == new_claim.snapshot.revision);
        ensure!(old_claim.snapshot.body != new_claim.snapshot.body);
        ensure!(peer.finish_cleanup(&old_claim).await? == Finished::Changed);
        let unrelated = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
        ensure!(unrelated.finish_cleanup(&new_claim).await.is_err());
        ensure!(peer.finish_cleanup(&new_claim).await? == Finished::Removed);
        Ok(())
    }
    .await;
    let closed = database.close().await;
    result?;
    closed?;
    Ok(())
}
