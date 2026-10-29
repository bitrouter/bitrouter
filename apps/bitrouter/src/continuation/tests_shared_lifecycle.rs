use super::super::tests::{bind, fixture_lease, isolated_database, publication, read, wait_past};
use super::*;
use crate::continuation::tests::{static_authority, target};
use sea_orm::TransactionTrait;

async fn snapshot(native: &SharedNative, identity: &str) -> Result<Snapshot> {
    let tx = native.authority.begin_cleanup(identity).await?;
    let snapshot = native
        .lock_row(&tx, identity)
        .await?
        .context("fixture row missing")?;
    tx.commit().await?;
    Ok(snapshot)
}

async fn activate(native: &SharedNative, request: &str) -> Result<continuation_entity::Model> {
    let bound = bind(native, request).await?;
    let token = publication(&bound.row);
    native
        .transition(&token, ForwardTransition::Delivering)
        .await?;
    native.transition(&token, ForwardTransition::Active).await
}

/// Change only authenticated retention metadata using the database clock.
async fn retention(
    native: &SharedNative,
    row: &continuation_entity::Model,
    expires_ms: i64,
    purge_ms: i64,
) -> Result<continuation_entity::Model> {
    let tx = native
        .authority
        .begin_cleanup(&row.continuation_identity)
        .await?;
    let old = native
        .lock_row(&tx, &row.continuation_identity)
        .await?
        .context("retention fixture row missing")?;
    let now = SharedContinuationDb::now(&tx).await?;
    let mut row = old.row.clone();
    row.expires_at = timestamp(now + TimeDelta::milliseconds(expires_ms));
    row.purge_after = timestamp(now + TimeDelta::milliseconds(purge_ms));
    native.seal(&mut row, &native.decrypt(&old.row)?)?;
    // Test-only metadata replacement preserves authentication and index copies.
    // https://www.postgresql.org/docs/current/sql-update.html
    let changed = tx
        .execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "UPDATE gateway_shared_native_continuations
         SET body = $1, expires_at = $2, purge_after = $3, revision = revision + 1
         WHERE namespace = $4 AND identity = $5 AND body = $6",
            [
                serde_json::to_string(&row)?.into(),
                row.expires_at.clone().into(),
                row.purge_after.clone().into(),
                native.authority.namespace().into(),
                row.continuation_identity.clone().into(),
                old.body.into(),
            ],
        ))
        .await?;
    ensure!(changed.rows_affected() == 1);
    tx.commit().await?;
    Ok(row)
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn forward_handshake_commits_before_wait_and_rechecks_expiry() -> Result<()> {
    let database = isolated_database().await?;
    let result = async {
        let native = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
        let bound = bind(&native, "forward-expiry").await?;
        let token = publication(&bound.row);
        let error = native
            .transition(&token, ForwardTransition::Active)
            .await
            .err()
            .context("activation skipped delivery")?;
        ensure!(matches!(
            error.downcast_ref::<Rejection>(),
            Some(Rejection::Changed)
        ));
        let delivering = native
            .transition(&token, ForwardTransition::Delivering)
            .await?;
        ensure!(delivering.publication_state == PUBLICATION_DELIVERING);
        ensure!(
            native
                .transition(&token, ForwardTransition::Delivering)
                .await?
                == delivering
        );
        // A separate transaction can immediately acquire the row during the
        // caller's delivery wait: no transaction spans that external handshake.
        let observer = database.begin().await?;
        observer
            .execute(Statement::from_string(
                DatabaseBackend::Postgres,
                "SET LOCAL lock_timeout = '100ms'",
            ))
            .await?;
        ensure!(
            native
                .lock_row(&observer, &delivering.continuation_identity)
                .await?
                .is_some()
        );
        observer.commit().await?;

        let expired = fixture_lease(&native, &delivering, -1).await?;
        let error = native
            .transition(&publication(&expired), ForwardTransition::Active)
            .await
            .err()
            .context("expired delivery became active")?;
        ensure!(matches!(
            error.downcast_ref::<Rejection>(),
            Some(Rejection::Expired)
        ));
        ensure!(read(&native, &expired.continuation_identity).await? == expired);
        ensure!(matches!(
            native.resolve("owner", "forward-expiry").await?,
            ContinuationResolution::Missing
        ));

        let live = activate(&native, "forward-live").await?;
        let token = publication(&live);
        ensure!(native.transition(&token, ForwardTransition::Active).await? == live);
        for phase in [
            PendingPublicationPhase::Reserved,
            PendingPublicationPhase::Reconcile,
        ] {
            let mut cleanup = publication(&live);
            cleanup.phase = phase;
            ensure!(
                native
                    .transition(&cleanup, ForwardTransition::Active)
                    .await
                    .is_err()
            );
        }
        let mut foreign = publication(&live);
        foreign.instance_id = uuid::Uuid::new_v4().to_string();
        ensure!(
            native
                .transition(&foreign, ForwardTransition::Active)
                .await
                .is_err()
        );
        native.authority.close_binds().await?;
        ensure!(
            native
                .transition(&token, ForwardTransition::Active)
                .await
                .is_err()
        );
        // Closing publication does not invalidate retained read authority.
        ensure!(matches!(
            native.resolve("owner", "forward-live").await?,
            ContinuationResolution::Active(_)
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
async fn active_resolution_uses_retention_and_shared_target_framing() -> Result<()> {
    let database = isolated_database().await?;
    let result = async {
        let native = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
        let active = activate(&native, "retained-active").await?;
        fixture_lease(&native, &active, -1).await?;
        let ContinuationResolution::Active(resolved) =
            native.resolve("owner", "retained-active").await?
        else {
            anyhow::bail!("active mapping depended on its publication lease");
        };
        ensure!(resolved.provider_response_id == "fixture-native-response");
        ensure!(resolved.effective_model.as_deref() == Some("openai:gpt-5"));
        ensure!(resolved.matches_target(
            &target("fixture-static-key"),
            &static_authority("fixture-static-key")
        )?);
        ensure!(
            !resolved
                .matches_target(&target("different-key"), &static_authority("different-key"))?
        );
        let mut changed = target("fixture-static-key");
        changed.account_label = Some("another-account".into());
        ensure!(!resolved.matches_target(&changed, &static_authority("fixture-static-key"))?);
        ensure!(matches!(
            native.resolve("other-owner", "retained-active").await?,
            ContinuationResolution::Missing
        ));
        let other = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
        ensure!(matches!(
            other.resolve("owner", "retained-active").await?,
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
async fn retention_expiring_during_lock_wait_scrubs_native_payload() -> Result<()> {
    let database = isolated_database().await?;
    let result = async {
        let native = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
        let active = activate(&native, "lookup-lock").await?;
        let short = retention(&native, &active, 300, 60_000).await?;
        let blocker = database.begin().await?;
        native
            .lock_row(&blocker, &short.continuation_identity)
            .await?;
        let mut lookup = Box::pin(native.resolve("owner", "lookup-lock"));
        ensure!(
            tokio::time::timeout(Duration::from_millis(80), lookup.as_mut())
                .await
                .is_err()
        );
        wait_past(&native, &short.expires_at).await?;
        blocker.commit().await?;
        ensure!(matches!(lookup.await?, ContinuationResolution::Expired));
        let tombstone = read(&native, &short.continuation_identity).await?;
        ensure!(tombstone.publication_state == PUBLICATION_EXPIRED);
        ensure!(native.decrypt(&tombstone)?.is_empty());
        ensure!(tombstone.owner_identity == short.owner_identity);
        ensure!(tombstone.publication_generation == short.publication_generation);
        ensure!(
            tombstone.expires_at == short.expires_at && tombstone.purge_after == short.purge_after
        );
        let mut tampered = tombstone.clone();
        tampered.purge_after = short.expires_at.clone();
        ensure!(native.decrypt(&tampered).is_err());
        let error = bind(&native, "lookup-lock")
            .await
            .err()
            .context("expired identity rebound")?;
        ensure!(matches!(
            error.downcast_ref::<Rejection>(),
            Some(Rejection::Expired)
        ));
        let pass = native.maintain(32).await?;
        ensure!(pass.scanned == 0 && pass.purged == 0);
        ensure!(read(&native, &short.continuation_identity).await? == tombstone);

        retention(&native, &tombstone, -2, -1).await?;
        let pass = native.maintain(32).await?;
        ensure!(pass.scanned == 1 && pass.purged == 1 && pass.failed == 0);
        ensure!(matches!(
            native.resolve("owner", "lookup-lock").await?,
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
async fn maintenance_sql_guards_retention_and_full_snapshot() -> Result<()> {
    let database = isolated_database().await?;
    let result = async {
        let native = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
        let live = activate(&native, "guarded-maintenance").await?;
        let tx = native
            .authority
            .begin_cleanup(&live.continuation_identity)
            .await?;
        let current = native
            .lock_row(&tx, &live.continuation_identity)
            .await?
            .context("live snapshot missing")?;
        ensure!(!native.scrub(&tx, &current).await?);
        ensure!(!native.purge(&tx, &current).await?);
        tx.commit().await?;
        ensure!(read(&native, &live.continuation_identity).await? == live);

        let expired = retention(&native, &live, -2, 60_000).await?;
        let hint = snapshot(&native, &expired.continuation_identity).await?;
        ensure!(matches!(
            native.maintain_row(&hint).await?,
            RowMaintenance::Scrubbed
        ));
        let tx = native
            .authority
            .begin_cleanup(&expired.continuation_identity)
            .await?;
        ensure!(
            !native.scrub(&tx, &hint).await?,
            "stale snapshot scrubbed again"
        );
        ensure!(!native.purge(&tx, &hint).await?);
        tx.commit().await?;
        let tombstone = read(&native, &live.continuation_identity).await?;
        let purgeable = retention(&native, &tombstone, -2, -1).await?;
        // Reproduce an ABA identity: the removed row and its replacement both
        // have revision zero, so only comparing the full snapshot is sufficient.
        database
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "UPDATE gateway_shared_native_continuations SET revision = 0
             WHERE namespace = $1 AND identity = $2",
                [
                    native.authority.namespace().into(),
                    purgeable.continuation_identity.clone().into(),
                ],
            ))
            .await?;
        let purge_hint = snapshot(&native, &purgeable.continuation_identity).await?;
        ensure!(purge_hint.revision == 0);
        ensure!(matches!(
            native.maintain_row(&purge_hint).await?,
            RowMaintenance::Purged
        ));
        let rebound = bind(&native, "guarded-maintenance").await?;
        ensure!(rebound.outcome == BindOutcome::Inserted);
        ensure!(
            snapshot(&native, &rebound.row.continuation_identity)
                .await?
                .revision
                == 0
        );
        ensure!(matches!(
            native.maintain_row(&purge_hint).await?,
            RowMaintenance::Unchanged
        ));
        ensure!(read(&native, &rebound.row.continuation_identity).await? == rebound.row);
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
async fn maintenance_advances_past_corruption_with_bounded_namespace_scans() -> Result<()> {
    let database = isolated_database().await?;
    let result = async {
        let native = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
        let mut expired = Vec::new();
        for i in 0..34 {
            let bound = bind(&native, &format!("fair-maintenance-{i}")).await?;
            expired.push(retention(&native, &bound.row, -2, 60_000).await?);
        }
        expired.sort_by(|a, b| a.continuation_identity.cmp(&b.continuation_identity));
        let bad = expired.first().context("no maintenance fixture")?;
        let mut corrupt = bad.clone();
        corrupt.owner_identity.push('x');
        // Index copies still match; AEAD must reject the changed owner metadata.
        database
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "UPDATE gateway_shared_native_continuations SET body = $1
             WHERE namespace = $2 AND identity = $3",
                [
                    serde_json::to_string(&corrupt)?.into(),
                    native.authority.namespace().into(),
                    bad.continuation_identity.clone().into(),
                ],
            ))
            .await?;
        let other = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
        let other_row = bind(&other, "unrelated-namespace").await?;
        let other_expired = retention(&other, &other_row.row, -2, 60_000).await?;
        ensure!(native.maintain(0).await.is_err());
        ensure!(native.maintain(10_001).await.is_err());
        let first = native.maintain(10_000).await?;
        ensure!(first.scanned == 32 && first.scrubbed == 31 && first.failed == 1);
        let second = native.maintain(10_000).await?;
        ensure!(second.scanned == 2 && second.scrubbed == 2 && second.failed == 0);
        let retry = native.maintain(10_000).await?;
        ensure!(retry.scanned == 1 && retry.failed == 1);
        for row in expired.iter().skip(1) {
            let stored = read(&native, &row.continuation_identity).await?;
            ensure!(stored.publication_state == PUBLICATION_EXPIRED);
            ensure!(native.decrypt(&stored)?.is_empty());
        }
        ensure!(read(&other, &other_expired.continuation_identity).await? == other_expired);
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
async fn authenticated_payload_shape_cannot_confuse_live_and_expired_states() -> Result<()> {
    let database = isolated_database().await?;
    let result = async {
        let native = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
        let active = activate(&native, "empty-live").await?;
        let mut malformed = active.clone();
        native.seal(&mut malformed, &[])?;
        // Valid authentication alone does not make empty plaintext a live ID.
        database
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "UPDATE gateway_shared_native_continuations SET body = $1
                 WHERE namespace = $2 AND identity = $3",
                [
                    serde_json::to_string(&malformed)?.into(),
                    native.authority.namespace().into(),
                    malformed.continuation_identity.clone().into(),
                ],
            ))
            .await?;
        ensure!(
            native.resolve("owner", "empty-live").await.is_err(),
            "legacy empty-payload fallback escaped the shared boundary"
        );
        ensure!(native.renew(&publication(&malformed)).await.is_err());
        ensure!(
            native
                .transition(&publication(&malformed), ForwardTransition::Active)
                .await
                .is_err()
        );
        ensure!(bind(&native, "empty-live").await.is_err());
        ensure!(read(&native, &malformed.continuation_identity).await? == malformed);

        let bound = bind(&native, "nonempty-expired").await?;
        let mut malformed = retention(&native, &bound.row, -2, -1).await?;
        let plaintext = native.decrypt(&malformed)?;
        malformed.publication_state = PUBLICATION_EXPIRED.into();
        native.seal(&mut malformed, &plaintext)?;
        database
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "UPDATE gateway_shared_native_continuations
                 SET body = $1, publication_state = $2
                 WHERE namespace = $3 AND identity = $4",
                [
                    serde_json::to_string(&malformed)?.into(),
                    PUBLICATION_EXPIRED.into(),
                    native.authority.namespace().into(),
                    malformed.continuation_identity.clone().into(),
                ],
            ))
            .await?;
        ensure!(native.resolve("owner", "nonempty-expired").await.is_err());
        let pass = native.maintain(32).await?;
        ensure!(pass.scanned == 1 && pass.failed == 1 && pass.purged == 0);
        ensure!(read(&native, &malformed.continuation_identity).await? == malformed);
        Ok(())
    }
    .await;
    let closed = database.close().await;
    result?;
    closed?;
    Ok(())
}

async fn wait_for_blocked_query(database: &DatabaseConnection, blocker: i32) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            // Observe actual PostgreSQL lock contention before cancelling the
            // Rust waiter; scheduler delay alone is not evidence of that wait.
            // https://www.postgresql.org/docs/current/functions-info.html#FUNCTIONS-INFO-SESSION-TABLE
            let observed = database
                .query_one(Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    "SELECT EXISTS (
                         SELECT 1 FROM pg_stat_activity
                         WHERE $1 = ANY (pg_blocking_pids(pid))
                     ) AS waiting",
                    [blocker.into()],
                ))
                .await?
                .context("lock observation missing")?;
            if observed.try_get::<bool>("", "waiting")? {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("maintenance never reached the held native row lock")?
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn maintenance_cancellation_and_deadline_preserve_unattempted_suffix() -> Result<()> {
    let database = isolated_database().await?;
    let result = async {
        for (count, cancel) in [(2, true), (32, false)] {
            let native = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
            let mut expired = Vec::new();
            for i in 0..count {
                let bound = bind(&native, &format!("blocked-maintenance-{i}")).await?;
                expired.push(retention(&native, &bound.row, -2, 60_000).await?);
            }
            expired.sort_by(|a, b| a.continuation_identity.cmp(&b.continuation_identity));
            let first = expired.first().context("no locked fixture row")?;
            let blocker = database.begin().await?;
            native
                .lock_row(&blocker, &first.continuation_identity)
                .await?;
            let blocker_id: i32 = blocker
                .query_one(Statement::from_string(
                    DatabaseBackend::Postgres,
                    "SELECT pg_backend_pid() AS pid",
                ))
                .await?
                .context("blocker session missing")?
                .try_get("", "pid")?;
            let mut pass = Box::pin(native.maintain(32));
            tokio::select! {
                result = pass.as_mut() => {
                    result?;
                    anyhow::bail!("maintenance finished despite the held row lock");
                }
                ready = wait_for_blocked_query(&database, blocker_id) => ready?,
            }
            if cancel {
                drop(pass);
            } else {
                ensure!(
                    pass.await.is_err(),
                    "total deadline did not stop the blocked pass"
                );
            }
            let interrupted_cursor = native
                .maintenance_cursor
                .lock()
                .map_err(|_| anyhow::anyhow!("maintenance cursor lock poisoned"))?
                .clone();
            let interrupted_index = expired
                .iter()
                .position(|row| Some(&row.continuation_identity) == interrupted_cursor.as_ref());
            // The first lock stays held. Later rows must progress on the next
            // pass rather than waiting for the first record a second time.
            let next = native
                .maintain(32)
                .await
                .context("unattempted suffix remained behind the blocked first row")?;
            let current_cursor = native
                .maintenance_cursor
                .lock()
                .map_err(|_| anyhow::anyhow!("maintenance cursor lock poisoned"))?
                .clone();
            ensure!(
                next.scanned == count - 1
                    && next.scrubbed == u64::try_from(count - 1)?
                    && next.failed == 0,
                "maintenance suffix mismatch: cancel={cancel}, count={count}, \
                 scanned={}, scrubbed={}, failed={}, purged={}, \
                 interrupted_cursor={interrupted_cursor:?}, \
                 interrupted_index={interrupted_index:?}, \
                 current_cursor={current_cursor:?}, first_identity={}",
                next.scanned,
                next.scrubbed,
                next.failed,
                next.purged,
                first.continuation_identity,
            );
            for row in expired.iter().skip(1) {
                ensure!(
                    read(&native, &row.continuation_identity)
                        .await?
                        .publication_state
                        == PUBLICATION_EXPIRED
                );
            }
            blocker.commit().await?;
            let wrapped = native.maintain(32).await?;
            ensure!(wrapped.scanned == 1 && wrapped.scrubbed == 1 && wrapped.failed == 0);
        }
        Ok(())
    }
    .await;
    let closed = database.close().await;
    result?;
    closed?;
    Ok(())
}
