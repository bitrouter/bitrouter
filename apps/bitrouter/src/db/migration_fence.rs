//! Each data-only writer marker and its ledger form one MySQL DML transaction.
use sea_orm::{
    ActiveValue, ConnectionTrait, DatabaseBackend, DatabaseConnection, DatabaseTransaction, DbErr,
    EntityTrait, Statement, TransactionTrait,
};
use sea_orm_migration::{MigrationTrait, MigratorTrait, seaql_migrations};

use super::migration::{
    m20240101_000047_shared_admission_health::Migration as SharedHealth,
    m20240101_000048_native_generate_content::Migration as NativeGenerateContent,
};

/// Created only after engine checks hold metadata locks in this transaction.
/// Its private field prevents direct framework migrations from forging entry.
pub(in crate::db) struct MysqlFence<'a> {
    transaction: &'a DatabaseTransaction,
}
impl MysqlFence<'_> {
    pub(in crate::db) fn transaction(&self) -> &DatabaseTransaction {
        self.transaction
    }
}

enum Fence {
    SharedHealth,
    NativeGenerateContent,
}
impl Fence {
    fn migration(&self) -> &dyn MigrationTrait {
        match self {
            Self::SharedHealth => &SharedHealth,
            Self::NativeGenerateContent => &NativeGenerateContent,
        }
    }
    async fn apply(&self, fence: &MysqlFence<'_>) -> Result<(), DbErr> {
        match self {
            Self::SharedHealth => SharedHealth.apply_mysql(fence).await,
            Self::NativeGenerateContent => NativeGenerateContent.apply_mysql(fence).await,
        }
    }
}

pub(super) async fn mysql<M: MigratorTrait>(db: &DatabaseConnection) -> Result<(), DbErr> {
    for fence in [Fence::SharedHealth, Fence::NativeGenerateContent] {
        let pending = M::get_pending_migrations(db).await?;
        let Some(before) = pending
            .iter()
            .position(|migration| migration.name() == fence.migration().name())
        else {
            continue;
        };
        let steps =
            u32::try_from(before).map_err(|_| DbErr::Custom("migration count overflow".into()))?;
        // Historical DDL keeps its existing discipline. Each data-only fence
        // uses a separate transaction without CREATE TABLE IF NOT EXISTS: that
        // statement commits MySQL transactions even when the ledger exists.
        // https://dev.mysql.com/doc/refman/8.4/en/implicit-commit.html
        M::up(db, Some(steps)).await?;
        commit_fence(db, &fence).await?;
    }
    M::up(db, None).await
}

async fn commit_fence(db: &DatabaseConnection, fence: &Fence) -> Result<(), DbErr> {
    let transaction = db.begin().await?;
    let result = async {
        transactional_tables(&transaction).await?;
        fence
            .apply(&MysqlFence {
                transaction: &transaction,
            })
            .await?;
        // Match SeaORM's existing ledger entity without its DDL installation
        // inside this transaction.
        // https://github.com/SeaQL/sea-orm/blob/1.1.20/sea-orm-migration/src/migrator.rs
        seaql_migrations::Entity::insert(seaql_migrations::ActiveModel {
            version: ActiveValue::Set(fence.migration().name().to_owned()),
            applied_at: ActiveValue::Set(chrono::Utc::now().timestamp()),
        })
        .exec(&transaction)
        .await?;
        Ok::<_, DbErr>(())
    }
    .await;
    match result {
        Ok(()) => transaction.commit().await?,
        Err(error) => {
            transaction.rollback().await?;
            return Err(error);
        }
    }
    Ok(())
}

async fn transactional_tables(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    // Touch both tables in this same explicit transaction before inspecting
    // their engines. Metadata locks survive even nontransactional reads until
    // the transaction ends, so concurrent ALTER cannot invalidate the check.
    // https://dev.mysql.com/doc/refman/8.4/en/metadata-locking.html
    for table in ["tenancy_installation", "seaql_migrations"] {
        let _probe = db
            .query_one(Statement::from_string(
                DatabaseBackend::MySql,
                format!("SELECT 1 FROM `{table}` LIMIT 1"),
            ))
            .await?;
    }
    let rows = db.query_all(Statement::from_string(DatabaseBackend::MySql,
        "SELECT TABLE_NAME AS table_name, ENGINE AS engine FROM information_schema.tables WHERE TABLE_SCHEMA=DATABASE() AND TABLE_NAME IN ('tenancy_installation','seaql_migrations')",
    )).await?;
    let unsupported = || DbErr::Custom("unsupported MySQL migration fence storage".into());
    if rows.len() != 2 {
        return Err(unsupported());
    }
    for row in rows {
        let name = row.try_get::<String>("", "table_name")?;
        let engine = row.try_get::<Option<String>>("", "engine")?;
        // A transaction cannot make MyISAM/MEMORY writes rollback-capable.
        // Refuse these preconditions without rewriting retained tables/data.
        // https://dev.mysql.com/doc/refman/8.4/en/nontransactional-tables.html
        if !matches!(name.as_str(), "tenancy_installation" | "seaql_migrations")
            || !engine.is_some_and(|engine| engine.eq_ignore_ascii_case("InnoDB"))
        {
            return Err(unsupported());
        }
    }
    Ok(())
}
