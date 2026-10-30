//! Restart cleanup authenticates owned rows and leaves Legacy bytes untouched.
use super::*;
use crate::{
    storage::operation_budget::OperationBudget,
    tenancy::{
        Service,
        continuation::http::{
            codec::timestamp,
            store::{self as sql, Cursor, Row, Scan},
        },
        resources::Resources,
    },
};
use sea_orm::TransactionTrait;

#[tokio::test]
async fn restart_removes_stale_and_purged_rows_and_keeps_authenticated_tombstone() -> Result<()> {
    let mut provider = Server::start().await?;
    let mut ready = ready(&provider).await?;
    let mut published = Vec::new();
    for stream in [false, true, false] {
        let (status, bytes) = send(&ready, ready.headers.clone(), body(stream, None)).await?;
        ensure!(status == StatusCode::OK);
        let public = public_id(&bytes, stream)?;
        provider.wire().await?;
        published.push((public.clone(), store::retained(&ready, &public).await?.1));
    }
    let crypto = ready
        .fixture
        .service
        .http_continuation
        .state
        .crypto()
        .map_err(|_| anyhow::anyhow!("HTTP key"))?
        .clone();
    ready.fixture.service.shutdown().await?;
    let check = || Ok(());
    let budget = OperationBudget::new(&check);
    // Test fixture rewrites only authenticated timestamps/state after the sole
    // producer has joined. The next service, not this fixture, performs cleanup.
    let tx = ready.fixture.db.begin().await?;
    for (index, (_, original)) in published.iter().enumerate() {
        let mut metadata = original.metadata.clone();
        let now = chrono::Utc::now();
        metadata.created_at = timestamp(now - chrono::TimeDelta::days(3));
        metadata.publication_lease_until = timestamp(now - chrono::TimeDelta::hours(1));
        match index {
            0 => metadata.publication_state = "provisional".into(),
            1 => {
                metadata.expires_at = timestamp(now - chrono::TimeDelta::days(1));
                metadata.purge_after = timestamp(now + chrono::TimeDelta::days(1));
            }
            _ => {
                metadata.expires_at = timestamp(now - chrono::TimeDelta::days(2));
                metadata.purge_after = timestamp(now - chrono::TimeDelta::days(1));
            }
        }
        let rewritten = Row::seal(&crypto, metadata, &original.open(&crypto)?)?;
        sql::replace_in(&tx, original, &rewritten, &budget).await?;
    }
    tx.execute_unprepared("INSERT INTO provider_continuations (continuation_identity, owner_identity, ciphertext, nonce, target_fingerprint, key_id, cipher_version, created_at, expires_at, purge_after, publication_state, publication_generation, publication_instance_id, publication_lease_until) VALUES ('LegacyExact  ', 'LegacyOwner', 'Legacy opaque bytes  ', 'legacy-nonce', 'legacy-target', 'legacy-key', 1, 'legacy-created', '0001', '0001', 'provisional', 'legacy-generation', 'legacy-producer', '0001')").await?;
    tx.commit().await?;
    let initial = crate::tenancy::store::load(&ready.fixture.db, chrono::Utc::now()).await?;
    ready.fixture.service = Service::start_prepared(
        ready.fixture.db.clone(),
        None,
        Default::default(),
        initial,
        Arc::new(Resources::with_key(
            ready.fixture.installation.clone(),
            [29; 32],
        )?),
    )
    .await?;
    let outcome: Result<()> = async {
        ready.fixture.service.http_registry.join_reconciler_for_test().await?;
        for scan in [Scan::Stale, Scan::Expired, Scan::Purge] {
            ready.fixture.service.http_continuation.maintain(scan, Cursor::default(), tokio::time::Instant::now()+WAIT).await.map_err(|_| anyhow::anyhow!("maintenance"))?;
        }
        for (index, (public, _)) in published.iter().enumerate() {
            let row = sql::load(&ready.fixture.db, &crypto, &store::owner(&ready), public, &budget).await?;
            if index == 1 {
                let row = row.context("expiry tombstone")?;
                ensure!(row.metadata.publication_state == "active");
                ensure!(row.open(&crypto)?.live.is_none());
                ensure!(matches!(sql::resolve_in(&ready.fixture.db, &crypto, &store::owner(&ready), public, chrono::Utc::now(), &budget).await?, sql::Resolution::Expired));
            } else { ensure!(row.is_none()); }
        }
        let row = ready.fixture.db.query_one(Statement::from_string(ready.fixture.db.get_database_backend(), "SELECT continuation_identity, ciphertext, nonce, created_at, expires_at, purge_after, publication_state, publication_lease_until, installation_id, tenant_id, creator_principal_id FROM provider_continuations WHERE cipher_version=1".to_owned())).await?.context("Legacy row")?;
        for (column, expected) in [("continuation_identity", "LegacyExact  "), ("ciphertext", "Legacy opaque bytes  "), ("nonce", "legacy-nonce"), ("created_at", "legacy-created"), ("expires_at", "0001"), ("purge_after", "0001"), ("publication_state", "provisional"), ("publication_lease_until", "0001")] {
            ensure!(row.try_get::<String>("", column)? == expected);
        }
        for column in ["installation_id", "tenant_id", "creator_principal_id"] { ensure!(row.try_get::<Option<String>>("", column)?.is_none()); }
        ensure!(ready.fixture.service.current().is_ok());
        Ok(())
    }.await;
    ready.fixture.service.shutdown().await?;
    provider.close().await?;
    outcome
}
