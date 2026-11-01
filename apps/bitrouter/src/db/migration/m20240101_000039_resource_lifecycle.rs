//! Local managed heads and exact-revision revocations; no resource adoption.

use sea_orm::DatabaseBackend;
use sea_orm_migration::prelude::*;

use super::m20240101_000028_resource_tenancy::identity;

#[derive(DeriveMigrationName)]
pub struct Migration;

const TABLE: &str = "tenancy_resource_lifecycle";

fn definition(backend: DatabaseBackend) -> TableCreateStatement {
    let mut table = Table::create();
    table.table(Alias::new(TABLE));
    if backend == DatabaseBackend::MySql {
        table.engine("InnoDB");
    }
    for (name, width) in [
        ("owner_kind", 32),
        ("owner_id", 128),
        ("resource_kind", 32),
        ("resource_id", 128),
        ("revision_id", 128),
        ("installation_id", 128),
    ] {
        table.col(identity(backend, name, width, false));
    }
    for name in ["is_head", "revoked"] {
        table.col(ColumnDef::new(Alias::new(name)).boolean().not_null());
    }
    table.col(
        ColumnDef::new(Alias::new("authority_revision"))
            .big_integer()
            .not_null(),
    );
    // Same byte-exact qualified identity and MySQL key width as immutable revisions.
    // A single installation owns the table; load validates every row's installation.
    let mut primary = Index::create();
    for name in [
        "owner_kind",
        "owner_id",
        "resource_kind",
        "resource_id",
        "revision_id",
    ] {
        primary.col(Alias::new(name));
    }
    table.primary_key(&mut primary).to_owned()
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.has_table(TABLE).await? {
            return Err(DbErr::Custom(
                "migration039 resource lifecycle object already exists".into(),
            ));
        }
        manager
            .create_table(definition(manager.get_database_backend()))
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.has_table(TABLE).await? {
            return Err(DbErr::Custom(
                "migration039 requires verified-backup rollback".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, Result, ensure};
    use sea_orm::{ConnectionTrait, Statement};

    #[test]
    fn portable_ddl_uses_exact_identities_and_only_lifecycle_metadata() -> Result<()> {
        for backend in [
            DatabaseBackend::Sqlite,
            DatabaseBackend::Postgres,
            DatabaseBackend::MySql,
        ] {
            let sql = backend.build(&definition(backend)).to_string();
            ensure!(!sql.contains("IF NOT EXISTS") && !sql.contains("FOREIGN KEY"));
            ensure!(sql.contains("PRIMARY KEY") && sql.contains("authority_revision"));
            let collation = match backend {
                DatabaseBackend::Sqlite => "COLLATE BINARY",
                DatabaseBackend::Postgres => "COLLATE \"C\"",
                DatabaseBackend::MySql => "COLLATE utf8mb4_0900_bin",
            };
            ensure!(sql.matches(collation).count() == 6);
            for forbidden in ["payload", "secret", "definition", "content", "digest"] {
                ensure!(!sql.contains(forbidden));
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn migration_refuses_existing_tables_and_never_drops_history() -> Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        let manager = SchemaManager::new(&db);
        Migration.down(&manager).await?;
        Migration.up(&manager).await?;
        ensure!(Migration.up(&manager).await.is_err());
        ensure!(Migration.down(&manager).await.is_err());
        db.execute(Statement::from_string(DatabaseBackend::Sqlite,
            "INSERT INTO tenancy_resource_lifecycle VALUES('tenant','owner','credential','slot','v1','installation',1,1,7)")).await?;
        ensure!(Migration.up(&manager).await.is_err());
        ensure!(Migration.down(&manager).await.is_err());
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT is_head,revoked,authority_revision FROM tenancy_resource_lifecycle",
            ))
            .await?
            .context("lost lifecycle history")?;
        ensure!(row.try_get::<bool>("", "is_head")? && row.try_get::<bool>("", "revoked")?);
        ensure!(row.try_get::<i64>("", "authority_revision")? == 7);
        let columns = db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "PRAGMA table_info('tenancy_resource_lifecycle')",
            ))
            .await?;
        ensure!(columns.len() == 9);
        for column in columns {
            ensure!(column.try_get::<i64>("", "notnull")? == 1);
        }
        Ok(())
    }

    #[tokio::test]
    async fn incompatible_table_is_not_adopted_or_changed() -> Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        let original = "CREATE TABLE tenancy_resource_lifecycle(untrusted BLOB)";
        db.execute(Statement::from_string(DatabaseBackend::Sqlite, original))
            .await?;
        let manager = SchemaManager::new(&db);
        ensure!(Migration.up(&manager).await.is_err());
        ensure!(Migration.down(&manager).await.is_err());
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT sql FROM sqlite_schema WHERE name='tenancy_resource_lifecycle'",
            ))
            .await?
            .context("missing original table")?;
        ensure!(row.try_get::<String>("", "sql")? == original);
        Ok(())
    }
}
