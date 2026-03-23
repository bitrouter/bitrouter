//! Skills registry for bitrouter.
//!
//! This crate provides:
//!
//! - **Skill types** — [`Skill`](skill::Skill), [`SkillSource`](skill::SkillSource),
//!   [`InstalledBy`](skill::InstalledBy) following the
//!   [agentskills.io](https://agentskills.io) standard.
//! - **Entity types** — [`entity::skill`] backed by sea-orm.
//! - **Migrations** — Individual migration steps exported via
//!   [`migration::migrations()`](migration::migrations).
//! - **Registries** — [`ConfigSkillRegistry`](registry::ConfigSkillRegistry) for
//!   config-driven skills and [`SkillRegistry`](registry::SkillRegistry) for
//!   DB-backed skill management.

pub mod entity;
pub mod migration;
pub mod registry;
pub mod skill;
