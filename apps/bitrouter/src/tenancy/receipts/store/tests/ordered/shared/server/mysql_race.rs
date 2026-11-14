//! Two real runners cross a stale prefix count using only the owned test database.
use super::*;

pub(super) async fn campaign(db: &DatabaseConnection) -> Result<()> {
    const NATIVE: &str = "m20240101_000048_native_generate_content";
    let before = Migrator::migrations()
        .iter()
        .position(|migration| migration.name() == "m20240101_000046_profiled_authority")
        .context("prefix")?;
    Migrator::up(db, Some(u32::try_from(before)?)).await?;
    db.execute_unprepared("INSERT INTO tenancy_installation(singleton,installation_id,schema_version,authority_revision,operation_ids,safety_operation_ids,saturated,created_at) VALUES(1,'installation',2,0,0,0,FALSE,'2026-10-25T00:00:00Z')").await?;
    let original = candidate("installation", "tenant", "raced-history", "principal")?
        .with_usage_evidence(crate::tenancy::receipts::usage_evidence::Snapshot::default())?;
    ensure!(commit(db, &original).await?);
    let pending = Migrator::get_pending_migrations(db).await?;
    ensure!(
        pending
            .iter()
            .map(|migration| migration.name())
            .collect::<Vec<_>>()
            == ["m20240101_000046_profiled_authority", MIGRATION, NATIVE]
    );
    let engines=db.query_all(Statement::from_string(DatabaseBackend::MySql,
        "SELECT ENGINE AS engine FROM information_schema.tables WHERE TABLE_SCHEMA=DATABASE() AND TABLE_NAME IN ('tenancy_installation','seaql_migrations')")).await?;
    ensure!(engines.len() == 2);
    for row in engines {
        ensure!(row.try_get::<String>("", "engine")? == "InnoDB");
    }
    let mut ledger = None;
    let raced = crate::db::test_migrations::mysql_race::stale_prefix(db, async {
        // Runner A has already counted one historical step. Runner B uses the
        // same production transaction wrapper through the original fence47.
        migrate_shared_health(db).await?;
        ensure!(marker(db).await?==4);
        ledger=Some(Migrator::get_migration_models(db).await?);
        db.execute_unprepared(&format!("CREATE TRIGGER reject_raced_native BEFORE INSERT ON seaql_migrations FOR EACH ROW BEGIN IF NEW.version='{NATIVE}' THEN SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT='injected raced native ledger failure'; END IF; END")).await?;
        Ok(())
    }).await?;
    ensure!(raced.is_err(), "stale runner must refuse");
    let observed = marker(db).await?;
    ensure!(observed == 4, "stale prefix changed marker to {observed}");
    let ledger = ledger.context("committed B ledger")?;
    ensure!(Migrator::get_migration_models(db).await? == ledger);
    ensure!(
        raced
            .err()
            .context("stale prefix refusal")?
            .to_string()
            .contains("MySQL writer fence requires transactional runner")
    );
    let retained = lookup_candidate(
        db,
        &original.receipt.installation_id,
        &original.receipt.tenant_id,
        &original.receipt.execution_id,
    )
    .await?
    .context("retained receipt")?;
    ensure!(
        retained.json == original.json
            && retained.private_json == original.private_json
            && retained.digest == original.digest
    );
    let evidence = retained
        .usage_evidence
        .as_ref()
        .context("retained evidence")?;
    let old = original
        .usage_evidence
        .as_ref()
        .context("original evidence")?;
    ensure!(evidence.json == old.json && evidence.digest == old.digest);
    db.execute_unprepared("DROP TRIGGER reject_raced_native")
        .await?;
    crate::db::run_migrations(db).await?;
    ensure!(marker(db).await? == 5);
    let completed = Migrator::get_migration_models(db).await?;
    ensure!(
        completed.len() == ledger.len() + 1 && completed.iter().any(|row| row.version == NATIVE)
    );
    crate::db::run_migrations(db).await?;
    ensure!(Migrator::get_migration_models(db).await? == completed);
    Ok(())
}
