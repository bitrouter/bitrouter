//! Native harness checkpoint and tool-start ledger, atomically replaced by CAS.
use sea_orm::DatabaseBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveIden)]
enum BroManagedSessions {
    Table,
    Id,
    Revision,
    PayloadBytes,
    Payload,
}

/// DDL-only component of the fresh current baseline; never accepts an old dataset.
pub(super) async fn initialize_current(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let mut payload = ColumnDef::new(BroManagedSessions::Payload);
    // A managed journal may exceed MySQL TEXT's 64 KiB capacity.
    // https://dev.mysql.com/doc/refman/8.4/en/blob.html
    if manager.get_database_backend() == DatabaseBackend::MySql {
        payload.custom(Alias::new("LONGTEXT"));
    } else {
        payload.text();
    }
    manager
        .create_table(
            Table::create()
                .table(BroManagedSessions::Table)
                .if_not_exists()
                .col(
                    ColumnDef::new(BroManagedSessions::Id)
                        .string()
                        .not_null()
                        .primary_key(),
                )
                .col(
                    ColumnDef::new(BroManagedSessions::Revision)
                        .big_integer()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(BroManagedSessions::PayloadBytes)
                        .big_integer()
                        .not_null(),
                )
                .col(payload.not_null())
                .to_owned(),
        )
        .await
}
