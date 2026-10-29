//! Model/provider metadata without routing config, credentials or implicit I/O.
//!
//! A catalog begins empty with memory-only storage. Call `load` to inspect an
//! injected store, or `refresh` to request network discovery. The application
//! chooses sources, bundled baselines, storage paths and provider activation.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bitrouter_ai::catalog::types::{CanonicalModel, RegistryProvider};
use store::{CatalogSnapshot, CatalogStore, Durability, MemoryCatalogStore, StoreError};

/// Application-owned admitted snapshot and its legacy UI projections. The
/// lightweight public AI contracts do not depend on the exact JSON engine.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RegistryData {
    /// Complete exact document; registry caches serialize this field directly.
    #[serde(skip)]
    pub document: Option<registry_json::Value>,
    /// Understood provider metadata for local UI and configuration projection.
    pub providers: Vec<RegistryProvider>,
    /// Understood metadata for the independent recommendation list.
    pub canonical: Vec<CanonicalModel>,
}

pub(crate) mod bundled;
pub mod cache;
pub(crate) mod decode;
pub mod execution;
pub mod fetch;
pub(crate) mod refresh;
pub mod store;

/// Failures leave the catalog's previous complete snapshot available.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// Network or artifact decoding failed.
    #[error(transparent)]
    Fetch(#[from] fetch::FetchError),
    /// The caller's storage failed.
    #[error(transparent)]
    Storage(#[from] StoreError),
    /// Complete document admission failed with actionable structured details.
    #[error(transparent)]
    Admission(#[from] bitrouter_registry::document::RegistryError),
    /// A caller supplied a projection without its complete Registry v1 document.
    #[error("registry.invalid_data: a complete Registry v1 document is required")]
    InvalidData,
    /// A valid same-source response tried to move catalog time backwards.
    #[error(
        "registry.invalid_data at /as_of: registry rollback rejected; select an explicit historical source URL to load older data"
    )]
    Rollback,
}

/// A single explicit source and its last complete validated snapshot.
pub struct Catalog {
    source: String,
    store: Arc<dyn CatalogStore>,
    current: Option<CatalogSnapshot>,
}

impl Catalog {
    /// Seed an already admitted local alternative, without inventing a check.
    pub(crate) fn seed(&mut self, snapshot: CatalogSnapshot) {
        self.current = Some(snapshot);
    }
    /// Start empty with in-memory storage. This performs no discovery or reads.
    pub fn new(source: impl Into<String>) -> Self {
        Self::with_store(source, Arc::new(MemoryCatalogStore::default()))
    }

    /// Bind caller-selected storage without reading it implicitly.
    pub fn with_store(source: impl Into<String>, store: Arc<dyn CatalogStore>) -> Self {
        Self {
            source: source.into(),
            store,
            current: None,
        }
    }

    /// Inspect loaded data without auth, network or application configuration.
    pub fn snapshot(&self) -> Option<&CatalogSnapshot> {
        self.current.as_ref()
    }

    /// Report freshness separately from availability using the caller's window.
    pub fn is_fresh(&self, max_age: Duration) -> bool {
        self.current.as_ref().is_some_and(|snapshot| {
            snapshot.fetched_at > 0
                && unix_seconds()
                    .checked_sub(snapshot.fetched_at)
                    .is_some_and(|age| age < max_age.as_secs())
        })
    }

    /// Read the selected source from storage, retaining current data on failure.
    pub fn load(&mut self) -> Result<(), CatalogError> {
        if let Some(snapshot) = self.store.load(&self.source)? {
            validate(&snapshot.data)?;
            self.current = Some(snapshot);
        }
        Ok(())
    }

    /// Fetch, validate and save one complete document before replacement.
    /// Failed/disabled refreshes retain the previous snapshot and its timestamp.
    pub async fn refresh(
        &mut self,
        client: &reqwest::Client,
        policy: fetch::NetworkPolicy,
    ) -> Result<Durability, CatalogError> {
        let snapshot = self.download(client, policy).await?;
        Ok(self.admit(snapshot))
    }

    /// Stage a complete admitted response without changing memory or disk.
    pub(crate) async fn download(
        &self,
        client: &reqwest::Client,
        policy: fetch::NetworkPolicy,
    ) -> Result<CatalogSnapshot, CatalogError> {
        let data = fetch::fetch_registry(client, &self.source, policy).await?;
        validate(&data)?;
        let snapshot = CatalogSnapshot {
            fetched_at: unix_seconds(),
            data,
        };
        if self
            .current
            .as_ref()
            .and_then(|s| s.data.document.as_ref())
            .zip(snapshot.data.document.as_ref())
            .is_some_and(|(old, new)| {
                old["as_of"]
                    .as_str()
                    .and_then(bitrouter_registry::document::timestamp)
                    > new["as_of"]
                        .as_str()
                        .and_then(bitrouter_registry::document::timestamp)
            })
        {
            return Err(CatalogError::Rollback);
        }
        Ok(snapshot)
    }

    /// Commit only after the host fences the matching routing publication.
    pub(crate) fn admit(&mut self, snapshot: CatalogSnapshot) -> Durability {
        let durability = self
            .store
            .save(&self.source, &snapshot)
            .unwrap_or_else(|error| {
                tracing::warn!(%error,"registry admitted in memory; cache persistence unavailable");
                Durability::Memory
            });
        self.current = Some(snapshot);
        durability
    }
}

pub(crate) fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn validate(data: &RegistryData) -> Result<(), CatalogError> {
    let raw = data.document.as_ref().ok_or(CatalogError::InvalidData)?;
    bitrouter_registry::document::Document::from_value(
        raw.clone(),
        &bitrouter_registry::document::ReaderPolicy::default(),
    )
    .map_err(CatalogError::Admission)?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
