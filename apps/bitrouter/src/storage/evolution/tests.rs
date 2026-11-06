use super::*;
use bitrouter_ai::tenancy::operation::ErrorCode;
use sea_orm::DbErr;
use serde_json::{Value, json};

async fn fixture() -> Result<(EvolutionStore, crate::tenancy::Service)> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let service = crate::tenancy::Service::open(db.clone(), None).await?;
    let cell = Arc::new(OnceLock::new());
    cell.set(service.storage_budget())
        .map_err(|_| anyhow::anyhow!("budget already initialized"))?;
    Ok((
        EvolutionStore::new(db, "read-failure-fixture")?.with_storage_budget(Some(cell)),
        service,
    ))
}

fn healthy(service: &crate::tenancy::Service) -> Result<()> {
    service
        .storage_budget()
        .reserve(0, false)
        .map_err(|code| anyhow::anyhow!("unexpected reservation refusal: {code:?}"))?
        .complete()
        .map_err(|code| anyhow::anyhow!("unexpected release failure: {code:?}"))
}

#[tokio::test]
async fn public_select_failure_fences_later_writes_before_any_effect() -> Result<()> {
    for list in [false, true] {
        let (store, service) = fixture().await?;
        store
            .initialize("record", "key", None, &json!({"value": 1}))
            .await?;
        store
            .db
            .execute_unprepared("ALTER TABLE checkpoint_evolution RENAME TO hidden_evolution")
            .await?;
        let result = if list {
            store.list::<Value>("record").await.map(|_| ())
        } else {
            store.get::<Value>("record", "key").await.map(|_| ())
        };
        let error = result.err().context("failing SELECT was accepted")?;
        ensure!(error.to_string() == "internal error: local storage SQL failed");
        ensure!(matches!(
            service.storage_budget().reserve(0, false),
            Err(ErrorCode::StorageUnavailable)
        ));
        // Restore a usable SQL table: the next refusal must come from shared
        // storage health, not from the still-missing table.
        store
            .db
            .execute_unprepared("ALTER TABLE hidden_evolution RENAME TO checkpoint_evolution")
            .await?;
        let mut changed = false;
        ensure!(
            store
                .update("record", "key", 0, |value: &mut Value| {
                    changed = true;
                    *value = json!({"value": 2});
                    Ok(())
                })
                .await
                .is_err()
        );
        ensure!(!changed);
        ensure!(store.get::<Value>("record", "key").await? == Some((0, json!({"value": 1}))));
        service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn missing_decode_and_business_errors_leave_storage_healthy() -> Result<()> {
    let (store, service) = fixture().await?;
    ensure!(store.get::<Value>("record", "missing").await?.is_none());
    ensure!(store.list::<Value>("record").await?.is_empty());
    healthy(&service)?;
    store
        .initialize("record", "key", None, &json!({"value": 1}))
        .await?;
    ensure!(store.get::<u64>("record", "key").await.is_err());
    ensure!(store.list::<u64>("record").await.is_err());
    ensure!(store.ensure_owner("another-owner").is_err());
    healthy(&service)?;
    let rejected: Result<(i64, ())> = store
        .update("record", "key", 0, |value: &mut Value| {
            *value = json!({"value": 2});
            anyhow::bail!("application rejected mutation")
        })
        .await;
    ensure!(rejected.is_err());
    ensure!(store.get::<Value>("record", "key").await? == Some((0, json!({"value": 1}))));
    healthy(&service)?;
    service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn borrowed_select_failure_stays_with_the_transaction_owner() -> Result<()> {
    let (store, service) = fixture().await?;
    store
        .db
        .execute_unprepared("ALTER TABLE checkpoint_evolution RENAME TO hidden_evolution")
        .await?;
    let (write, tx) = store.begin_write(false).await?;
    let result = store.get_in::<Value>(&tx, "record", "key").await;
    ensure!(
        result
            .as_ref()
            .err()
            .is_some_and(|e| e.downcast_ref::<DbErr>().is_some())
    );
    healthy(&service)?;
    ensure!(write.finish(tx, result).await.is_err());
    ensure!(matches!(
        service.storage_budget().reserve(0, false),
        Err(ErrorCode::StorageUnavailable)
    ));
    store
        .db
        .execute_unprepared("ALTER TABLE hidden_evolution RENAME TO checkpoint_evolution")
        .await?;
    service.shutdown().await?;
    Ok(())
}
