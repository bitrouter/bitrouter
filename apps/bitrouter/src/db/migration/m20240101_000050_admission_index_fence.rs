//! Fence predecessor writers without rewriting retained resources or receipts.

use bitrouter_ai::tenancy::identity::InstallationId;
use sea_orm::{ConnectionTrait, DatabaseBackend};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

fn incompatible() -> DbErr {
    DbErr::Custom("incompatible indexed admission authority".into())
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
        // A fresh store has no authority row; initialization writes format six.
        return Ok(());
    };
    let format = row.try_get::<i32>("", "schema_version")?;
    if row.try_get::<i32>("", "singleton")? != 1 || !matches!(format, 1 | 5) {
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
        .value(Alias::new("schema_version"), 6)
        .and_where(Expr::col(Alias::new("singleton")).eq(1))
        .and_where(Expr::col(Alias::new("schema_version")).eq(5))
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
            "indexed admission authority requires verified-backup rollback".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migration::Migrator;
    use anyhow::{Context, Result, ensure};

    struct Predecessor;
    #[async_trait::async_trait]
    impl MigratorTrait for Predecessor {
        fn migrations() -> Vec<Box<dyn MigrationTrait>> {
            Migrator::migrations()
                .into_iter()
                .take_while(|m| m.name() != "m20240101_000049_admission_index")
                .collect()
        }
    }

    #[tokio::test]
    async fn admission_fence_preserves_empty_and_existing_authority_and_refuses_predecessor()
    -> Result<()> {
        for legacy in [false, true] {
            let db = crate::db::connect("sqlite::memory:").await?;
            Predecessor::up(&db, None).await?;
            if legacy {
                db.execute_unprepared("INSERT INTO tenancy_installation VALUES(1,'installation',5,23,17,3,0,'2026-01-01T00:00:00Z')").await?;
            }
            crate::db::run_migrations(&db).await?;
            ensure!(Predecessor::get_pending_migrations(&db).await.is_err());
            if legacy {
                let row = db.query_one(sea_orm::Statement::from_string(DatabaseBackend::Sqlite,
                    "SELECT schema_version,authority_revision,operation_ids,safety_operation_ids FROM tenancy_installation")).await?.context("authority")?;
                ensure!(row.try_get::<i32>("", "schema_version")? == 6);
                ensure!(row.try_get::<i64>("", "authority_revision")? == 23);
                ensure!(
                    row.try_get::<i64>("", "operation_ids")? == 17
                        && row.try_get::<i64>("", "safety_operation_ids")? == 3
                );
            } else {
                let service = crate::tenancy::Service::open(db.clone(), None).await?;
                service.shutdown().await?;
            }
        }
        Ok(())
    }
}
