//! Implementations of the shared [actions](bitrouter_mcp::actions).
//!
//! The types and their port traits live in `bitrouter-mcp` (the `#[tool]` macro
//! expands there, so the report must be nameable in that crate, and the reverse
//! dependency edge would be a cycle). The behavior lives here, once, and both
//! the CLI leaf and the MCP tool go through it.
//!
//! Only actions with more than one surface belong here; every other CLI command
//! keeps its own report under [`crate::output::reports`].

pub mod models;
pub mod route;
pub mod skills;
pub mod status;
