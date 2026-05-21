//! Schema migrations, as `sea-orm-migration` Rust code.
//!
//! Each migration is a `MigrationTrait` impl that builds tables and
//! indexes through sea-orm's portable schema API — no hand-written SQL —
//! so the identical schema applies on SQLite, Postgres and MySQL alike.
//!
//! Migration ordering is the order of the [`Migrator::migrations`] vec;
//! applied migrations are recorded in the `seaql_migrations` table so a
//! re-run is a no-op.

pub mod m20240101_000001_create_auth_tables;
pub mod m20240101_000002_create_metering_tables;
pub mod m20240101_000003_rename_legacy_charge_column;

use sea_orm_migration::{MigrationTrait, MigratorTrait};

/// The bitrouter schema migrator — owns the ordered list of every
/// migration the binary ships.
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20240101_000001_create_auth_tables::Migration),
            Box::new(m20240101_000002_create_metering_tables::Migration),
            Box::new(m20240101_000003_rename_legacy_charge_column::Migration),
        ]
    }
}
