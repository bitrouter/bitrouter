//! Per-command-group report types.
//!
//! Each command group gets a submodule here holding its `#[derive(Serialize)]`
//! report structs and their [`CliReport`](crate::output::CliReport)
//! implementations. Submodules are added as groups are converted.
