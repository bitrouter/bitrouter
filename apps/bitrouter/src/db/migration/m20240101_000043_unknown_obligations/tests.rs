use super::super::{Migrator, m20240101_000037_owned_oauth as legacy};
use super::*;
use anyhow::{Context, Result, ensure};
use sea_orm::{DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;

async fn previous() -> Result<DatabaseConnection> {
    let db = crate::db::connect("sqlite::memory:").await?;
    let index = Migrator::migrations()
        .iter()
        .position(|m| m.name() == Migration.name())
        .context("migration043 registration")?;
    Migrator::up(&db, Some(index.try_into()?)).await?;
    Ok(db)
}
async fn seed(db: &DatabaseConnection) -> Result<()> {
    crate::tenancy::receipts::store::obligations::tests::seed_legacy(db).await
}
async fn originals(db: &DatabaseConnection) -> Result<Vec<String>> {
    let mut saved = Vec::new();
    for query in [
        "SELECT json_array(slot_key,installation_id,owner_kind,owner_id,credential_resource_id,authority_revision,generation,state,metadata_json,payload_json,content_digest) AS original FROM tenancy_oauth_slots ORDER BY slot_key",
        "SELECT json_array(installation_id,tenant_id,execution_id,principal_id,correlation_id,receipt_json,content_digest,disposition_json,byte_count,unknown_outcome,recorded_at,storage_kind) AS original FROM tenancy_execution_receipts ORDER BY installation_id,tenant_id,execution_id",
        "SELECT json_array(installation_id,tenant_id,execution_id,receipt_digest,evidence_json,evidence_digest,unknown_outcome) AS original FROM tenancy_receipt_auxiliary ORDER BY installation_id,tenant_id,execution_id",
        "SELECT json_array(installation_id,tenant_id,execution_id,schema_version,receipt_digest,evidence_digest,evidence_json,byte_count,storage_kind) AS original FROM tenancy_usage_evidence ORDER BY installation_id,tenant_id,execution_id",
        "SELECT json_array(singleton,installation_id,schema_version,authority_revision,operation_ids,safety_operation_ids,saturated,created_at) AS original FROM tenancy_installation",
    ] {
        for row in db
            .query_all(Statement::from_string(DatabaseBackend::Sqlite, query))
            .await?
        {
            saved.push(row.try_get("", "original")?);
        }
    }
    Ok(saved)
}
async fn schema(db: &DatabaseConnection) -> Result<Vec<(String, String)>> {
    db.query_all(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT name,coalesce(sql,'') AS sql FROM main.sqlite_schema UNION ALL SELECT name,coalesce(sql,'') AS sql FROM temp.sqlite_schema ORDER BY name,sql")).await?.into_iter().map(|r| Ok((r.try_get("", "name")?,r.try_get("", "sql")?))).collect()
}
#[tokio::test]
async fn migration_preserves_bytes_backfills_canonical_obligations_once_and_is_ledger_idempotent()
-> Result<()> {
    let db = previous().await?;
    seed(&db).await?;
    legacy::validate_legacy(&db).await?;
    legacy::validate(&db).await?;
    let original = originals(&db).await?;
    crate::db::run_migrations(&db).await?;
    validate(&db).await?;
    legacy::validate(&db).await?;
    ensure!(legacy::validate_legacy(&db).await.is_err());
    ensure!(originals(&db).await? == original);
    let rows=db.query_all(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT pending_tenant_id,pending_execution_id FROM tenancy_oauth_slots ORDER BY pending_tenant_id,pending_execution_id")).await?;
    let pairs = rows
        .into_iter()
        .map(|row| {
            Ok((
                row.try_get::<Option<String>>("", "pending_tenant_id")?,
                row.try_get::<Option<String>>("", "pending_execution_id")?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        pairs
            == vec![
                (None, None),
                (Some("Tenant".into()), Some("Execution".into())),
                (Some("tenant".into()), Some("Execution".into()))
            ]
    );
    let obligations=db.query_all(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT tenant_id,execution_id,source_mask FROM tenancy_unknown_obligations ORDER BY tenant_id,execution_id")).await?;
    let obligations = obligations
        .into_iter()
        .map(|row| {
            Ok((
                row.try_get::<String>("", "tenant_id")?,
                row.try_get::<String>("", "execution_id")?,
                row.try_get::<i64>("", "source_mask")?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        obligations
            == vec![
                ("Tenant".into(), "Execution".into(), 4),
                ("tenant".into(), "Execution".into(), 3)
            ]
    );
    let header = db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT schema_version FROM tenancy_installation",
        ))
        .await?
        .context("header")?;
    ensure!(header.try_get::<i64>("", "schema_version")? == 1);
    let saved = schema(&db).await?;
    let ledger = Migrator::get_migration_models(&db).await?;
    crate::db::run_migrations(&db).await?;
    ensure!(
        schema(&db).await? == saved
            && originals(&db).await? == original
            && Migrator::get_migration_models(&db).await? == ledger
    );
    ensure!(Migration.up(&SchemaManager::new(&db)).await.is_err());
    ensure!(Migration.down(&SchemaManager::new(&db)).await.is_err());
    db.execute_unprepared("DELETE FROM tenancy_unknown_obligations")
        .await?;
    crate::db::run_migrations(&db).await?;
    ensure!(
        db.query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT 1 FROM tenancy_unknown_obligations LIMIT 1"
        ))
        .await?
        .is_none(),
        "migration silently rebuilt later corruption"
    );
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn failed_backfill_or_ledger_preserves_exact_legacy_schema_and_rows() -> Result<()> {
    for failure in ["metadata", "canonical", "ledger"] {
        let db = previous().await?;
        seed(&db).await?;
        if failure == "ledger" {
            db.execute_unprepared(&format!("CREATE TRIGGER fail043 BEFORE INSERT ON seaql_migrations WHEN NEW.version='{}' BEGIN SELECT RAISE(ABORT,'injected'); END",Migration.name())).await?;
        } else if failure == "canonical" {
            db.execute_unprepared(
                "UPDATE tenancy_execution_receipts SET content_digest=printf('%064d',0)",
            )
            .await?;
        } else {
            db.execute_unprepared(
                "UPDATE tenancy_oauth_slots SET metadata_json='{}' WHERE state='quarantined'",
            )
            .await?;
        }
        let original = originals(&db).await?;
        let saved = schema(&db).await?;
        let ledger = Migrator::get_migration_models(&db).await?;
        ensure!(crate::db::run_migrations(&db).await.is_err());
        ensure!(
            schema(&db).await? == saved
                && originals(&db).await? == original
                && Migrator::get_migration_models(&db).await? == ledger
        );
        legacy::validate_legacy(&db).await?;
        if failure == "ledger" {
            db.execute_unprepared("DROP TRIGGER fail043").await?;
            crate::db::run_migrations(&db).await?;
            validate(&db).await?;
        }
        db.close().await?;
    }
    Ok(())
}
#[tokio::test]
async fn legacy_profile_rejects_unrecorded_partial_and_extra_objects_without_changes() -> Result<()>
{
    for mutation in [
        "ALTER TABLE tenancy_oauth_slots ADD COLUMN pending_tenant_id TEXT",
        "CREATE TABLE tenancy_unknown_obligations(value TEXT)",
        "CREATE VIEW tenancy_unknown_obligations AS SELECT 1",
        "CREATE INDEX idx_tenancy_oauth_execution ON tenancy_oauth_slots(state)",
        "CREATE INDEX extra_slot_index ON tenancy_oauth_slots(state)",
        "CREATE TEMP VIEW TENANCY_OAUTH_SLOTS AS SELECT 1",
        "CREATE TEMP VIEW IDX_TENANCY_UNKNOWN_DUE AS SELECT 1",
    ] {
        let db = previous().await?;
        db.execute_unprepared(mutation).await?;
        let saved = schema(&db).await?;
        ensure!(
            legacy::validate_legacy(&db).await.is_err(),
            "legacy accepted {mutation}"
        );
        ensure!(
            crate::db::run_migrations(&db).await.is_err(),
            "migration accepted {mutation}"
        );
        ensure!(schema(&db).await? == saved);
        db.close().await?;
    }
    Ok(())
}
#[tokio::test]
async fn upgraded_profile_refuses_missing_objects_extras_and_changed_index_semantics() -> Result<()>
{
    for mutation in [
        "DROP TABLE tenancy_unknown_obligations",
        "ALTER TABLE tenancy_unknown_obligations ADD COLUMN extra TEXT",
        "ALTER TABLE tenancy_oauth_slots ADD COLUMN extra TEXT",
        "CREATE INDEX extra_unknown ON tenancy_unknown_obligations(source_mask)",
        "CREATE INDEX extra_aux ON tenancy_receipt_auxiliary(execution_id)",
        "DROP INDEX idx_tenancy_oauth_execution",
        "CREATE TEMP VIEW TENANCY_UNKNOWN_OBLIGATIONS AS SELECT 1",
        "CREATE TEMP VIEW IDX_TENANCY_OAUTH_EXECUTION AS SELECT 1",
        "CREATE TRIGGER corrupt_unknown AFTER INSERT ON tenancy_unknown_obligations BEGIN DELETE FROM tenancy_unknown_obligations; END",
        "CREATE TEMP TRIGGER corrupt_slot AFTER INSERT ON main.tenancy_oauth_slots BEGIN DELETE FROM tenancy_oauth_slots; END",
    ] {
        let db = previous().await?;
        crate::db::run_migrations(&db).await?;
        db.execute_unprepared(mutation).await?;
        ensure!(
            validate(&db).await.is_err() && legacy::validate(&db).await.is_err(),
            "accepted {mutation}"
        );
        db.close().await?;
    }
    let db = previous().await?;
    crate::db::run_migrations(&db).await?;
    db.execute_unprepared("DROP INDEX idx_tenancy_oauth_execution")
        .await?;
    db.execute_unprepared("CREATE INDEX idx_tenancy_oauth_execution ON tenancy_oauth_slots(installation_id,pending_tenant_id COLLATE NOCASE,pending_execution_id,slot_key)").await?;
    ensure!(validate(&db).await.is_err());
    db.close().await?;
    Ok(())
}
