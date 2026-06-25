//! Per-command-group report types.
//!
//! Each command group gets a submodule here holding its `#[derive(Serialize)]`
//! report structs and their [`CliReport`](crate::output::CliReport)
//! implementations. Submodules are added as groups are converted.

pub mod admin;
pub mod agents;
pub mod config;
pub mod daemon;
pub mod routing;
pub mod tools;
