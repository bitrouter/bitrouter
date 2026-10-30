//! Immutable private import evidence. No historical rows are adopted by DDL.
use super::m20240101_000028_resource_tenancy::{identity, payload};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;
const TABLE: &str = "eval_import_provenance";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // There is no legacy form of this table. Refuse a pre-existing object;
        // do not bless a same-name table or its contents as import evidence.
        if manager.has_table(TABLE).await? {
            return Err(DbErr::Custom(
                "migration038 provenance object already exists".into(),
            ));
        }
        let backend = manager.get_database_backend();
        let mut table = Table::create();
        table.table(Alias::new(TABLE));
        if backend == sea_orm::DatabaseBackend::MySql {
            table.engine("InnoDB");
        }
        for (name, width) in [
            ("installation_id", 128),
            ("tenant_id", 128),
            ("creator_principal_id", 128),
            ("snapshot_key", 64),
            ("manifest_sha256", 64),
            ("provenance_digest", 64),
        ] {
            table.col(identity(backend, name, width, false));
        }
        table.col(payload(backend, "provenance_json"));
        table.primary_key(Index::create().col(Alias::new("snapshot_key")));
        manager.create_table(table.to_owned()).await
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Even an empty-looking table may be referenced externally. This version
        // has no destructive downgrade: use an explicitly verified old backup.
        if manager.has_table(TABLE).await? {
            return Err(DbErr::Custom(
                "migration038 requires verified-backup rollback".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, ensure};
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    #[tokio::test]
    async fn existing_provenance_object_is_not_adopted_and_down_never_drops_evidence() -> Result<()>
    {
        let db = crate::db::connect("sqlite::memory:").await?;
        let manager = SchemaManager::new(&db);
        Migration.up(&manager).await?;
        db.execute(Statement::from_string(DatabaseBackend::Sqlite,
            "INSERT INTO eval_import_provenance VALUES('installation','tenant','creator','physical','manifest','proof','original-private-record')".to_owned())).await?;
        ensure!(Migration.up(&manager).await.is_err());
        ensure!(Migration.down(&manager).await.is_err());
        let rows = db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT provenance_json FROM eval_import_provenance".to_owned(),
            ))
            .await?;
        ensure!(
            rows.len() == 1
                && rows[0].try_get::<String>("", "provenance_json")? == "original-private-record"
        );
        db.close().await?;
        Ok(())
    }
    #[tokio::test]
    async fn incompatible_same_name_table_is_refused_before_any_ddl() -> Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "CREATE TABLE eval_import_provenance(untrusted BLOB)".to_owned(),
        ))
        .await?;
        let manager = SchemaManager::new(&db);
        ensure!(Migration.up(&manager).await.is_err());
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT sql FROM sqlite_schema WHERE name='eval_import_provenance'".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("sentinel missing"))?;
        ensure!(
            row.try_get::<String>("", "sql")?
                == "CREATE TABLE eval_import_provenance(untrusted BLOB)"
        );
        db.close().await?;
        Ok(())
    }
}
