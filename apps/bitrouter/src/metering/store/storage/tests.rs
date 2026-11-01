use super::*;
use anyhow::{Context, Result, ensure};
use futures::FutureExt;
use sea_orm::{ConnectionTrait, EntityTrait, Set, TransactionTrait, Unchanged};
use std::sync::{Arc, OnceLock};

use crate::{
    cloud::settlement::{SettlementReceipt, SettlementState, SettlementUsage},
    metering::{
        db::{ReconciliationStatus, RequestMetric},
        pricing::{ChargeStatus, unavailable_charge_evidence},
    },
    tenancy::Service,
};

fn metric(id: &str) -> RequestMetric {
    let usage = bitrouter_ai::types::Usage::default();
    RequestMetric {
        request_id: id.into(),
        user_id: "storage-user".into(),
        api_key_id: "storage-key-id".into(),
        launch_id: None,
        session_identity: None,
        router_id: None,
        binding_digest: None,
        original_selector: None,
        model_id: "storage-model".into(),
        provider_id: "storage-provider".into(),
        prompt_tokens: 0,
        completion_tokens: 0,
        reasoning_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        uncached_input_tokens: 0,
        output_tokens: 0,
        usage_origin: usage.origin,
        raw_usage: Some(serde_json::json!({"encoded": "quote\"\n\u{0000}雪"})),
        charge_status: ChargeStatus::Unknown,
        charge_evidence: unavailable_charge_evidence(&usage, "test_pending"),
        reconciliation_status: ReconciliationStatus::Pending,
        estimated_charge_micro_usd: 0,
        latency_ms: 1,
        generation_time_ms: 1,
        streamed: false,
        error: None,
    }
}

fn receipt(id: &str, state: SettlementState) -> SettlementReceipt {
    SettlementReceipt {
        request_id: id.into(),
        state,
        model_id: None,
        provider_id: None,
        usage: SettlementUsage {
            uncached_input_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
        },
        final_charge_micro_usd: None,
    }
}

async fn database() -> Result<sea_orm::DatabaseConnection> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    Ok(db)
}

async fn owned_store() -> Result<(MeteringStore, Service)> {
    let db = database().await?;
    let service = Service::open(db.clone(), None).await?;
    let cell = Arc::new(OnceLock::new());
    cell.set(service.storage_budget())
        .map_err(|_| anyhow::anyhow!("budget cell already initialized"))?;
    Ok((MeteringStore::new(db).with_storage_budget(cell), service))
}

fn healthy(budget: &Budget) -> Result<()> {
    budget
        .reserve(0, false)
        .map_err(|code| anyhow::anyhow!("unexpected storage refusal: {code:?}"))?
        .complete()
        .map_err(|code| anyhow::anyhow!("unexpected storage release failure: {code:?}"))
}

#[test]
fn prepared_bytes_include_encoded_json_utf8_and_unchanged_row_values() -> Result<()> {
    let encoded = serde_json::to_string(&serde_json::json!({"value": "\n\u{0000}雪"}))?;
    let mut row = requests::ActiveModel {
        request_id: Set("request".into()),
        raw_usage_json: Unchanged(Some(encoded.clone())),
        prompt_tokens: Unchanged(17),
        reconciliation_last_error: Set(None),
        ..Default::default()
    };
    let old_bytes = model_bytes(&row)?;
    ensure!(old_bytes == "request".len() + encoded.len() + 8 + 1);
    row.raw_usage_json = Set(Some("{}".into()));
    let new_bytes = model_bytes(&row)?;
    ensure!(new_bytes == "request".len() + 2 + 8 + 1);
    ensure!(
        old_bytes > new_bytes,
        "shrinking a row still has an old payload"
    );
    row.raw_usage_json = Unchanged(Some("{}".into()));
    ensure!(model_bytes(&row)? == new_bytes);
    Ok(())
}

#[tokio::test]
async fn uninitialized_shared_budget_refuses_every_metering_write_path() -> Result<()> {
    let db = database().await?;
    let ordinary = MeteringStore::new(db.clone());
    ordinary.record_request(metric("pending")).await?;
    let mut unknown = metric("unknown");
    unknown.reconciliation_status = ReconciliationStatus::Unknown;
    ordinary.record_request(unknown).await?;
    let before = requests::Entity::find().all(&db).await?;
    let cell = Arc::new(OnceLock::new());
    let store = MeteringStore::new(db.clone()).with_storage_budget(cell.clone());
    let cloned_before_startup = store.clone();
    ensure!(store.record_request(metric("new")).await.is_err());
    ensure!(store.start_reconciliation_attempt("pending").await.is_err());
    ensure!(
        store
            .reopen_unknown_reconciliation("unknown", 3)
            .await
            .is_err()
    );
    ensure!(
        store
            .record_reconciliation_error("pending", "retry")
            .await
            .is_err()
    );
    ensure!(
        store
            .exhaust_reconciliation("pending", "expired")
            .await
            .is_err()
    );
    ensure!(
        store
            .apply_authoritative_receipt(&receipt("pending", SettlementState::NotCharged), &[])
            .await
            .is_err()
    );
    ensure!(
        store
            .apply_authoritative_receipt(&receipt("pending", SettlementState::Unknown), &[])
            .await
            .is_err()
    );
    let after = requests::Entity::find().all(&db).await?;
    ensure!(before.len() == after.len());
    for row in &before {
        ensure!(after.iter().any(|candidate| candidate == row));
    }
    let service = Service::open(db, None).await?;
    cell.set(service.storage_budget())
        .map_err(|_| anyhow::anyhow!("budget cell already initialized"))?;
    cloned_before_startup
        .record_request(metric("after-startup"))
        .await?;
    healthy(&service.storage_budget())?;
    service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn shared_budget_success_preserves_upsert_and_reconciliation_behavior() -> Result<()> {
    let (store, service) = owned_store().await?;
    let cloned = store.clone();
    store.record_request(metric("settled")).await?;
    let before = requests::Entity::find_by_id("settled")
        .one(&store.db)
        .await?
        .context("metering row missing")?;
    let mut refreshed = metric("settled");
    refreshed.latency_ms = 20;
    cloned.record_request(refreshed).await?;
    let after = requests::Entity::find_by_id("settled")
        .one(&store.db)
        .await?
        .context("upsert row missing")?;
    ensure!(after.created_at == before.created_at && after.latency_ms == 20);
    ensure!(cloned.start_reconciliation_attempt("settled").await? == 1);
    cloned
        .record_reconciliation_error("settled", "retry")
        .await?;
    cloned.exhaust_reconciliation("settled", "expired").await?;
    ensure!(cloned.reopen_unknown_reconciliation("settled", 3).await?);
    ensure!(
        cloned
            .apply_authoritative_receipt(&receipt("settled", SettlementState::NotCharged), &[])
            .await?
            == ReconciliationStatus::NotCharged
    );
    store.record_request(metric("unresolved")).await?;
    ensure!(
        store
            .apply_authoritative_receipt(&receipt("unresolved", SettlementState::Unknown), &[])
            .await?
            == ReconciliationStatus::Unknown
    );
    healthy(&service.storage_budget())?;
    service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn estimate_overflow_and_unstarted_drop_do_not_write_or_fence() -> Result<()> {
    let (store, service) = owned_store().await?;
    store.record_request(metric("unchanged")).await?;
    let before = requests::Entity::find_by_id("unchanged")
        .one(&store.db)
        .await?
        .context("row missing")?;
    let mut active: requests::ActiveModel = before.clone().into();
    active.reconciliation_attempts = Set(9);
    let refused = store
        .update_row(active.clone(), usize::MAX, "overflow must precede SQL")
        .await;
    ensure!(matches!(refused, Err(BitrouterError::RateLimited { .. })));
    let charge = store.reserve_write(&active, 0)?;
    ensure!(charge.is_some());
    drop(charge);
    healthy(&service.storage_budget())?;
    let after = requests::Entity::find_by_id("unchanged")
        .one(&store.db)
        .await?
        .context("row disappeared")?;
    ensure!(before == after);
    service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn autocommit_error_fences_shared_budget_without_echoing_database_payload() -> Result<()> {
    let (store, service) = owned_store().await?;
    store.db.execute_unprepared(
        "CREATE TRIGGER metering_write_fault BEFORE INSERT ON requests BEGIN SELECT RAISE(ABORT, 'private-database-payload'); END"
    ).await?;
    let result = store.record_request(metric("failed-write")).await;
    ensure!(matches!(result, Err(BitrouterError::Internal(message))
        if message == "record_request write failed"));
    ensure!(matches!(
        service.storage_budget().reserve(0, false),
        Err(ErrorCode::StorageUnavailable | ErrorCode::AuthorityUnavailable)
    ));
    ensure!(service.shutdown().await.is_err());
    Ok(())
}

#[tokio::test]
async fn reconciliation_update_error_fences_shared_budget() -> Result<()> {
    let (store, service) = owned_store().await?;
    store.record_request(metric("failed-update")).await?;
    store.db.execute_unprepared(
        "CREATE TRIGGER metering_update_fault BEFORE UPDATE ON requests BEGIN SELECT RAISE(ABORT, 'private-update-payload'); END"
    ).await?;
    let result = store.start_reconciliation_attempt("failed-update").await;
    ensure!(matches!(result, Err(BitrouterError::Internal(message))
        if message == "persist reconciliation attempt"));
    ensure!(matches!(
        service.storage_budget().reserve(0, false),
        Err(ErrorCode::StorageUnavailable | ErrorCode::AuthorityUnavailable)
    ));
    ensure!(service.shutdown().await.is_err());
    Ok(())
}

#[tokio::test]
async fn dropping_pending_autocommit_write_keeps_shared_storage_fenced() -> Result<()> {
    let (store, service) = owned_store().await?;
    healthy(&service.storage_budget())?;
    // Occupy the real one-connection SQLite pool. Poll the write through its
    // reservation into the SQL await, then drop that actual consumer future.
    let held = store.db.begin().await?;
    let mut writing = Box::pin(store.record_request(metric("cancelled-write")));
    ensure!(writing.as_mut().now_or_never().is_none());
    drop(writing);
    held.rollback().await?;
    ensure!(matches!(
        service.storage_budget().reserve(0, false),
        Err(ErrorCode::StorageUnavailable | ErrorCode::AuthorityUnavailable)
    ));
    ensure!(service.shutdown().await.is_err());
    Ok(())
}

#[cfg(all(unix, any(feature = "otel-http", feature = "otel-grpc")))]
#[tokio::test]
async fn actual_upsert_and_loaded_shrink_estimates_cover_sampled_file_growth() -> Result<()> {
    use crate::storage::write::measurement;
    use sea_orm::{ConnectOptions, Database};
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("metering-calibration.sqlite");
    let mut options = ConnectOptions::new(format!("sqlite://{}?mode=rwc", path.display()));
    options
        .min_connections(1)
        .max_connections(1)
        .sqlx_logging(false);
    let db = Database::connect(options).await?;
    crate::db::run_migrations(&db).await?;
    let service = Service::open(db.clone(), None).await?;
    let budget = service.storage_budget();
    let cell = Arc::new(OnceLock::new());
    cell.set(budget.clone())
        .map_err(|_| anyhow::anyhow!("budget cell already set"))?;
    let store = MeteringStore::new(db.clone()).with_storage_budget(cell);
    let result: Result<()> = async {
        let mut estimates = Vec::new();
        for (id, loaded) in [("upsert-shrink", false), ("loaded-shrink", true)] {
            let mut seed = metric(id);
            seed.raw_usage = Some(serde_json::json!({"retained": "r".repeat(128 * 1024)}));
            store.record_request(seed).await?;
            let old = requests::Entity::find_by_id(id)
                .one(&db)
                .await?
                .context("seed row")?;
            let mut active: requests::ActiveModel = old.clone().into();
            let previous_bytes = store.stored_bytes(&active)?;
            let mut replacement = metric(id);
            replacement.raw_usage = Some(serde_json::json!({"small": true}));
            let expected_json = serde_json::to_string(&replacement.raw_usage)?;
            active.raw_usage_json = Set(Some(expected_json.clone()));
            let loaded_encoded = previous_bytes
                .checked_add(store.stored_bytes(&active)?)
                .context("prepared bytes")?;
            ensure!(previous_bytes > 128 * 1024 && measurement::reservation(&budget)? == 0);
            let before = measurement::wal_baseline(&db, &path).await?;
            // Occupy only the pool, not a new wrapper transaction around the
            // production autocommit. Both real paths reserve before their SQL await.
            let held = db.begin().await?;
            let mut writing = Box::pin(async {
                if loaded {
                    store
                        .update_row(active, previous_bytes, "calibration loaded update")
                        .await
                } else {
                    store.record_request(replacement).await
                }
            });
            ensure!(writing.as_mut().now_or_never().is_none());
            let charged = measurement::reservation(&budget)?;
            let pending = measurement::files(&path)?;
            held.rollback().await?;
            writing.await?;
            ensure!(measurement::reservation(&budget)? == 0);
            let encoded = measurement::report(&db, &path, id, &before, pending, charged).await?;
            eprintln!("caller calibration metering loaded={loaded} old_prepared={previous_bytes} loaded_old_and_new={loaded_encoded}");
            if loaded {
                ensure!(encoded == u64::try_from(loaded_encoded)?);
            }
            estimates.push(encoded);
            let saved = requests::Entity::find_by_id(id)
                .one(&db)
                .await?
                .context("updated row")?;
            ensure!(saved.raw_usage_json.as_deref() == Some(expected_json.as_str()));
            ensure!(saved.created_at == old.created_at);
        }
        ensure!(estimates.len() == 2 && estimates[1] > estimates[0]);
        healthy(&budget)?;
        Ok(())
    }
    .await;
    let stopped = service.shutdown().await;
    drop((store, service));
    let closed = db.close().await;
    let removed = directory.close();
    result?;
    stopped?;
    closed?;
    removed?;
    Ok(())
}
