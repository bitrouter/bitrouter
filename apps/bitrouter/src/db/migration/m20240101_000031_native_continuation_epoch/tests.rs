use super::*;
use anyhow::{Context, Result, ensure};
use sea_orm_migration::MigratorTrait;

mod server;

#[tokio::test]
async fn additive_epoch_migration_preserves_legacy_bytes_and_refuses_populated_down() -> Result<()>
{
    let db = crate::db::connect("sqlite::memory:").await?;
    let all = crate::db::migration::Migrator::migrations();
    let previous = all
        .iter()
        .position(|migration| migration.name() == Migration.name())
        .context("migration031 registration")?;
    crate::db::migration::Migrator::up(&db, Some(previous.try_into()?)).await?;
    db.execute_unprepared("INSERT INTO provider_continuation_key_epoch(singleton_id,key_id,created_at) VALUES(1,'LegacyKey','2024-01-01T01:02:03+00:00')").await?;
    db.execute_unprepared("INSERT INTO provider_continuations(continuation_identity,owner_identity,ciphertext,nonce,target_fingerprint,key_id,cipher_version,created_at,expires_at,purge_after,publication_state,publication_generation,publication_instance_id,publication_lease_until) VALUES('legacy-id','legacy-owner','LiteralCipher+/=','LiteralNonce+/=','target','LegacyKey',1,'created-exact','expires-exact','purge-exact','active','generation-exact','instance-exact','lease-exact')").await?;
    let before = crate::db::migration::Migrator::get_migration_models(&db).await?;
    let schema = db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT sql FROM sqlite_master WHERE name='provider_continuations'".to_owned(),
        ))
        .await?
        .context("legacy schema")?
        .try_get::<String>("", "sql")?;
    // Exercise exactly this additive migration even when later migrations are
    // registered; its predecessor schema and one-entry ledger delta are fixed.
    crate::db::migration::Migrator::up(&db, Some(1)).await?;
    let manager = SchemaManager::new(&db);
    Migration.up(&manager).await?;
    let after = crate::db::migration::Migrator::get_migration_models(&db).await?;
    ensure!(after.starts_with(&before) && after.len() == before.len() + 1);
    ensure!(
        after
            .last()
            .is_some_and(|row| row.version == Migration.name())
    );
    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT * FROM provider_continuations WHERE continuation_identity='legacy-id'"
                .to_owned(),
        ))
        .await?
        .context("legacy row")?;
    for (field, value) in [
        ("ciphertext", "LiteralCipher+/="),
        ("nonce", "LiteralNonce+/="),
        ("created_at", "created-exact"),
        ("publication_generation", "generation-exact"),
        ("publication_lease_until", "lease-exact"),
    ] {
        ensure!(row.try_get::<String>("", field)? == value);
    }
    ensure!(row.try_get::<i32>("", "cipher_version")? == 1);
    ensure!(
        db.query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT sql FROM sqlite_master WHERE name='provider_continuations'".to_owned()
        ))
        .await?
        .context("legacy schema")?
        .try_get::<String>("", "sql")?
            == schema
    );
    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT created_at FROM provider_continuation_key_epoch".to_owned(),
        ))
        .await?
        .context("legacy epoch")?;
    ensure!(row.try_get::<String>("", "created_at")? == "2024-01-01T01:02:03+00:00");
    db.execute_unprepared("INSERT INTO tenancy_continuation_epoch(singleton,installation_id,key_id,created_at) VALUES(1,'Installation','0123456789012345678901234567890123456789012345678901234567890123','2026-10-25T00:00:00Z')").await?;
    ensure!(Migration.down(&manager).await.is_err());
    ensure!(manager.has_table(TABLE).await?);
    Ok(())
}

#[tokio::test]
async fn existing_incompatible_epoch_definition_is_not_silently_adopted() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    db.execute_unprepared("CREATE TABLE tenancy_continuation_epoch(singleton INTEGER NOT NULL PRIMARY KEY, installation_id VARCHAR(128) COLLATE NOCASE NOT NULL, key_id VARCHAR(64) NOT NULL, created_at VARCHAR(20) NOT NULL)").await?;
    ensure!(Migration.up(&SchemaManager::new(&db)).await.is_err());
    Ok(())
}
