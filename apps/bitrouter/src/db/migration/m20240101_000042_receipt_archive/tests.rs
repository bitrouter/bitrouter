use super::*;
use anyhow::{Result, ensure};
use sea_orm::{DatabaseConnection, Statement};

async fn schema(db: &DatabaseConnection) -> Result<Vec<(String, String, String)>> {
    db.query_all(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT 'main' AS owner,name,coalesce(sql,'') AS sql FROM main.sqlite_schema UNION ALL SELECT 'temp' AS owner,name,coalesce(sql,'') AS sql FROM temp.sqlite_schema ORDER BY owner,name"
    )).await?.into_iter().map(|row| Ok((row.try_get("", "owner")?, row.try_get("", "name")?, row.try_get("", "sql")?))).collect()
}

#[tokio::test]
async fn current_archive_catalog_requires_complete_format_and_location_metadata() -> Result<()> {
    for corrupt in [
        "ALTER TABLE tenancy_execution_receipts DROP COLUMN receipt_format",
        "DROP INDEX idx_tenancy_receipts_storage",
        "DROP TABLE tenancy_receipt_archive",
        "CREATE TEMP VIEW tenancy_execution_receipts AS SELECT 1",
        "CREATE TRIGGER rewrite_usage AFTER INSERT ON tenancy_usage_evidence BEGIN DELETE FROM tenancy_usage_evidence; END",
    ] {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        validate(&db).await?;
        super::super::validate_receipt_archive(&db).await?;
        db.execute_unprepared(corrupt).await?;
        let before = schema(&db).await?;
        ensure!(validate(&db).await.is_err(), "accepted {corrupt}");
        ensure!(super::super::validate_receipt_archive(&db).await.is_err());
        ensure!(usage::validate(&db).await.is_err());
        ensure!(schema(&db).await? == before, "validator changed {corrupt}");
        db.close().await?;
    }
    Ok(())
}
