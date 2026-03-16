//! Database migrations for spend tracking tables.

pub mod m20260316_000001_create_spend_logs;

use sea_orm_migration::MigrationTrait;

/// Returns all observe-crate migrations in order.
pub fn migrations() -> Vec<Box<dyn MigrationTrait>> {
    vec![Box::new(m20260316_000001_create_spend_logs::Migration)]
}
