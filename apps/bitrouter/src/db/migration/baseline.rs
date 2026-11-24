//! Fresh schema installation and read-only validation of current gateway storage.
use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, Statement, TransactionTrait};
use sea_orm_migration::{SchemaManagerConnection, prelude::*};

use crate::db::store_format::{self, State};

pub(super) struct Migration;
impl MigrationName for Migration {
    fn name(&self) -> &str {
        store_format::GATEWAY_SERVER
    }
}

pub(super) async fn open(connection: SchemaManagerConnection<'_>) -> Result<(), DbErr> {
    // MySQL DDL commits implicitly. Stage only the exactly validated new table
    // before starting the data/marker transaction. A crash leaves the old
    // marker and a recognized empty table, so the same upgrade can retry.
    if connection.get_database_backend() == DatabaseBackend::MySql {
        let state = store_format::inspect(&connection)
            .await
            .map_err(|error| DbErr::Custom(error.to_string()))?;
        if state == State::Legacy {
            crate::db::current_schema::validate_legacy(&connection).await?;
            let manager = match &connection {
                SchemaManagerConnection::Connection(db) => SchemaManager::new(*db),
                SchemaManagerConnection::Transaction(tx) => SchemaManager::new(*tx),
            };
            super::capacity::ensure_table(&manager)
                .await
                .map_err(|error| DbErr::Custom(error.to_string()))?;
        }
    }
    let tx = connection.begin().await?;
    let result = async {
        if tx.get_database_backend() == DatabaseBackend::Postgres {
            tx.execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT pg_advisory_xact_lock($1)",
                [0x4252_4d30_5343_4831_i64.into()],
            ))
            .await?;
        }
        let state = store_format::inspect(&tx)
            .await
            .map_err(|error| DbErr::Custom(error.to_string()))?;
        if state == State::Legacy {
            if tx.get_database_backend() != DatabaseBackend::Sqlite {
                crate::db::current_schema::validate_legacy(&tx).await?;
            }
            super::capacity::upgrade(&tx)
                .await
                .map_err(|error| DbErr::Custom(format!("capacity upgrade: {error:#}")))?;
            return validate(&tx).await;
        }
        if tx.get_database_backend() == DatabaseBackend::Sqlite {
            if state == State::Empty {
                crate::db::sqlite_schema::initialize_gateway_schema(&tx)
                    .await
                    .map_err(|error| DbErr::Custom(error.to_string()))?;
            }
            return validate(&tx).await;
        }
        if state == State::Current {
            return validate(&tx).await;
        }
        let manager = SchemaManager::new(&tx);
        Migration.up(&manager).await?;
        super::capacity::initialize(
            &tx,
            &crate::tenancy::receipts::capacity::Inventory::default(),
            false,
        )
        .await
        .map_err(|error| DbErr::Custom(error.to_string()))?;
        crate::db::current_schema::validate(&tx).await?;
        // Install and publish the only ledger row after the complete schema
        // validates. SQLite/PostgreSQL commit the schema and marker together.
        // MySQL's implicit DDL commits cannot publish a half-built baseline.
        tx.execute(Statement::from_sql_and_values(
            tx.get_database_backend(),
            match tx.get_database_backend() {
                DatabaseBackend::Postgres => {
                    "INSERT INTO seaql_migrations(version,applied_at) VALUES ($1,$2)"
                }
                _ => "INSERT INTO seaql_migrations(version,applied_at) VALUES (?,?)",
            },
            [
                store_format::GATEWAY_SERVER.into(),
                chrono::Utc::now().timestamp().into(),
            ],
        ))
        .await?;
        validate(&tx).await?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => tx.commit().await,
        Err(error) => {
            tx.rollback().await?;
            Err(error)
        }
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if store_format::inspect(manager.get_connection())
            .await
            .map_err(|error| DbErr::Custom(error.to_string()))?
            != State::Empty
        {
            return Err(DbErr::Custom(
                "baseline initialization requires a genuinely empty database".into(),
            ));
        }
        for statement in crate::db::current_schema::statements(manager.get_database_backend())
            .map_err(|error| DbErr::Custom(error.to_string()))?
        {
            manager
                .get_connection()
                .execute_unprepared(&statement)
                .await?;
        }
        Ok(())
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "current store rollback requires its original binary and backup".into(),
        ))
    }
}

/// Validate the same compiled schema used for fresh installation.
pub(super) async fn validate(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    if db.get_database_backend() == DatabaseBackend::Sqlite {
        crate::db::sqlite_schema::validate(db, true)
            .await
            .map_err(|error| DbErr::Custom(error.to_string()))
    } else {
        crate::db::current_schema::validate(db).await
    }
}

#[cfg(all(test, any(feature = "db-postgres", feature = "db-mysql")))]
mod server_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, ensure};

    #[tokio::test]
    async fn fresh_baseline_has_current_receipts_and_one_atomic_marker() -> Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        super::super::Migrator::up(&db, None).await?;
        ensure!(store_format::inspect(&db).await? == State::Current);
        validate(&db).await?;
        let before = db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT name,sql FROM sqlite_schema ORDER BY name",
            ))
            .await?;
        let ledger = super::super::Migrator::get_migration_models(&db).await?;
        ensure!(ledger.len() == 1 && ledger[0].version == store_format::GATEWAY_SQLITE);
        super::super::Migrator::up(&db, None).await?;
        let after = db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT name,sql FROM sqlite_schema ORDER BY name",
            ))
            .await?;
        ensure!(before.len() == after.len());
        for (before, after) in before.iter().zip(&after) {
            ensure!(before.try_get::<String>("", "name")? == after.try_get::<String>("", "name")?);
            ensure!(
                before.try_get::<Option<String>>("", "sql")?
                    == after.try_get::<Option<String>>("", "sql")?
            );
        }
        ensure!(super::super::Migrator::get_migration_models(&db).await? == ledger);
        db.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn old_or_unmarked_stores_refuse_without_ledger_mutation() -> Result<()> {
        for marked in [false, true] {
            let db = crate::db::connect("sqlite::memory:").await?;
            db.execute_unprepared(
                "CREATE TABLE preserved (value TEXT); INSERT INTO preserved VALUES ('original')",
            )
            .await?;
            if marked {
                db.execute_unprepared("CREATE TABLE seaql_migrations (version TEXT PRIMARY KEY,applied_at BIGINT NOT NULL); INSERT INTO seaql_migrations VALUES ('unsupported-store-format',1)").await?;
            }
            ensure!(super::super::Migrator::up(&db, None).await.is_err());
            ensure!(
                SchemaManager::new(&db)
                    .has_table("seaql_migrations")
                    .await?
                    == marked
            );
            ensure!(
                !SchemaManager::new(&db)
                    .has_table("tenancy_installation")
                    .await?
            );
            ensure!(
                db.query_one(Statement::from_string(
                    DatabaseBackend::Sqlite,
                    "SELECT value FROM preserved WHERE value='original'"
                ))
                .await?
                .is_some()
            );
            db.close().await?;
        }
        Ok(())
    }
}
