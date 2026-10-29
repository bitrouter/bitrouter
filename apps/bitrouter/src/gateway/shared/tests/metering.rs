//! Shared domain storage with colliding legacy and namespace identities.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::execution::ResolutionOutcome;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

use super::super::{schema, store};
use crate::metering::shared::{
    SharedMetering,
    tests::{boundary_record, fixture, no_dispatch, rename, resolve, signed_zero},
};
use store::Store;

#[path = "metering/replay.rs"]
mod replay;

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL for a dedicated PostgreSQL test database"]
async fn postgres_shared_metering() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), exercise())
        .await
        .context("shared metering regression deadline exceeded")?
}

async fn exercise() -> Result<()> {
    let url = std::env::var("BITROUTER_TEST_DATABASE_URL")?;
    let db = store::connect(&url, 3, Duration::from_secs(3)).await?;
    let other_db = store::connect(&url, 3, Duration::from_secs(3)).await?;
    let namespace = format!("metering-{}", uuid::Uuid::new_v4());
    let first = Store::new(db.clone(), &namespace, Duration::from_secs(3))?;
    let second = Store::new(other_db.clone(), &namespace, Duration::from_secs(3))?;
    let result = async {
        run(&first, &second).await?;
        exact_and_bounded(&first, &second).await
    }
    .await;
    let closed = db.close().await;
    let other_closed = other_db.close().await;
    result?;
    closed?;
    other_closed?;
    Ok(())
}

async fn run(first: &Store, second: &Store) -> Result<()> {
    schema::initialize_all(first).await?;
    let metering = SharedMetering::new(first.namespace())?;
    let other = SharedMetering::new(&format!("metering-{}", uuid::Uuid::new_v4()))?;
    let record = fixture()?;
    let id = &record.event.execution.execution_id;
    let request_id = record
        .request_id
        .as_ref()
        .context("missing canonical request")?;
    let tx = first.begin().await?;
    ensure!(
        metering
            .get_in(&tx, "tenant", "principal", id)
            .await?
            .is_none()
    );
    ensure!(
        metering
            .spend_in(&tx, "tenant", None, 0, 3000)
            .await?
            .complete_total_micro_usd
            == Some(0)
    );
    ensure!(metering.apply_in(&tx, &record).await?);
    tx.rollback().await?;
    let tx = second.begin().await?;
    ensure!(
        metering
            .get_in(&tx, "tenant", "principal", id)
            .await?
            .is_none()
    );
    tx.commit().await?;

    let (a, b) = tokio::join!(
        write(first, &metering, &record),
        write(second, &metering, &record)
    );
    ensure!(u8::from(a?) + u8::from(b?) == 1);
    let tx = second.begin().await?;
    ensure!(metering.get_in(&tx, "tenant", "principal", id).await? == Some(record.clone()));
    ensure!(!metering.apply_in(&tx, &record).await?);
    ensure!(
        metering
            .get_in(&tx, "other", "principal", id)
            .await?
            .is_none()
    );
    ensure!(metering.get_in(&tx, "tenant", "other", id).await?.is_none());
    let spend = metering
        .spend_in(&tx, "tenant", Some("principal"), 1000, 1001)
        .await?;
    ensure!(spend.executions == 1 && spend.complete_total_micro_usd == Some(209));
    ensure!(
        metering
            .spend_in(&tx, "tenant", None, 1001, 3000)
            .await?
            .executions
            == 0
    );
    ensure!(
        metering
            .spend_in(&tx, "tenant", Some("other"), 0, 3000)
            .await?
            .executions
            == 0
    );
    let mut resolved = record.clone();
    resolve(&mut resolved, ResolutionOutcome::Terminated)?;
    ensure!(other.apply_in(&tx, &resolved).await?);
    let spend = other.spend_in(&tx, "tenant", None, 0, 3000).await?;
    ensure!(spend.known_subtotal_micro_usd == 19 && spend.unknown_attempts == 1);
    ensure!(spend.complete_total_micro_usd.is_none());
    ensure!(
        metering
            .spend_in(&tx, "tenant", None, 0, 3000)
            .await?
            .complete_total_micro_usd
            == Some(209)
    );
    // A legacy local request can carry the very same canonical request identity.
    // No shared query may see it and shared projection may not overwrite it.
    tx.execute(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "INSERT INTO requests (request_id,user_id,api_key_id,model_id,provider_id,
         prompt_tokens,completion_tokens,reasoning_tokens,estimated_charge_micro_usd,created_at)
         VALUES ($1,'tenant','principal','legacy-model','legacy-provider',1,1,0,999,'fixture')",
        [request_id.clone().into()],
    ))
    .await?;
    tx.commit().await?;

    let tx = first.begin().await?;
    for outcome in [
        ResolutionOutcome::NotAccepted,
        ResolutionOutcome::Terminated,
    ] {
        let mut partial = record.clone();
        rename(&mut partial, &format!("partial-{}", uuid::Uuid::new_v4()));
        resolve(&mut partial, outcome)?;
        ensure!(metering.apply_in(&tx, &partial).await?);
    }
    let mut empty = record.clone();
    rename(&mut empty, &format!("empty-{}", uuid::Uuid::new_v4()));
    no_dispatch(&mut empty)?;
    ensure!(metering.apply_in(&tx, &empty).await?);
    let stored = metering
        .get_in(
            &tx,
            "tenant",
            "principal",
            &empty.event.execution.execution_id,
        )
        .await?
        .context("no-dispatch projection missing")?;
    ensure!(stored.accounting()?.usage.cost_micro_usd.is_none());
    let spend = metering.spend_in(&tx, "tenant", None, 1000, 1001).await?;
    ensure!(spend.executions == 4 && spend.known_subtotal_micro_usd == 247);
    ensure!(spend.unknown_attempts == 2 && spend.complete_total_micro_usd.is_none());
    tx.commit().await?;

    let mut changed = record.clone();
    changed.event.execution.revision += 1;
    let tx = first.begin().await?;
    ensure!(metering.apply_in(&tx, &changed).await.is_err());
    tx.rollback().await?;
    let tx = first.begin().await?;
    ensure!(metering.apply_in(&tx, &resolved).await.is_err());
    tx.rollback().await?;
    let mut collision = record.clone();
    rename(&mut collision, &format!("new-{}", uuid::Uuid::new_v4()));
    collision.request_id = record.request_id.clone();
    let tx = first.begin().await?;
    ensure!(metering.apply_in(&tx, &collision).await.is_err());
    tx.rollback().await?;

    let tx = second.begin().await?;
    ensure!(metering.get_in(&tx, "tenant", "principal", id).await? == Some(record.clone()));
    let local = tx
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT estimated_charge_micro_usd,model_id FROM requests WHERE request_id=$1",
            [request_id.clone().into()],
        ))
        .await?
        .context("legacy row disappeared")?;
    ensure!(local.try_get::<i64>("", "estimated_charge_micro_usd")? == 999);
    ensure!(local.try_get::<String>("", "model_id")? == "legacy-model");
    // An inconsistent denormalized summary is rejected on an exact read.
    tx.execute(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "UPDATE gateway_shared_metering_requests SET known_subtotal_micro_usd=210,
         complete_total_micro_usd=210 WHERE namespace=$1 AND execution_id=$2",
        [first.namespace().into(), id.clone().into()],
    ))
    .await?;
    ensure!(
        metering
            .get_in(&tx, "tenant", "principal", id)
            .await
            .is_err()
    );
    tx.rollback().await?;
    Ok(())
}

async fn write(
    store: &Store,
    metering: &SharedMetering,
    record: &crate::metering::shared::record::Record,
) -> Result<bool> {
    store
        .retry(|| async {
            let tx = store.begin().await?;
            let inserted = metering.apply_in(&tx, record).await?;
            tx.commit().await?;
            Ok(inserted)
        })
        .await
}

async fn exact_and_bounded(first: &Store, second: &Store) -> Result<()> {
    let metering = SharedMetering::new(first.namespace())?;
    let mut positive = fixture()?;
    signed_zero(&mut positive, 0.0)?;
    let mut negative = positive.clone();
    signed_zero(&mut negative, -0.0)?;
    positive.accounting()?;
    negative.accounting()?;
    ensure!(
        positive == negative,
        "fixture must exercise floating-point equality"
    );
    let original = serde_json::to_string(&positive)?;
    ensure!(original != serde_json::to_string(&negative)?);
    let tx = first.begin().await?;
    ensure!(metering.apply_in(&tx, &positive).await?);
    tx.commit().await?;
    let tx = second.begin().await?;
    ensure!(metering.apply_in(&tx, &negative).await.is_err());
    tx.rollback().await?;
    let tx = second.begin().await?;
    ensure!(!metering.apply_in(&tx, &positive).await?);
    let stored = metering
        .get_in(
            &tx,
            "tenant",
            "principal",
            &positive.event.execution.execution_id,
        )
        .await?
        .context("missing signed-zero fixture")?;
    ensure!(serde_json::to_string(&stored)? == original);
    tx.commit().await?;

    let boundary = boundary_record()?;
    let tx = first.begin().await?;
    ensure!(metering.apply_in(&tx, &boundary).await?);
    tx.commit().await?;
    let tx = second.begin().await?;
    let stored = metering
        .get_in(
            &tx,
            "tenant",
            "principal",
            &boundary.event.execution.execution_id,
        )
        .await?
        .context("missing boundary projection")?;
    ensure!(
        stored.attempts.len() == 32
            && serde_json::to_string(&stored)? == serde_json::to_string(&boundary)?
    );
    ensure!(!metering.apply_in(&tx, &boundary).await?);
    tx.commit().await?;

    let mut oversized = fixture()?;
    oversized.event.execution.correlation_id = "x".repeat(6_701_056 + 1);
    let tx = first.begin().await?;
    let error = metering
        .apply_in(&tx, &oversized)
        .await
        .err()
        .context("oversized record was accepted")?;
    ensure!(error.to_string() == "metering record exceeds byte limit");
    ensure!(
        metering
            .get_in(
                &tx,
                "tenant",
                "principal",
                &oversized.event.execution.execution_id
            )
            .await?
            .is_none()
    );
    tx.commit().await?;
    Ok(())
}
