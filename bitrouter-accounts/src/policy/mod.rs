//! Per-caller tool access-control policy engine.
//!
//! Policy data types live in [`bitrouter_core::policy`]. This module provides:
//!
//! - **Policy loading** — [`file::load_policies`] loads policy files from
//!   `<home>/policies/` with tracing-based error reporting.
//! - **Policy cache** — [`cache::PolicyCache`] holds loaded policies and
//!   implements [`ToolPolicyResolver`](bitrouter_core::routers::admin::ToolPolicyResolver).
//! - **Tool registry** — [`registry::GuardedToolRegistry`] wraps any
//!   [`ToolRegistry`](bitrouter_core::routers::registry::ToolRegistry) for
//!   admin API tool listing with visibility filtering.

pub mod cache;
pub mod config;
pub mod file;
pub mod registry;
