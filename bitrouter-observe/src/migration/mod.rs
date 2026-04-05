//! Database migrations for spend tracking tables.

pub mod m20260316_000001_create_spend_logs;
pub mod m20260319_000002_add_session_id_to_spend_logs;
pub mod m20260322_000003_unify_spend_logs;
pub mod m20260405_000004_add_key_id_to_spend_logs;

use sea_orm_migration::MigrationTrait;

/// Returns all observe-crate migrations in order.
pub fn migrations() -> Vec<Box<dyn MigrationTrait>> {
    vec![
        Box::new(m20260316_000001_create_spend_logs::Migration),
        Box::new(m20260319_000002_add_session_id_to_spend_logs::Migration),
        Box::new(m20260322_000003_unify_spend_logs::Migration),
        Box::new(m20260405_000004_add_key_id_to_spend_logs::Migration),
    ]
}
