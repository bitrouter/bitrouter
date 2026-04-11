//! Per-caller tool access-control policy engine.
//!
//! Policies are JSON files stored in `<home>/policies/`, attached to API keys
//! via JWT `pol` claims, and evaluated at request time with AND semantics.
//!
//! - **Policy files** — [`file`] defines the on-disk policy format with
//!   spend limits and per-provider tool access rules.
//! - **Policy cache** — [`cache::PolicyCache`] loads policy files and resolves
//!   merged access rules across multiple policies.
//! - **Tool registry** — [`registry::GuardedToolRegistry`] wraps any
//!   [`ToolRegistry`](bitrouter_core::routers::registry::ToolRegistry) for
//!   admin API tool listing.
//! - **Configuration** — [`config::ToolProviderPolicy`] defines per-provider
//!   filter + restriction bundles reused by policy files.

pub mod cache;
pub mod config;
pub mod file;
pub mod registry;
