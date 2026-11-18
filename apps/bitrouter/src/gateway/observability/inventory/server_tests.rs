//! Real database locking and visibility checks. Each campaign uses fresh request IDs.
use super::*;
use anyhow::{Context, ensure};

use std::time::Duration;

async fn database(scheme: &str) -> Result<DatabaseConnection> {
    let source =
        std::env::var("BITROUTER_TEST_DATABASE_URL").context("dedicated test database required")?;
    let url = url::Url::parse(&source)?;
    ensure!(
        url.scheme() == scheme && !url.path().trim_matches('/').is_empty(),
        "expected dedicated {scheme} database"
    );
    let db = crate::db::connect(&source).await?;
    crate::db::run_migrations(&db).await?;
    Ok(db)
}

async fn cleanup(
    db: DatabaseConnection,
    store: &EvolutionStore,
    key: &str,
    result: Result<()>,
) -> Result<()> {
    let deleted = records::Entity::delete_by_id(store.id(INVENTORY_KIND, key)?)
        .exec(&db)
        .await;
    let closed = db.close().await;
    result?;
    deleted?;
    closed?;
    Ok(())
}

async fn parallel_admission(scheme: &str) -> Result<()> {
    let db = database(scheme).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let first = GatewayInventory::new(db.clone());
    let second = GatewayInventory::new(db.clone());
    let store = first.service_store()?;
    let key = digest(&(&id, &id))?;
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let (one, two) = tokio::join!(
            first.begin(&id, "first", &id),
            second.begin(&id, "second", &id)
        );
        let winner = match (one?, two?) {
            (BeginResult::Tracked(row), BeginResult::Duplicate)
            | (BeginResult::Duplicate, BeginResult::Tracked(row)) => row,
            _ => anyhow::bail!("expected exactly one admitted attempt"),
        };
        let saved = GatewayInventory::new(db.clone())
            .request(&id, &id)
            .await?
            .context("no stored admission")?;
        ensure!(saved.attempt_id == winner.attempt_id && saved.binding.is_none());
        ensure!(matches!(
            second.begin(&id, "restart", &id).await?,
            BeginResult::Duplicate
        ));
        Ok(())
    })
    .await
    .context("parallel admission timed out")
    .and_then(|r| r);
    cleanup(db, &store, &key, result).await
}

#[cfg(feature = "db-postgres")]
#[tokio::test]
#[ignore = "requires a dedicated PostgreSQL BITROUTER_TEST_DATABASE_URL"]
async fn postgres_competing_gateway_controllers_share_one_durable_reservation() -> Result<()> {
    parallel_admission("postgres").await
}

#[cfg(feature = "db-mysql")]
mod mysql_tests {
    use super::*;
    use sea_orm::{IsolationLevel, TransactionTrait};

    #[tokio::test]
    #[ignore = "requires a dedicated MySQL BITROUTER_TEST_DATABASE_URL"]
    async fn competing_gateway_controllers_share_one_durable_reservation() -> Result<()> {
        parallel_admission("mysql").await
    }

    #[tokio::test]
    #[ignore = "requires a dedicated MySQL BITROUTER_TEST_DATABASE_URL"]
    async fn locked_gateway_inventory_sees_new_and_changed_rows_after_older_snapshots() -> Result<()>
    {
        let db = database("mysql").await?;
        let inventory = GatewayInventory::new(db.clone());
        let id = uuid::Uuid::new_v4().to_string();
        let store = inventory.service_store()?;
        let key = digest(&(&id, &id))?;
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            for initial in [None, Some(0)] {
                let reader = db
                    .begin_with_config(Some(IsolationLevel::RepeatableRead), None)
                    .await?;
                let old = store
                    .get_in::<GatewayRequest>(&reader, INVENTORY_KIND, &key)
                    .await?;
                assert_eq!(old.as_ref().map(|(rev, _)| *rev), initial);
                if let Some((_, row)) = old {
                    inventory
                        .settle(
                            &row,
                            ExecutionSettlement {
                                settled_at: "now".into(),
                                hops: vec![],
                                final_model: "model".into(),
                                final_provider: "provider".into(),
                                error_code: None,
                                duration_ms: 1,
                                outcome: None,
                                final_metered_cost_micro_usd: Some(1),
                                total_cost_micro_usd: Some(1),
                            },
                        )
                        .await?;
                } else {
                    ensure!(matches!(
                        inventory.begin(&id, "controller", &id).await?,
                        BeginResult::Tracked(_)
                    ));
                }
                let snapshot = store
                    .get_in::<GatewayRequest>(&reader, INVENTORY_KIND, &key)
                    .await?;
                assert_eq!(snapshot.map(|(rev, _)| rev), initial);
                let (rev, row) = store
                    .get_current_in::<GatewayRequest>(&reader, INVENTORY_KIND, &key)
                    .await?
                    .context("current row missing")?;
                reader.rollback().await?;
                assert_eq!(rev, initial.map_or(0, |n| n + 1));
                assert_eq!(row.settlement.is_some(), initial.is_some());
            }
            Ok(())
        })
        .await
        .context("RR snapshot proof timed out")
        .and_then(|r| r);
        cleanup(db, &store, &key, result).await
    }
}
