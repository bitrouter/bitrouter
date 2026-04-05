//! Database migrations for account and session tables.
//!
//! This module exports individual migration steps. The final binary
//! crate assembles them (possibly alongside migrations from other crates)
//! into a [`MigratorTrait`](sea_orm_migration::MigratorTrait) implementation.

pub mod m20260310_000001_create_accounts;
pub mod m20260310_000002_create_api_keys;
pub mod m20260310_000003_create_sessions;
pub mod m20260310_000004_create_messages;
pub mod m20260310_000005_create_session_files;
pub mod m20260311_000006_jwt_auth;
pub mod m20260405_000007_create_revoked_keys;

use sea_orm_migration::MigrationTrait;

/// Returns all account-crate migrations in order.
pub fn migrations() -> Vec<Box<dyn MigrationTrait>> {
    vec![
        Box::new(m20260310_000001_create_accounts::Migration),
        Box::new(m20260310_000002_create_api_keys::Migration),
        Box::new(m20260310_000003_create_sessions::Migration),
        Box::new(m20260310_000004_create_messages::Migration),
        Box::new(m20260310_000005_create_session_files::Migration),
        Box::new(m20260311_000006_jwt_auth::Migration),
        Box::new(m20260405_000007_create_revoked_keys::Migration),
    ]
}
