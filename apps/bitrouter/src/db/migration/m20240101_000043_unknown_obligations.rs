//! SQLite obligation layout and one-time canonical backfill.
//!
//! OAuth lookup columns are backfilled without rewriting canonical bytes. The
//! same exclusive migration transaction validates/backfills obligations before
//! its ledger entry commits. This enables no ongoing writer, maintenance driver,
//! authority format, or admission accounting. Normal startup only validates.
use super::m20240101_000028_resource_tenancy::identity;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseTransaction, TransactionTrait};
use sea_orm_migration::prelude::*;

mod backfill;
pub(super) mod catalog;
#[cfg(test)]
mod tests;

#[derive(DeriveMigrationName)]
pub struct Migration;
const TABLE: &str = "tenancy_unknown_obligations";
const SLOTS: &str = "tenancy_oauth_slots";
const AUXILIARY: &str = "tenancy_receipt_auxiliary";
const DUE: &str = "idx_tenancy_unknown_due";
const PENDING: &str = "idx_tenancy_oauth_execution";
const PENDING_COLUMNS: [&str; 2] = ["pending_tenant_id", "pending_execution_id"];

fn incompatible() -> DbErr {
    DbErr::Custom("migration043 incompatible unknown obligation storage".into())
}
fn definition() -> TableCreateStatement {
    let mut table = Table::create();
    table.table(Alias::new(TABLE));
    for name in ["installation_id", "tenant_id", "execution_id"] {
        table.col(identity(DatabaseBackend::Sqlite, name, 128, false));
    }
    table.col(
        ColumnDef::new(Alias::new("source_mask"))
            .integer()
            .not_null()
            .check(Expr::col(Alias::new("source_mask")).between(1, 7)),
    );
    for prefix in ["first", "due"] {
        table.col(
            ColumnDef::new(Alias::new(format!("{prefix}_seconds")))
                .big_integer()
                .not_null(),
        );
        let name = format!("{prefix}_nanos");
        table.col(
            ColumnDef::new(Alias::new(&name))
                .integer()
                .not_null()
                .check(Expr::col(Alias::new(&name)).between(0, 999_999_999)),
        );
    }
    table.col(
        ColumnDef::new(Alias::new("escalated"))
            .integer()
            .not_null()
            .check(Expr::col(Alias::new("escalated")).is_in([0, 1])),
    );
    table
        .primary_key(
            Index::create()
                .col(Alias::new("installation_id"))
                .col(Alias::new("tenant_id"))
                .col(Alias::new("execution_id")),
        )
        .to_owned()
}
fn pending_column(name: &str) -> ColumnDef {
    identity(DatabaseBackend::Sqlite, name, 128, true)
}
fn index(name: &str, table: &str, columns: &[&str]) -> IndexCreateStatement {
    let mut value = Index::create();
    value.name(name).table(Alias::new(table));
    for column in columns {
        value.col(Alias::new(*column));
    }
    value.to_owned()
}
fn pending_index() -> IndexCreateStatement {
    index(
        PENDING,
        SLOTS,
        &[
            "installation_id",
            "pending_tenant_id",
            "pending_execution_id",
            "slot_key",
        ],
    )
}
fn due_index() -> IndexCreateStatement {
    index(
        DUE,
        TABLE,
        &[
            "installation_id",
            "escalated",
            "due_seconds",
            "due_nanos",
            "tenant_id",
            "execution_id",
        ],
    )
}

pub(super) async fn validate(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    if db.get_database_backend() != DatabaseBackend::Sqlite {
        return Err(incompatible());
    }
    catalog::validate(db, true).await
}
async fn upgrade(tx: &DatabaseTransaction) -> Result<(), DbErr> {
    let manager = SchemaManager::new(tx);
    let db = manager.get_connection();
    if catalog::present(db).await? {
        return Err(incompatible());
    }
    super::m20240101_000037_owned_oauth::validate_legacy(db).await?;
    // SQLite preserves canonical row bytes while adding nullable scalar columns.
    // https://www.sqlite.org/lang_altertable.html#alter_table_add_column
    for name in PENDING_COLUMNS {
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new(SLOTS))
                    .add_column(pending_column(name))
                    .to_owned(),
            )
            .await?;
    }
    backfill::run(db).await?;
    manager.create_index(pending_index()).await?;
    manager.create_table(definition()).await?;
    manager.create_index(due_index()).await?;
    validate(db).await?;
    crate::tenancy::receipts::store::obligations::backfill_migration(tx)
        .await
        .map_err(|error| {
            DbErr::Custom(format!(
                "migration043 canonical obligation backfill: {error}"
            ))
        })
}
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() != DatabaseBackend::Sqlite {
            return Ok(());
        }
        let tx = manager.get_connection().begin().await?;
        match upgrade(&tx).await {
            Ok(()) => tx.commit().await,
            Err(error) => {
                tx.rollback().await?;
                Err(error)
            }
        }
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() != DatabaseBackend::Sqlite {
            return Ok(());
        }
        Err(DbErr::Custom(
            "migration043 rollback requires a verified compatible backup".into(),
        ))
    }
}
