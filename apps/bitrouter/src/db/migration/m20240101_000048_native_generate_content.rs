//! Fence predecessor writers without rewriting retained resources or receipts.

use bitrouter_ai::tenancy::identity::InstallationId;
use sea_orm::{ConnectionTrait, DatabaseBackend};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

fn incompatible() -> DbErr {
    DbErr::Custom("incompatible native generation authority".into())
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
        // A fresh store has no authority row; initialization writes format five.
        return Ok(());
    };
    let format = row.try_get::<i32>("", "schema_version")?;
    if row.try_get::<i32>("", "singleton")? != 1 || !matches!(format, 1 | 4) {
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
        .value(Alias::new("schema_version"), 5)
        .and_where(Expr::col(Alias::new("singleton")).eq(1))
        .and_where(Expr::col(Alias::new("schema_version")).eq(4))
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
            "native generation authority requires verified-backup rollback".into(),
        ))
    }
}
