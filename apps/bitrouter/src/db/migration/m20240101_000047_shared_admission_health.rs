//! Fence predecessor writers without rewriting retained resources or receipts.

use bitrouter_ai::tenancy::identity::InstallationId;
use sea_orm::{ConnectionTrait, DatabaseBackend};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

fn incompatible() -> DbErr {
    DbErr::Custom("incompatible shared admission authority".into())
}

fn identities(backend: DatabaseBackend) -> SelectStatement {
    let column = Expr::col(Alias::new("installation_id"));
    // SQLite TEXT length stops at an embedded NUL. Count bytes before the
    // driver allocates even a corrupt identity, then retain typed validation.
    // https://sqlite.org/lang_corefunc.html#length
    // https://www.postgresql.org/docs/current/functions-string.html
    // https://dev.mysql.com/doc/refman/8.4/en/string-functions.html#function_octet-length
    let length = if backend == DatabaseBackend::Sqlite {
        Func::cust(Alias::new("length")).arg(column.clone().cast_as(Alias::new("BLOB")))
    } else {
        Func::cust(Alias::new("octet_length")).arg(column.clone())
    };
    Query::select()
        .from(Alias::new("tenancy_installation"))
        .columns([Alias::new("singleton"), Alias::new("schema_version")])
        .expr_as(
            Expr::case(Expr::expr(length).lte(128), column)
                .finally(Expr::val(Option::<String>::None)),
            Alias::new("installation_id"),
        )
        .limit(2)
        .to_owned()
}

impl Migration {
    pub(in crate::db) async fn apply_mysql(
        &self,
        fence: &super::super::migration_fence::MysqlFence<'_>,
    ) -> Result<(), DbErr> {
        apply(&SchemaManager::new(fence.transaction())).await
    }
}

async fn apply(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let db = manager.get_connection();
    let backend = manager.get_database_backend();
    super::m20240101_000045_ordered_receipts::validate(db).await?;
    let rows = db.query_all(backend.build(&identities(backend))).await?;
    if rows.len() > 1 {
        return Err(incompatible());
    }
    let Some(row) = rows.first() else {
        // A fresh store has no authority row; initialization writes format four.
        return Ok(());
    };
    let format = row.try_get::<i32>("", "schema_version")?;
    if row.try_get::<i32>("", "singleton")? != 1 || !matches!(format, 1 | 3) {
        return Err(incompatible());
    }
    let _: InstallationId = row
        .try_get::<String>("", "installation_id")?
        .try_into()
        .map_err(|_| incompatible())?;
    if format == 1 {
        // Only the owned legacy capacity upgrade may release the old latch.
        return Ok(());
    }
    let update = Query::update()
        .table(Alias::new("tenancy_installation"))
        .value(Alias::new("schema_version"), 4)
        .and_where(Expr::col(Alias::new("singleton")).eq(1))
        .and_where(Expr::col(Alias::new("schema_version")).eq(3))
        .to_owned();
    if db.execute(backend.build(&update)).await?.rows_affected() != 1 {
        return Err(incompatible());
    }
    Ok(())
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // A historical step count can race framework rediscovery on MySQL.
        // Never let that path execute a writer fence without its transaction.
        if manager.get_database_backend() == DatabaseBackend::MySql {
            return Err(DbErr::Custom(
                "MySQL writer fence requires transactional runner".into(),
            ));
        }
        apply(manager).await
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "shared admission authority requires verified-backup rollback".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migration::Migrator;
    use anyhow::{Context, Result, ensure};
    use sea_orm::Statement;

    #[tokio::test]
    async fn shared_health_identity_projection_bounds_nul_and_multibyte_before_allocation()
    -> Result<()> {
        for identity in [
            format!("a\0{}", "x".repeat(1_000_000)),
            "x".repeat(129),
            "é".repeat(128),
            "a\0tail".to_owned(),
            "x".repeat(128),
        ] {
            let db = crate::db::connect("sqlite::memory:").await?;
            let before = Migrator::migrations()
                .iter()
                .position(|m| m.name() == Migration.name())
                .context("migration")?;
            Migrator::up(&db, Some(before.try_into()?)).await?;
            db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "INSERT INTO tenancy_installation(singleton,installation_id,schema_version,authority_revision,operation_ids,safety_operation_ids,saturated,created_at) VALUES(1,?,3,0,0,0,0,'2026-10-25T00:00:00Z')",
                [identity.clone().into()],
            )).await?;
            // Query the actual migration projection, not a copy of its SQL.
            let row = db
                .query_one(DatabaseBackend::Sqlite.build(&identities(DatabaseBackend::Sqlite)))
                .await?
                .context("projected identity")?;
            let projected: Option<String> = row.try_get("", "installation_id")?;
            if identity.len() > 128 {
                ensure!(projected.is_none());
            } else {
                ensure!(projected.as_deref() == Some(identity.as_str()));
            }
            let ledger = Migrator::get_migration_models(&db).await?;
            let valid = identity == "x".repeat(128);
            // This component probe stops at the shared-health fence. Successor
            // migrations have their own atomicity and historical-byte probes.
            let result = crate::db::test_migrations::through_shared_health(&db).await;
            ensure!(result.is_ok() == valid);
            let row = db
                .query_one(Statement::from_string(
                    DatabaseBackend::Sqlite,
                    "SELECT schema_version FROM tenancy_installation",
                ))
                .await?
                .context("marker")?;
            ensure!(row.try_get::<i32>("", "schema_version")? == if valid { 4 } else { 3 });
            if valid {
                ensure!(Migrator::get_migration_models(&db).await?.len() == ledger.len() + 1);
            } else {
                ensure!(Migrator::get_migration_models(&db).await? == ledger);
            }
            db.close().await?;
        }
        Ok(())
    }
}
