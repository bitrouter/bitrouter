use super::*;
use anyhow::{Context, Result, ensure};
use sea_orm::{DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;

use super::super::Migrator;

struct ThroughArchive;

#[async_trait::async_trait]
impl MigratorTrait for ThroughArchive {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        let mut migrations = Vec::new();
        for migration in Migrator::migrations() {
            let last = migration.name() == Migration.name();
            migrations.push(migration);
            if last {
                break;
            }
        }
        migrations
    }
}

async fn run_archive_migration(db: &DatabaseConnection) -> Result<()> {
    // Keep the production DDL/ledger transaction boundary, but stop at this
    // migration: later source backfills must reject these opaque legacy fixtures.
    let tx = db.begin().await?;
    match ThroughArchive::up(&tx, None).await {
        Ok(()) => tx.commit().await?,
        Err(error) => {
            tx.rollback().await?;
            return Err(error.into());
        }
    }
    Ok(())
}

async fn previous() -> Result<DatabaseConnection> {
    let db = crate::db::connect("sqlite::memory:").await?;
    let index = Migrator::migrations()
        .iter()
        .position(|m| m.name() == Migration.name())
        .context("archive migration registration")?;
    Migrator::up(&db, Some(index.try_into()?)).await?;
    Ok(db)
}

async fn seed(db: &DatabaseConnection) -> Result<()> {
    // Opaque historical bytes deliberately cannot be canonicalized as current
    // receipt JSON. Schema migration must neither parse nor repair this data.
    db.execute_unprepared("INSERT INTO tenancy_execution_receipts
        (installation_id,tenant_id,execution_id,principal_id,correlation_id,receipt_json,content_digest,disposition_json,byte_count,unknown_outcome,recorded_at)
        VALUES ('I','Tenant','E','P','C','{ \"historical\": 1 }',printf('%064d',7),'{ \"private\": 2 }',987,0,'2026-11-01T00:00:00Z'),
        ('I','tenant','E','P2','C','opaque',printf('%064d',8),'private',654,1,'2026-11-01T01:00:00Z')").await?;
    db.execute_unprepared("INSERT INTO tenancy_usage_evidence
        (installation_id,tenant_id,execution_id,schema_version,receipt_digest,evidence_digest,evidence_json,byte_count)
        VALUES ('I','Tenant','E',1,printf('%064d',7),printf('%064d',9),'{ \"original\": 3 }',321)").await?;
    Ok(())
}

async fn saved(db: &DatabaseConnection) -> Result<Vec<String>> {
    let mut result = Vec::new();
    for query in [
        "SELECT json_array(installation_id,tenant_id,execution_id,principal_id,correlation_id,receipt_json,content_digest,disposition_json,byte_count,unknown_outcome,recorded_at) AS original FROM tenancy_execution_receipts ORDER BY installation_id,tenant_id,execution_id",
        "SELECT json_array(installation_id,tenant_id,execution_id,schema_version,receipt_digest,evidence_digest,evidence_json,byte_count) AS original FROM tenancy_usage_evidence ORDER BY installation_id,tenant_id,execution_id",
    ] {
        for row in db
            .query_all(Statement::from_string(DatabaseBackend::Sqlite, query))
            .await?
        {
            result.push(row.try_get("", "original")?);
        }
    }
    Ok(result)
}

async fn schema(db: &DatabaseConnection) -> Result<Vec<(String, String, String)>> {
    db.query_all(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT 'main' AS owner,name,coalesce(sql,'') AS sql FROM main.sqlite_schema UNION ALL SELECT 'temp' AS owner,name,coalesce(sql,'') AS sql FROM temp.sqlite_schema ORDER BY owner,name"
    )).await?.into_iter().map(|r| Ok((r.try_get("", "owner")?, r.try_get("", "name")?, r.try_get("", "sql")?))).collect()
}

async fn validate_current(db: &DatabaseConnection) -> Result<()> {
    validate(db).await?;
    super::super::validate_receipt_archive(db).await?;
    usage::validate(db).await?;
    Ok(())
}

#[tokio::test]
async fn upgrade_preserves_all_original_fields_and_only_initializes_inline_locations() -> Result<()>
{
    let db = previous().await?;
    seed(&db).await?;
    let original = saved(&db).await?;
    receipts::validate_legacy(&db).await?;
    usage::validate_legacy(&db).await?;
    super::super::validate_receipt_archive(&db).await?;
    usage::validate(&db).await?;
    run_archive_migration(&db).await?;
    validate_current(&db).await?;
    ensure!(saved(&db).await? == original);
    ensure!(receipts::validate_legacy(&db).await.is_err());
    ensure!(usage::validate_legacy(&db).await.is_err());
    for table in [RECEIPTS, USAGE] {
        ensure!(db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
            format!("SELECT 1 FROM {table} WHERE typeof(storage_kind)<>'integer' OR storage_kind<>0 LIMIT 1"))).await?.is_none());
    }
    ensure!(
        db.query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT 1 FROM tenancy_receipt_archive LIMIT 1"
        ))
        .await?
        .is_none()
    );
    let before_schema = schema(&db).await?;
    let ledger = Migrator::get_migration_models(&db).await?;
    run_archive_migration(&db).await?;
    ensure!(Migrator::get_migration_models(&db).await? == ledger);
    ensure!(schema(&db).await? == before_schema && saved(&db).await? == original);
    // Direct replay without ledger mediation must not adopt existing objects.
    ensure!(Migration.up(&SchemaManager::new(&db)).await.is_err());
    ensure!(Migration.down(&SchemaManager::new(&db)).await.is_err());
    ensure!(schema(&db).await? == before_schema && saved(&db).await? == original);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn failed_ledger_write_rolls_back_columns_archive_and_data_and_retry_succeeds() -> Result<()>
{
    let db = previous().await?;
    seed(&db).await?;
    let original = saved(&db).await?;
    let ledger = Migrator::get_migration_models(&db).await?;
    db.execute_unprepared(&format!("CREATE TRIGGER reject_archive_migration BEFORE INSERT ON seaql_migrations WHEN NEW.version='{}' BEGIN SELECT RAISE(ABORT,'injected migration failure'); END", Migration.name())).await?;
    let before_schema = schema(&db).await?;
    ensure!(run_archive_migration(&db).await.is_err());
    ensure!(schema(&db).await? == before_schema && saved(&db).await? == original);
    ensure!(Migrator::get_migration_models(&db).await? == ledger);
    receipts::validate_legacy(&db).await?;
    usage::validate_legacy(&db).await?;
    ensure!(!catalog::present(&db).await?);
    db.execute_unprepared("DROP TRIGGER reject_archive_migration")
        .await?;
    run_archive_migration(&db).await?;
    validate_current(&db).await?;
    ensure!(saved(&db).await? == original);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn incompatible_legacy_or_existing_archive_objects_are_never_adopted_or_changed() -> Result<()>
{
    for mutation in [
        "ALTER TABLE tenancy_execution_receipts ADD COLUMN storage_kind INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE tenancy_usage_evidence ADD COLUMN unexpected TEXT",
        "CREATE INDEX extra_receipt_index ON tenancy_execution_receipts(execution_id)",
        "CREATE INDEX extra_usage_index ON tenancy_usage_evidence(execution_id)",
        "CREATE INDEX idx_tenancy_receipts_storage ON tenancy_usage_evidence(execution_id)",
        "CREATE TEMP VIEW IDX_TENANCY_RECEIPTS_STORAGE AS SELECT 1",
        "CREATE TABLE tenancy_receipt_archive(untrusted BLOB)",
        "CREATE VIEW tenancy_receipt_archive AS SELECT 1",
        "CREATE INDEX tenancy_receipt_archive ON tenancy_usage_evidence(execution_id)",
        "CREATE TEMP TABLE TENANCY_EXECUTION_RECEIPTS(untrusted BLOB)",
        "CREATE TEMP VIEW TENANCY_USAGE_EVIDENCE AS SELECT 1",
        "CREATE TEMP VIEW TENANCY_RECEIPT_ARCHIVE AS SELECT 1",
        "CREATE TRIGGER rewrite_usage AFTER INSERT ON tenancy_usage_evidence BEGIN DELETE FROM tenancy_usage_evidence; END",
    ] {
        let db = previous().await?;
        seed(&db).await?;
        db.execute_unprepared(mutation).await?;
        let before = schema(&db).await?;
        let ledger = Migrator::get_migration_models(&db).await?;
        ensure!(
            run_archive_migration(&db).await.is_err(),
            "adopted {mutation}"
        );
        ensure!(
            schema(&db).await? == before && Migrator::get_migration_models(&db).await? == ledger
        );
        db.close().await?;
    }
    let db = previous().await?;
    SchemaManager::new(&db).create_table(definition()).await?;
    let before = schema(&db).await?;
    ensure!(
        run_archive_migration(&db).await.is_err(),
        "adopted unrecorded canonical archive"
    );
    ensure!(schema(&db).await? == before);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn complete_profile_refuses_partial_schema_extra_columns_indexes_and_triggers() -> Result<()>
{
    for mutation in [
        "DROP TABLE tenancy_receipt_archive",
        "ALTER TABLE tenancy_receipt_archive ADD COLUMN unexpected TEXT",
        "ALTER TABLE tenancy_execution_receipts ADD COLUMN unexpected TEXT",
        "ALTER TABLE tenancy_usage_evidence ADD COLUMN unexpected TEXT",
        "CREATE INDEX extra_archive_index ON tenancy_receipt_archive(codec)",
        "CREATE INDEX extra_receipt_index ON tenancy_execution_receipts(execution_id)",
        "CREATE INDEX extra_usage_index ON tenancy_usage_evidence(execution_id)",
        "DROP INDEX idx_tenancy_receipts_storage",
        "CREATE TEMP VIEW IDX_TENANCY_RECEIPTS_STORAGE AS SELECT 1",
        "CREATE TEMP VIEW TENANCY_RECEIPT_ARCHIVE AS SELECT 1",
        "CREATE TEMP VIEW TENANCY_EXECUTION_RECEIPTS AS SELECT 1",
        "CREATE TEMP VIEW TENANCY_USAGE_EVIDENCE AS SELECT 1",
        "CREATE TRIGGER rewrite_archive AFTER INSERT ON tenancy_receipt_archive BEGIN DELETE FROM tenancy_receipt_archive; END",
        "CREATE TEMP TRIGGER rewrite_archive AFTER INSERT ON main.tenancy_receipt_archive BEGIN DELETE FROM tenancy_receipt_archive; END",
    ] {
        let db = previous().await?;
        run_archive_migration(&db).await?;
        db.execute_unprepared(mutation).await?;
        let before = schema(&db).await?;
        ensure!(validate(&db).await.is_err(), "accepted {mutation}");
        ensure!(super::super::validate_receipt_archive(&db).await.is_err());
        ensure!(usage::validate(&db).await.is_err());
        ensure!(schema(&db).await? == before, "validation changed schema");
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn archive_affinity_collation_and_primary_key_changes_are_not_equivalent_schemas()
-> Result<()> {
    let canonical = definition().to_string(SqliteQueryBuilder);
    for sql in [
        canonical.replace("COLLATE BINARY", "COLLATE NOCASE"),
        canonical.replace("\"payload\" blob", "\"payload\" text"),
        canonical.replace("\"codec\" bigint", "\"codec\" integer"),
        canonical.replace(
            "\"tenant_id\", \"execution_id\"",
            "\"execution_id\", \"tenant_id\"",
        ),
    ] {
        ensure!(sql != canonical, "ineffective schema mutation");
        let db = previous().await?;
        run_archive_migration(&db).await?;
        db.execute_unprepared("DROP TABLE tenancy_receipt_archive")
            .await?;
        db.execute_unprepared(&sql).await?;
        ensure!(validate_current(&db).await.is_err());
        db.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn storage_index_requires_exact_columns_order_collation_and_constraints() -> Result<()> {
    for replacement in [
        "CREATE INDEX idx_tenancy_receipts_storage ON tenancy_execution_receipts(installation_id,tenant_id,storage_kind,execution_id)",
        "CREATE INDEX idx_tenancy_receipts_storage ON tenancy_execution_receipts(installation_id,tenant_id,execution_id)",
        "CREATE INDEX idx_tenancy_receipts_storage ON tenancy_execution_receipts(installation_id,storage_kind,tenant_id,execution_id,principal_id)",
        "CREATE UNIQUE INDEX idx_tenancy_receipts_storage ON tenancy_execution_receipts(installation_id,storage_kind,tenant_id,execution_id)",
        "CREATE INDEX idx_tenancy_receipts_storage ON tenancy_execution_receipts(installation_id,storage_kind,tenant_id,execution_id) WHERE storage_kind=0",
        "CREATE INDEX idx_tenancy_receipts_storage ON tenancy_execution_receipts(installation_id,storage_kind,tenant_id COLLATE NOCASE,execution_id)",
        "CREATE INDEX idx_tenancy_receipts_storage ON tenancy_execution_receipts(installation_id,storage_kind,tenant_id DESC,execution_id)",
        "CREATE INDEX idx_tenancy_receipts_storage ON tenancy_execution_receipts(installation_id,storage_kind,lower(tenant_id),execution_id)",
        "CREATE INDEX idx_tenancy_receipts_storage ON tenancy_usage_evidence(installation_id,storage_kind,tenant_id,execution_id)",
    ] {
        let db = previous().await?;
        seed(&db).await?;
        run_archive_migration(&db).await?;
        validate_current(&db).await?;
        db.execute_unprepared("DROP INDEX idx_tenancy_receipts_storage")
            .await?;
        db.execute_unprepared(replacement).await?;
        let before = schema(&db).await?;
        let original = saved(&db).await?;
        ensure!(validate(&db).await.is_err(), "accepted {replacement}");
        ensure!(super::super::validate_receipt_archive(&db).await.is_err());
        ensure!(usage::validate(&db).await.is_err());
        ensure!(schema(&db).await? == before && saved(&db).await? == original);
        db.close().await?;
    }
    Ok(())
}
