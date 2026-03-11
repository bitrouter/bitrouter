//! Database migrations.
//!
//! Call [`Migrator::up`] to apply all pending migrations. The CLI should
//! run this before starting the server — the server itself does not
//! auto-migrate.

mod m20260310_000001_create_accounts;
mod m20260310_000002_create_api_keys;
mod m20260310_000003_create_sessions;
mod m20260310_000004_create_messages;
mod m20260310_000005_create_session_files;

use sea_orm_migration::{MigrationTrait, MigratorTrait};

pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260310_000001_create_accounts::Migration),
            Box::new(m20260310_000002_create_api_keys::Migration),
            Box::new(m20260310_000003_create_sessions::Migration),
            Box::new(m20260310_000004_create_messages::Migration),
            Box::new(m20260310_000005_create_session_files::Migration),
        ]
    }
}

/// Run all pending migrations against the given database connection.
pub async fn run(db: &sea_orm::DatabaseConnection) -> Result<(), sea_orm::DbErr> {
    Migrator::up(db, None).await
}
