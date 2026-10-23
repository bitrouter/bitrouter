//! Real PostgreSQL replay across every immutable metering identity.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use tokio::sync::Barrier;

use crate::gateway::shared::{schema, store::Store, test_database::EmptyDatabase};
use crate::metering::shared::{
    SharedMetering,
    record::Record,
    tests::{fixture, rename},
};

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with PostgreSQL CREATEDB privilege"]
async fn postgres_metering_replay_covers_all_unique_keys() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(20), exercise(&mut database))
        .await
        .context("metering replay campaign deadline exceeded")
        .and_then(|result| result);
    database.finish(result).await
}

async fn exercise(database: &mut EmptyDatabase) -> Result<()> {
    const WRITERS: usize = 4;
    let namespace = format!("metering-replay-{}", uuid::Uuid::new_v4());
    let mut stores = Vec::new();
    for _ in 0..WRITERS {
        let db = database.connect(1, Duration::from_secs(3)).await?;
        stores.push(Store::new(db, &namespace, Duration::from_secs(3))?);
    }
    let first = stores.first().context("missing metering fixture pool")?;
    schema::initialize_all(first).await?;
    let metering = SharedMetering::new(&namespace)?;
    for request_bound in [false, true] {
        let mut record = fixture()?;
        if !request_bound {
            record.request_id = None;
        }
        let barrier = Barrier::new(WRITERS);
        // Each first transaction observes absence before any writer inserts.
        // No tasks detach; cancellation drops all owned transaction futures.
        // This controls overlap, not PostgreSQL's internal index-check order.
        let outcomes = futures::future::join_all(
            stores
                .iter()
                .map(|store| write_together(store, &metering, &record, &barrier)),
        )
        .await;
        let mut inserted = 0;
        for outcome in outcomes {
            inserted += usize::from(outcome?);
        }
        ensure!(inserted == 1, "exact replay inserted {inserted} rows");
        let before = snapshot(first, &record).await?;
        let tx = first.begin().await?;
        let stored = metering
            .get_in(
                &tx,
                &record.event.tenant_id,
                &record.principal_id,
                &record.event.execution.execution_id,
            )
            .await?
            .context("concurrent metering projection disappeared")?;
        ensure!(serde_json::to_string(&stored)? == serde_json::to_string(&record)?);
        ensure!(!metering.apply_in(&tx, &record).await?);
        tx.commit().await?;

        let mut event_collision = record.clone();
        rename(
            &mut event_collision,
            &format!("event-collision-{}", uuid::Uuid::new_v4()),
        );
        event_collision
            .event
            .event_id
            .clone_from(&record.event.event_id);
        reject(
            first,
            &metering,
            &event_collision,
            "conflicting metering projection owner",
        )
        .await?;
        if record.request_id.is_some() {
            let mut request_collision = record.clone();
            rename(
                &mut request_collision,
                &format!("request-collision-{}", uuid::Uuid::new_v4()),
            );
            request_collision.request_id.clone_from(&record.request_id);
            reject(
                first,
                &metering,
                &request_collision,
                "conflicting metering projection owner",
            )
            .await?;
        }
        let mut foreign = record.clone();
        foreign.principal_id = "another-principal".into();
        reject(
            first,
            &metering,
            &foreign,
            "conflicting metering projection owner",
        )
        .await?;
        let mut changed = record.clone();
        changed.event.execution.revision += 1;
        reject(
            first,
            &metering,
            &changed,
            "conflicting immutable metering projection",
        )
        .await?;
        ensure!(snapshot(first, &record).await? == before);
    }
    Ok(())
}

async fn write_together(
    store: &Store,
    metering: &SharedMetering,
    record: &Record,
    barrier: &Barrier,
) -> Result<bool> {
    let mut first_attempt = true;
    store
        .retry(|| {
            let initial = std::mem::take(&mut first_attempt);
            async move {
                let tx = store.begin().await?;
                if initial {
                    ensure!(
                        metering
                            .get_in(
                                &tx,
                                &record.event.tenant_id,
                                &record.principal_id,
                                &record.event.execution.execution_id,
                            )
                            .await?
                            .is_none(),
                        "first writer snapshot was not empty"
                    );
                    barrier.wait().await;
                }
                let inserted = metering.apply_in(&tx, record).await?;
                tx.commit().await?;
                Ok(inserted)
            }
        })
        .await
}

async fn reject(
    store: &Store,
    metering: &SharedMetering,
    record: &Record,
    expected: &str,
) -> Result<()> {
    record.accounting()?;
    let tx = store.begin().await?;
    let result = metering.apply_in(&tx, record).await;
    tx.rollback().await?;
    let error = result
        .err()
        .context("conflicting metering evidence was accepted")?;
    ensure!(
        error.to_string() == expected,
        "unexpected rejection: {error:#}"
    );
    Ok(())
}

async fn snapshot(store: &Store, record: &Record) -> Result<String> {
    let tx = store.begin().await?;
    // Compare every persisted column, including denormalized accounting.
    // https://www.postgresql.org/docs/17/functions-json.html
    let row = tx
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT to_jsonb(m)::text AS snapshot
             FROM gateway_shared_metering_requests m
             WHERE namespace=$1 AND execution_id=$2",
            [
                store.namespace().into(),
                record.event.execution.execution_id.clone().into(),
            ],
        ))
        .await?
        .context("metering row missing after replay")?;
    let snapshot = row.try_get("", "snapshot")?;
    tx.commit().await?;
    Ok(snapshot)
}
