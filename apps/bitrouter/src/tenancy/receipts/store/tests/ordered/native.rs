//! The native writer fence changes no historical receipt or evidence bytes.
use super::*;
use crate::db::migration::{Migrator, m20240101_000048_native_generate_content::Migration};
use sea_orm::Statement;
const FENCE: &str = "m20240101_000048_native_generate_content";

#[cfg(unix)]
mod predecessor;
async fn before_native(path: &std::path::Path) -> Result<DatabaseConnection> {
    let db = legacy_database(path).await?;
    Migrator::up(&db, Some(3)).await?;
    ensure!(version(&db).await? == 4);
    Ok(db)
}
async fn installed(db: &DatabaseConnection) -> Result<bool> {
    Ok(db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            format!("SELECT 1 FROM seaql_migrations WHERE version='{FENCE}'"),
        ))
        .await?
        .is_some())
}
#[tokio::test]
async fn native_generate_content_writer_fence_is_atomic_and_preserves_original_bytes() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let db = before_native(&directory.path().join("native.sqlite")).await?;
    let original = candidate(
        "installation",
        "tenant",
        "historical-native-fence",
        "principal",
    )?
    .with_usage_evidence(crate::tenancy::receipts::usage_evidence::Snapshot::default())?;
    ensure!(commit(&db, &original).await?);
    db.execute_unprepared("CREATE TRIGGER refuse_native_upgrade BEFORE UPDATE ON tenancy_installation WHEN NEW.schema_version=5 BEGIN SELECT RAISE(ABORT,'refuse native format upgrade'); END").await?;
    ensure!(crate::db::run_migrations(&db).await.is_err());
    ensure!(version(&db).await? == 4 && !installed(&db).await?);
    db.execute_unprepared("DROP TRIGGER refuse_native_upgrade")
        .await?;
    let ledger = Migrator::get_migration_models(&db).await?;
    db.execute_unprepared(&format!("CREATE TRIGGER refuse_native_ledger BEFORE INSERT ON seaql_migrations WHEN NEW.version='{FENCE}' BEGIN SELECT RAISE(ABORT,'refuse native ledger'); END")).await?;
    ensure!(crate::db::run_migrations(&db).await.is_err());
    ensure!(version(&db).await? == 4 && Migrator::get_migration_models(&db).await? == ledger);
    db.execute_unprepared("DROP TRIGGER refuse_native_ledger")
        .await?;
    crate::db::run_migrations(&db).await?;
    ensure!(version(&db).await? == 5 && installed(&db).await?);
    management_mutation(&db).await?;
    let saved = lookup_candidate(
        &db,
        &original.receipt.installation_id,
        &original.receipt.tenant_id,
        &original.receipt.execution_id,
    )
    .await?
    .context("history")?;
    ensure!(
        saved.json == original.json
            && saved.private_json == original.private_json
            && saved.digest == original.digest
    );
    let old = original.usage_evidence.as_ref().context("old evidence")?;
    let new = saved.usage_evidence.as_ref().context("new evidence")?;
    ensure!(old.json == new.json && old.digest == new.digest);
    ensure!(Migration.down(&SchemaManager::new(&db)).await.is_err());
    ensure!(version(&db).await? == 5 && installed(&db).await?);
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn native_generate_content_writer_fence_refuses_malformed_and_future_markers() -> Result<()> {
    for (marker, identity) in [
        (0, "installation"),
        (2, "installation"),
        (3, "installation"),
        (5, "installation"),
        (6, "installation"),
        (4, ""),
    ] {
        let directory = tempfile::tempdir()?;
        let db = before_native(&directory.path().join("bad-native.sqlite")).await?;
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE tenancy_installation SET schema_version=?,installation_id=?",
            [marker.into(), identity.into()],
        ))
        .await?;
        ensure!(
            crate::db::run_migrations(&db).await.is_err(),
            "accepted {marker} {identity:?}"
        );
        ensure!(version(&db).await? == marker && !installed(&db).await?);
        db.close().await?;
    }
    Ok(())
}
