//! Bounded offline working storage; transaction and file custody stay with callers.

mod inventory;
mod io;
#[cfg(feature = "core")]
mod json;
mod phase;
pub(crate) mod sort;
mod workspace;

use std::path::PathBuf;

/// Operator working resources for complete offline evidence processing.
#[derive(Clone, Debug)]
pub struct Options {
    /// Aggregate temporary application memory, independent of retained data size.
    pub working_bytes: usize,
    /// Existing private absolute directory in which to create temporary files.
    pub scratch_directory: PathBuf,
    /// Free bytes to preserve on each actual output volume.
    pub min_free_bytes: u64,
}

/// A working allocation cannot fit; the stored data is not thereby corrupt.
#[derive(Debug, thiserror::Error)]
#[error("offline working capacity exceeded")]
pub(crate) struct CapacityExceeded;

// These concrete aliases stay private to the crate; no public re-export API.
pub(crate) type Workspace = workspace::Workspace;
pub(crate) type Charge = workspace::Charge;
pub(crate) type Source = io::Source;
pub(crate) type Inspected = inventory::Inspected;

#[cfg(feature = "core")]
pub(crate) fn encode_json(
    value: &impl serde::Serialize,
    workspace: &Workspace,
) -> anyhow::Result<(Vec<u8>, Charge)> {
    json::encode(value, workspace)
}

/// Probe the actual publication volume after temporary storage has been removed.
pub(crate) fn probe(path: &std::path::Path, floor: u64, next: u64) -> anyhow::Result<()> {
    workspace::probe(path, floor, next)
}

pub(crate) async fn inspect_in(
    db: &mut sea_orm::sqlx::SqliteConnection,
    key: &[u8; 32],
    workspace: &Workspace,
) -> anyhow::Result<Inspected> {
    inventory::inspect_in(db, key, workspace).await
}

pub(crate) fn copy_new(
    source: &std::path::Path,
    destination: &std::path::Path,
    workspace: &Workspace,
) -> anyhow::Result<u64> {
    io::copy_new(source, destination, workspace)
}

pub(crate) fn snapshot_digest(
    path: &std::path::Path,
    key: &[u8; 32],
    workspace: &Workspace,
) -> anyhow::Result<(u64, String)> {
    io::snapshot_digest(path, key, workspace)
}

#[cfg(all(test, unix))]
mod tests;
