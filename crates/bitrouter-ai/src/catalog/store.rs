//! Explicit catalog persistence; no ambient paths or bundled data.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::types::RegistryData;

/// One complete provider/model snapshot, with its fetch timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogSnapshot {
    /// Unix seconds when both artifacts were fetched.
    pub fetched_at: u64,
    /// The complete parsed pair of artifacts.
    pub data: RegistryData,
}

/// Persistence acknowledgement for a successful catalog update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// Stored only in this process.
    Memory,
    /// The caller's persistent backend acknowledged the snapshot.
    Persistent,
}

/// Bounded persistence failures, without cached payloads or credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// The backend could not access or persist its data.
    #[error("catalog storage unavailable")]
    Unavailable,
    /// Persisted data could not be parsed.
    #[error("invalid persisted catalog")]
    Invalid,
}

/// Caller-owned snapshot storage, bound to the exact registry source.
///
/// Implementations must not return another source's snapshot or replace a
/// previous complete snapshot with partially written data. Cross-process and
/// crash guarantees depend on the backend, rather than this interface.
pub trait CatalogStore: Send + Sync {
    /// Load this source without network discovery.
    fn load(&self, source: &str) -> Result<Option<CatalogSnapshot>, StoreError>;
    /// Save the complete validated pair before acknowledging durability.
    fn save(&self, source: &str, snapshot: &CatalogSnapshot) -> Result<Durability, StoreError>;
}

/// Empty, process-local default storage. It reads no files or environment.
#[derive(Default)]
pub struct MemoryCatalogStore(Mutex<BTreeMap<String, CatalogSnapshot>>);

impl CatalogStore for MemoryCatalogStore {
    fn load(&self, source: &str) -> Result<Option<CatalogSnapshot>, StoreError> {
        Ok(self
            .0
            .lock()
            .map_err(|_| StoreError::Unavailable)?
            .get(source)
            .cloned())
    }

    fn save(&self, source: &str, snapshot: &CatalogSnapshot) -> Result<Durability, StoreError> {
        self.0
            .lock()
            .map_err(|_| StoreError::Unavailable)?
            .insert(source.into(), snapshot.clone());
        Ok(Durability::Memory)
    }
}
