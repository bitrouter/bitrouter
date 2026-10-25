use super::*;
use anyhow::{Result, ensure};
use sea_orm::{
    DatabaseConnection,
    sea_query::{Order, Query},
};
#[cfg(any(feature = "db-postgres", feature = "db-mysql"))]
mod server;

async fn sqlite() -> Result<DatabaseConnection> {
    Ok(crate::db::connect("sqlite::memory:").await?)
}
async fn saved(db: &DatabaseConnection) -> Result<Vec<Vec<String>>> {
    let query = Query::select()
        .columns(
            [
                "installation_id",
                "tenant_id",
                "execution_id",
                "principal_id",
                "correlation_id",
                "receipt_json",
                "content_digest",
                "disposition_json",
                "recorded_at",
            ]
            .map(Alias::new),
        )
        .from(Alias::new(TABLE))
        .order_by(Alias::new("execution_id"), Order::Asc)
        .to_owned();
    db.query_all(db.get_database_backend().build(&query))
        .await?
        .iter()
        .map(|row| {
            [
                "installation_id",
                "tenant_id",
                "execution_id",
                "principal_id",
                "correlation_id",
                "receipt_json",
                "content_digest",
                "disposition_json",
                "recorded_at",
            ]
            .into_iter()
            .map(|column| row.try_get::<String>("", column).map_err(Into::into))
            .collect()
        })
        .collect()
}
async fn seed(db: &DatabaseConnection) -> Result<()> {
    let values: Vec<(&str, Value)> = vec![
        ("installation_id", "I".into()),
        ("tenant_id", "Tenant".into()),
        ("execution_id", "A".into()),
        ("principal_id", "Creator".into()),
        ("correlation_id", "Correlation".into()),
        (
            "receipt_json",
            "{ \"opaque\": \"historical bytes\" }".into(),
        ),
        ("content_digest", "d".repeat(64).into()),
        ("disposition_json", "{}".into()),
        ("byte_count", 32_i64.into()),
        ("unknown_outcome", false.into()),
        ("recorded_at", "2026-10-25T00:00:00Z".into()),
    ];
    let mut query = Query::insert();
    query
        .into_table(Alias::new(TABLE))
        .columns(values.iter().map(|(name, _)| Alias::new(*name)))
        .values(values.into_iter().map(|(_, value)| value.into()))?;
    db.execute(db.get_database_backend().build(&query)).await?;
    Ok(())
}
async fn schema(db: &DatabaseConnection) -> Result<Vec<(String, String, String)>> {
    db.query_all(Statement::from_string(
        DatabaseBackend::Sqlite,
        "SELECT type,name,coalesce(sql,'') AS sql FROM sqlite_schema ORDER BY type,name",
    ))
    .await?
    .into_iter()
    .map(|row| {
        Ok((
            row.try_get("", "type")?,
            row.try_get("", "name")?,
            row.try_get("", "sql")?,
        ))
    })
    .collect()
}
#[tokio::test]
async fn sqlite_restart_partial_index_and_populated_down_preserve_bytes() -> Result<()> {
    let db = sqlite().await?;
    let manager = SchemaManager::new(&db);
    Migration.up(&manager).await?;
    seed(&db).await?;
    let before = saved(&db).await?;
    Migration.up(&manager).await?;
    ensure!(saved(&db).await? == before);
    manager
        .drop_index(
            Index::drop()
                .name(INDEX)
                .table(Alias::new(TABLE))
                .to_owned(),
        )
        .await?;
    Migration.up(&manager).await?;
    ensure!(saved(&db).await? == before);
    let full = schema(&db).await?;
    ensure!(Migration.down(&manager).await.is_err());
    ensure!(schema(&db).await? == full && saved(&db).await? == before);
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn sqlite_incompatible_last_index_is_rejected_before_any_ddl() -> Result<()> {
    let db = sqlite().await?;
    let manager = SchemaManager::new(&db);
    Migration.up(&manager).await?;
    seed(&db).await?;
    db.execute_unprepared("DROP INDEX idx_tenancy_receipts_correlation")
        .await?;
    db.execute_unprepared("CREATE INDEX idx_tenancy_receipts_correlation ON tenancy_execution_receipts(installation_id,tenant_id,correlation_id,execution_id DESC)").await?;
    let schema_before = schema(&db).await?;
    let rows_before = saved(&db).await?;
    ensure!(Migration.up(&manager).await.is_err());
    ensure!(schema(&db).await? == schema_before && saved(&db).await? == rows_before);
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn sqlite_name_collision_and_external_dependencies_leave_whole_schema_unchanged() -> Result<()>
{
    let db = sqlite().await?;
    let manager = SchemaManager::new(&db);
    db.execute_unprepared("CREATE TABLE other_rows(id INTEGER)")
        .await?;
    db.execute_unprepared("CREATE INDEX idx_tenancy_receipts_correlation ON other_rows(id)")
        .await?;
    let before = schema(&db).await?;
    ensure!(Migration.up(&manager).await.is_err());
    ensure!(schema(&db).await? == before);
    db.execute_unprepared("DROP INDEX idx_tenancy_receipts_correlation")
        .await?;
    Migration.up(&manager).await?;
    for ddl in [
        "CREATE VIEW external_receipt_view AS SELECT * FROM tenancy_execution_receipts",
        "CREATE TRIGGER external_receipt_trigger AFTER INSERT ON other_rows BEGIN DELETE FROM tenancy_execution_receipts; END",
    ] {
        db.execute_unprepared(ddl).await?;
        let before = schema(&db).await?;
        ensure!(Migration.down(&manager).await.is_err());
        ensure!(schema(&db).await? == before);
        if ddl.contains("VIEW") {
            db.execute_unprepared("DROP VIEW external_receipt_view")
                .await?;
        } else {
            db.execute_unprepared("DROP TRIGGER external_receipt_trigger")
                .await?;
        }
    }
    Migration.down(&manager).await?;
    ensure!(!manager.has_table(TABLE).await?);
    db.close().await?;
    Ok(())
}
