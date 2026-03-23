//! Database migrations for the skills table.
//!
//! This module exports individual migration steps. The final binary
//! crate assembles them (possibly alongside migrations from other crates)
//! into a [`MigratorTrait`](sea_orm_migration::MigratorTrait) implementation.

pub mod m20260323_000001_create_skills;

use sea_orm_migration::MigrationTrait;

/// Returns all skills-crate migrations in order.
pub fn migrations() -> Vec<Box<dyn MigrationTrait>> {
    vec![Box::new(m20260323_000001_create_skills::Migration)]
}
