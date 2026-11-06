//! Actual native SQL owners sharing the tenancy ledger; finite file fixtures.
use super::*;
use crate::agent_store::{
    ExecutionRecord, ExecutionStore, OwnerClaim, codec, discovery, execution, record,
    runtime_owner, runtime_ownership,
};
use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, Set,
};
use std::{sync::Arc, time::Duration};
use tokio_util::task::AbortOnDropHandle;

const TIMEOUT: Duration = Duration::from_secs(10);
const FLOOR: u64 = 64 * 1024;

struct Fixture {
    directory: tempfile::TempDir,
    db: sea_orm::DatabaseConnection,
    service: crate::tenancy::Service,
    budget: Budget,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("native.sqlite");
        let db = crate::db::connect(&format!("sqlite://{}", path.display())).await?;
        crate::db::run_migrations(&db).await?;
        db.execute_unprepared("PRAGMA max_page_count=4096; PRAGMA wal_autocheckpoint=0;")
            .await?;
        let mut config = crate::config::tenancy::TenancyConfig::default();
        config.capacity.receipts.admission_free_bytes = 2 * FLOOR;
        config.capacity.receipts.maintenance_free_bytes = FLOOR;
        let service = crate::tenancy::Service::open_profile(
            db.clone(),
            None,
            &config,
            &crate::config::ContinuationConfig::default(),
        )
        .await?;
        let budget = service.storage_budget();
        ensure!(std::fs::metadata(&path)?.len() > 0);
        Ok(Self {
            directory,
            db,
            service,
            budget,
        })
    }

    fn store(&self, gate: Option<Arc<Gate>>) -> DatabaseExecutionStore {
        let mut store = DatabaseExecutionStore::new(self.db.clone())
            .with_storage_budget(Some(self.budget.clone()));
        store.physical_gate = gate;
        store
    }

    fn observe(
        &self,
        free: Option<u64>,
    ) -> Result<crate::telemetry::receipt_latency::capacity::Snapshot> {
        let observed = self
            .budget
            .calibration_observation(free)
            .map_err(|e| anyhow::anyhow!("native physical observation: {e:?}"))?;
        ensure!(
            observed.observed_free.is_none(),
            "pressure requires an unbound test volume"
        );
        Ok(observed)
    }

    async fn finish(self, quarantined: bool) -> Result<()> {
        let stopped = tokio::time::timeout(TIMEOUT, self.service.shutdown()).await?;
        ensure!(stopped.is_err() == quarantined);
        self.db.close().await?;
        // Reuse the existing finite scratch observer, including SQLite sidecars.
        crate::storage::write::measurement::files(&self.directory.path().join("native.sqlite"))?;
        Ok(())
    }

    async fn claim(
        &self,
        name: &'static str,
    ) -> Result<(bitrouter_ai::harness::runtime::ExecutionOwner, u64)> {
        let gate = Gate::new(true);
        let store = self.store(Some(gate.clone()));
        let task =
            AbortOnDropHandle::new(tokio::spawn(async move { store.claim_owner(name).await }));
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        let reserved = self.observe(None)?.physical_write_reserved;
        gate.release.add_permits(1);
        let claim = tokio::time::timeout(TIMEOUT, task)
            .await??
            .map_err(anyhow::Error::msg)?;
        let OwnerClaim::Acquired { owner } = claim else {
            anyhow::bail!("owner not acquired")
        };
        ensure!(self.observe(None)?.physical_write_reserved == 0);
        Ok((owner, reserved))
    }
}

fn queued(prompt: String) -> ExecutionRecord {
    ExecutionRecord::TurnQueued {
        turn_id: "turn".into(),
        user_item_id: "input".into(),
        prompt,
        queue_order: 1,
    }
}

fn legacy(raw: &str) -> Result<String> {
    Ok(format!(
        "{}{}",
        codec::LEGACY_PREFIX,
        serde_json::json!({
            "bytes": raw.len(), "sha256": crate::storage::checkpoint::sha256(raw.as_bytes()),
            "body": STANDARD.encode(zstd::bulk::compress(raw.as_bytes(), 3)?)
        })
    ))
}

#[tokio::test]
async fn claim_growth_refusal_rolls_back_owner_index_and_earlier_legacy_rewrites() -> Result<()> {
    let f = Fixture::new().await?;
    let (old, _) = f.claim("old0").await?;
    let store = f.store(None);
    let first = queued("x".repeat(2048));
    let mut seed = 17_u32;
    let bytes: Vec<_> = (0..64 * 1024)
        .map(|_| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 24) as u8
        })
        .collect();
    let second = queued(STANDARD.encode(bytes));
    store
        .commit_owned(&old, "legacy", 0, &[first.clone(), second.clone()])
        .await
        .map_err(anyhow::Error::msg)?;
    let raw_first = serde_json::to_string(&first)?;
    let previous = [
        legacy(&raw_first)?,
        legacy(&serde_json::to_string(&second)?)?,
    ];
    ensure!(
        raw_first.len() > previous[0].len(),
        "fixture must expand legacy v1"
    );
    for (index, payload) in previous.iter().enumerate() {
        record::ActiveModel {
            execution_id: Set("legacy".into()),
            sequence: Set(i64::try_from(index)? + 1),
            payload: Set(payload.clone()),
        }
        .update(&f.db)
        .await?;
    }
    discovery::Entity::delete_many()
        .filter(discovery::Column::ExecutionId.eq("legacy"))
        .exec(&f.db)
        .await?;
    let stopped = store.stop_owner(&old).await.map_err(anyhow::Error::msg)?;
    f.observe(Some(4 * FLOOR))?;
    let gate = Gate::new(false);
    let claiming = f.store(Some(gate.clone()));
    let task = AbortOnDropHandle::new(tokio::spawn(
        async move { claiming.claim_owner("new0").await },
    ));
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    let held = f.observe(None)?;
    // The small expanding replacement has consumed its charge; the following
    // large row cannot grow. No partial transaction charge has been refunded.
    ensure!(
        held.physical_write_reserved
            > crate::tenancy::storage::estimate(raw_first.len() as u64)
                .map_err(|e| anyhow::anyhow!("estimate: {e:?}"))?
            && !held.fenced
    );
    gate.release.add_permits(1);
    let refused = tokio::time::timeout(TIMEOUT, task).await??;
    ensure!(refused.is_err_and(|error| error.contains("CapacityExceeded")));
    let after = f.observe(None)?;
    ensure!(after.physical_write_reserved == 0 && !after.fenced);
    ensure!(
        runtime_owner::Entity::find_by_id(i64::try_from(stopped.generation)? + 1)
            .one(&f.db)
            .await?
            .is_none()
    );
    ensure!(discovery::Entity::find().one(&f.db).await?.is_none());
    let control = runtime_ownership::Entity::find_by_id(1)
        .one(&f.db)
        .await?
        .context("control")?;
    ensure!(
        control.server_instance_id.as_deref() == Some("old0") && control.stopped_at_ms.is_some()
    );
    let rows = record::Entity::find()
        .order_by_asc(record::Column::Sequence)
        .all(&f.db)
        .await?;
    ensure!(rows.iter().map(|row| &row.payload).eq(previous.iter()));
    f.observe(Some(u64::MAX))?;
    let (next, _) = f.claim("new0").await?;
    let stored = store
        .load("legacy")
        .await
        .map_err(anyhow::Error::msg)?
        .context("legacy facts")?;
    ensure!(serde_json::to_value(stored.records)? == serde_json::to_value([first, second])?);
    store.stop_owner(&next).await.map_err(anyhow::Error::msg)?;
    let (next, baseline) = f.claim("new1").await?;
    store
        .commit_owned(&next, "modern", 0, &[queued("modern ".repeat(16 * 1024))])
        .await
        .map_err(anyhow::Error::msg)?;
    let modern = record::Entity::find_by_id(("modern".to_owned(), 1))
        .one(&f.db)
        .await?
        .context("modern")?;
    ensure!(modern.payload.starts_with("bro-zstd-v2:"));
    store.stop_owner(&next).await.map_err(anyhow::Error::msg)?;
    let (_, with_modern_history) = f.claim("new2").await?;
    ensure!(
        baseline == with_modern_history,
        "unchanged modern history is not a write set"
    );
    f.finish(false).await
}

#[tokio::test]
async fn append_constraints_refund_only_after_rollback_and_stop_keeps_maintenance() -> Result<()> {
    let f = Fixture::new().await?;
    let (owner, _) = f.claim("writer").await?;
    let store = f.store(None);
    let facts = vec![
        queued("accepted".into()),
        ExecutionRecord::AcceptedKey {
            entry: crate::harness::storage::AcceptedKey {
                scope: "caller".into(),
                key: "request".into(),
                fingerprint: "digest".into(),
                thread_id: "thread".into(),
                turn_id: Some("turn".into()),
            },
        },
    ];
    ensure!(
        store
            .commit_owned(&owner, "thread", 0, &facts)
            .await
            .map_err(anyhow::Error::msg)?
            == 2
    );
    let gate = Gate::new(false);
    let writer = f.store(Some(gate.clone()));
    let owned = owner.clone();
    let task = AbortOnDropHandle::new(tokio::spawn(async move {
        writer.commit_owned(&owned, "thread", 2, &facts).await
    }));
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    let pending = f.observe(None)?;
    ensure!(pending.physical_write_reserved > 0 && !pending.fenced);
    gate.release.add_permits(1);
    ensure!(tokio::time::timeout(TIMEOUT, task).await??.is_err());
    ensure!(f.observe(None)?.physical_write_reserved == 0 && !f.observe(None)?.fenced);
    ensure!(
        store
            .load("thread")
            .await
            .map_err(anyhow::Error::msg)?
            .context("thread")?
            .version
            == 2
    );
    ensure!(
        store
            .commit_owned(&owner, "thread", 1, &[ExecutionRecord::CoreDispatch])
            .await
            .is_err()
    );
    f.observe(Some(2 * FLOOR - 1))?;
    // A real lock write would fail READONLY. Ordinary reservation must refuse
    // first, without misclassifying this no-SQL failure as damaged storage.
    f.db.execute_unprepared("PRAGMA query_only=ON").await?;
    let refused = store
        .commit_owned(&owner, "new", 0, &[ExecutionRecord::CoreDispatch])
        .await;
    f.db.execute_unprepared("PRAGMA query_only=OFF").await?;
    ensure!(refused.is_err_and(|error| error.contains("CapacityExceeded")));
    ensure!(!f.observe(None)?.fenced && f.observe(None)?.physical_write_reserved == 0);
    ensure!(
        execution::Entity::find_by_id("new")
            .one(&f.db)
            .await?
            .is_none()
    );
    ensure!(
        store
            .stop_owner(&owner)
            .await
            .map_err(anyhow::Error::msg)?
            .stopped_at_ms
            .is_some()
    );
    f.finish(false).await
}

#[tokio::test]
async fn actual_readonly_native_lock_fences_before_ack_and_cannot_be_cleared_by_fresh_space()
-> Result<()> {
    let f = Fixture::new().await?;
    let (owner, _) = f.claim("writer").await?;
    f.db.execute_unprepared("PRAGMA query_only=ON").await?;
    let gate = Gate::new(false);
    let store = f.store(Some(gate.clone()));
    let task = AbortOnDropHandle::new(tokio::spawn(async move {
        store
            .commit_owned(&owner, "readonly", 0, &[ExecutionRecord::CoreDispatch])
            .await
    }));
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    let pending = f.observe(None)?;
    ensure!(pending.fenced && pending.physical_write_reserved > 0);
    gate.release.add_permits(1);
    ensure!(tokio::time::timeout(TIMEOUT, task).await??.is_err());
    ensure!(f.observe(None)?.fenced && f.observe(None)?.physical_write_reserved == 0);
    f.db.execute_unprepared("PRAGMA query_only=OFF").await?;
    f.observe(Some(u64::MAX))?;
    ensure!(f.budget.reserve(0, false).is_err() && f.budget.reserve(0, true).is_err());
    ensure!(
        execution::Entity::find_by_id("readonly")
            .one(&f.db)
            .await?
            .is_none()
    );
    f.finish(false).await
}

#[tokio::test]
async fn native_cancellation_and_consuming_commit_failure_retain_aggregate_custody() -> Result<()> {
    for cancel in [false, true] {
        let f = Fixture::new().await?;
        let (owner, _) = f.claim("writer").await?;
        if !cancel {
            // The INSERT succeeds; the consuming COMMIT produces a real
            // deferred-FK error, with no explicit rollback ACK available.
            f.db.execute_unprepared("PRAGMA foreign_keys=ON; CREATE TABLE native_parent(id INTEGER PRIMARY KEY); CREATE TABLE native_child(id INTEGER REFERENCES native_parent(id) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER native_commit_fault AFTER INSERT ON bro_execution_records BEGIN INSERT INTO native_child VALUES(1); END;").await?;
        }
        let gate = Gate::new(true);
        let store = f.store(Some(gate.clone()));
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            store
                .commit_owned(&owner, "unconfirmed", 0, &[queued("pending".repeat(512))])
                .await
        }));
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        let pending = f.observe(None)?;
        ensure!(!pending.fenced && pending.physical_write_reserved > 0);
        if cancel {
            task.abort();
            ensure!(
                tokio::time::timeout(TIMEOUT, task)
                    .await?
                    .is_err_and(|e| e.is_cancelled())
            );
        } else {
            gate.release.add_permits(1);
            ensure!(tokio::time::timeout(TIMEOUT, task).await??.is_err());
        }
        let retained = f.observe(Some(u64::MAX))?;
        ensure!(
            retained.fenced && retained.physical_write_reserved == pending.physical_write_reserved
        );
        ensure!(f.budget.reserve(0, true).is_err());
        ensure!(
            crate::tenancy::ownership::acquire_path(
                &f.directory.path().join("native.sqlite"),
                false
            )
            .is_err()
        );
        f.finish(true).await?;
    }
    Ok(())
}

#[tokio::test]
async fn unbudgeted_native_path_skips_accounting_preparation() -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let store = DatabaseExecutionStore::new(db.clone());
    let called = AtomicBool::new(false);
    let (mut write, tx) = Write::begin(&store, false, || {
        called.store(true, Ordering::SeqCst);
        anyhow::bail!("disabled budget must not prepare estimates")
    })
    .await?;
    write.statement(|| {
        called.store(true, Ordering::SeqCst);
        sea_orm::Statement::from_string(sea_orm::DatabaseBackend::Sqlite, "SELECT 1")
    })?;
    write
        .finish(tx, Err::<(), _>(anyhow::anyhow!("known refusal")))
        .await
        .err()
        .context("expected known rollback")?;
    ensure!(!called.load(Ordering::SeqCst));
    ensure!(
        store
            .commit("legacy", 0, &[ExecutionRecord::CoreDispatch])
            .await
            .map_err(anyhow::Error::msg)?
            == 1
    );
    db.close().await?;
    Ok(())
}
