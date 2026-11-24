use super::*;
use sea_orm::TransactionTrait;

/// Losing the durable epoch refuses bootstrap before an explicit import.
#[tokio::test]
async fn epoch_requirement_survives_deleted_epoch_and_empty_history() -> Result<()> {
    let db = database().await?;
    let initial = store::load(&db, chrono::Utc::now()).await?;
    let installation = initial.installation.clone();
    let key: [u8; 32] = rand::random();
    let resources = Arc::new(Resources::with_key(installation.clone(), key)?);
    let service = Service::start_prepared(db.clone(), None, initial, resources.clone()).await?;
    service.shutdown().await?;
    ensure!(crate::db::migration::capacity::epoch_required(&db).await?);
    db.execute_unprepared("DELETE FROM tenancy_continuation_epoch")
        .await?;
    let import = serde_json::to_vec(&serde_json::json!({
        "format":1, "installation_id":installation, "resources":[{
            "reference":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"credential","resource_id":"slot","revision_id":"r1"},
            "definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"must-not-be-imported"}}}
        }]
    }))?;
    let mut other_key = key;
    other_key[0] ^= 1;
    for key in [key, other_key] {
        let initial = store::initial_header(&db, chrono::Utc::now()).await?;
        let resources = Arc::new(Resources::with_key(installation.clone(), key)?);
        ensure!(
            Service::start_resources(
                db.clone(),
                None,
                initial,
                resources,
                Some(import.clone()),
                None
            )
            .await
            .is_err()
        );
        ensure!(!has_epoch(&db).await?);
        ensure!(
            db.query_one(sea_orm::Statement::from_string(
                db.get_database_backend(),
                "SELECT 1 FROM tenancy_resource_revisions LIMIT 1"
            ))
            .await?
            .is_none()
        );
    }
    db.close().await?;
    Ok(())
}

/// Trajectory history inspection remains explicit and read-only.
#[tokio::test]
async fn trajectory_history_corruption_refuses_audit_without_blocking_startup() -> Result<()> {
    let db = database().await?;
    let initial = store::initial_header(&db, chrono::Utc::now()).await?;
    let resources = Arc::new(Resources::with_key(
        initial.installation.clone(),
        rand::random(),
    )?);
    let service = Service::start_prepared(db.clone(), None, initial, resources.clone()).await?;
    let header = service
        .current()
        .map_err(|error| anyhow::anyhow!("{:?}", error.0.code))?
        .header();
    service.shutdown().await?;
    db.execute(sea_orm::Statement::from_sql_and_values(db.get_database_backend(),
        "INSERT INTO trajectory_outbox(outbox_id,owner_user_id,topic,payload_json,payload_digest,created_at,attempts,delivered_at,installation_id) VALUES('corrupt-owned','owner','fixture','{}',?,'2026-01-01T00:00:00Z',1,'2026-01-01T00:00:00Z',?)",
        ["a".repeat(64).into(), header.installation.as_str().into()])).await?;
    let initial = store::initial_header(&db, chrono::Utc::now()).await?;
    let service = Service::start_prepared(db.clone(), None, initial, resources.clone()).await?;
    let tx = db.begin().await?;
    let audit = audit_in(&tx, &header, &resources, 64 * 1024 * 1024, &|| Ok(())).await;
    tx.rollback().await?;
    ensure!(audit.is_err());
    ensure!(service.current().is_ok());
    let row = db.query_one(sea_orm::Statement::from_string(db.get_database_backend(),
        "SELECT installation_id,tenant_id,delivered_at FROM trajectory_outbox WHERE outbox_id='corrupt-owned'"))
        .await?.context("audit removed history")?;
    ensure!(row.try_get::<String>("", "installation_id")? == header.installation.as_str());
    ensure!(row.try_get::<Option<String>>("", "tenant_id")?.is_none());
    ensure!(row.try_get::<String>("", "delivered_at")? == "2026-01-01T00:00:00Z");
    service.shutdown().await?;
    db.close().await?;
    Ok(())
}
