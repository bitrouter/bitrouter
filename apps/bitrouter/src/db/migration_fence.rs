//! The shared-health marker and its ledger entry form one MySQL DML transaction.
use sea_orm::{ActiveValue, DatabaseConnection, DbErr, EntityTrait, TransactionTrait};
use sea_orm_migration::{
    MigrationName, MigrationTrait, MigratorTrait, SchemaManager, seaql_migrations,
};

use super::migration::{Migrator, m20240101_000047_shared_admission_health::Migration};

pub(super) async fn mysql(db: &DatabaseConnection) -> Result<(), DbErr> {
    let pending = Migrator::get_pending_migrations(db).await?;
    let fence = Migration;
    let Some(before) = pending
        .iter()
        .position(|migration| migration.name() == fence.name())
    else {
        return Migrator::up(db, None).await;
    };
    let steps =
        u32::try_from(before).map_err(|_| DbErr::Custom("migration count overflow".into()))?;
    // Historical DDL keeps its existing discipline. Only the new DML-only fence
    // is grouped below; wrapping MySQL CREATE TABLE IF NOT EXISTS would commit
    // the transaction even when the ledger table already exists.
    // https://dev.mysql.com/doc/refman/8.4/en/implicit-commit.html
    Migrator::up(db, Some(steps)).await?;
    let transaction = db.begin().await?;
    let result = async {
        fence.up(&SchemaManager::new(&transaction)).await?;
        // Match SeaORM's existing ledger entity without its DDL installation
        // inside this transaction.
        // https://github.com/SeaQL/sea-orm/blob/1.1.20/sea-orm-migration/src/migrator.rs
        seaql_migrations::Entity::insert(seaql_migrations::ActiveModel {
            version: ActiveValue::Set(fence.name().to_owned()),
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
    Migrator::up(db, None).await
}
