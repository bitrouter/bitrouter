//! Per-caller tool access-control policy engine.
//!
//! Policies are JSON files stored in `<home>/policies/`, attached to API keys
//! via a single JWT `pol` claim, and evaluated at request time.
//!
//! - **Policy files** — [`file`] defines the on-disk policy format with
//!   spend limits and per-provider tool allow-lists.
//! - **Policy cache** — [`cache::PolicyCache`] loads policy files and resolves
//!   tool allow-lists for a single policy.
//! - **Tool registry** — [`registry::GuardedToolRegistry`] wraps any
//!   [`ToolRegistry`](bitrouter_core::routers::registry::ToolRegistry) for
//!   admin API tool listing.
//! - **Configuration** — [`config::ToolProviderPolicy`] defines per-provider
//!   allow-list bundles reused by policy files.

pub mod cache;
pub mod config;
pub mod file;
pub mod registry;
