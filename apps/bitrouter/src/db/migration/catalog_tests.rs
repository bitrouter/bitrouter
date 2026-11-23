use crate::db::migration::validate_receipt_archive as validate;
use anyhow::{Result, ensure};
use sea_orm::{ConnectionTrait, DatabaseBackend};
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
        crate::db::migration::validate_receipt_archive(&db).await?;
        db.execute_unprepared(corrupt).await?;
        let before = schema(&db).await?;
        ensure!(validate(&db).await.is_err(), "accepted {corrupt}");
        ensure!(
            crate::db::migration::validate_receipt_archive(&db)
                .await
                .is_err()
        );
        ensure!(
            crate::db::migration::validate_usage_evidence(&db)
                .await
                .is_err()
        );
        ensure!(schema(&db).await? == before, "validator changed {corrupt}");
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn current_usage_catalog_refuses_shadow_without_writes() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    validate(&db).await?;
    db.execute_unprepared("CREATE TEMP VIEW tenancy_usage_evidence AS SELECT 1")
        .await?;
    ensure!(validate(&db).await.is_err());
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn trigger_or_nonbinary_shape_cannot_be_served_as_immutable_evidence() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    db.execute_unprepared("CREATE TRIGGER rewrite_usage AFTER INSERT ON tenancy_usage_evidence BEGIN DELETE FROM tenancy_usage_evidence; END").await?;
    ensure!(validate(&db).await.is_err());
    db.execute_unprepared("DROP TRIGGER rewrite_usage").await?;
    db.execute_unprepared("DROP TABLE tenancy_usage_evidence")
        .await?;
    db.execute_unprepared("CREATE TABLE tenancy_usage_evidence(installation_id TEXT COLLATE NOCASE, tenant_id TEXT, execution_id TEXT)").await?;
    ensure!(validate(&db).await.is_err());
    db.close().await?;
    Ok(())
}
