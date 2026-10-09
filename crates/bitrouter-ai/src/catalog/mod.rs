//! Model/provider metadata without routing config, credentials or implicit I/O.
//!
//! A catalog begins empty with memory-only storage. Call `load` to inspect an
//! injected store, or `refresh` to request network discovery. The application
//! chooses sources, bundled baselines, storage paths and provider activation.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use store::{CatalogSnapshot, CatalogStore, Durability, MemoryCatalogStore, StoreError};
use types::RegistryData;

pub mod fetch;
pub mod store;
pub mod types;

/// Failures leave the catalog's previous complete snapshot available.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// Network or artifact decoding failed.
    #[error(transparent)]
    Fetch(#[from] fetch::FetchError),
    /// The caller's storage failed.
    #[error(transparent)]
    Storage(#[from] StoreError),
    /// An ambiguous/invalid identifier or empty protocol set was encountered.
    #[error("catalog has invalid or duplicate identifiers or empty model protocols")]
    InvalidData,
}

/// A single explicit source and its last complete validated snapshot.
pub struct Catalog {
    source: String,
    store: Arc<dyn CatalogStore>,
    current: Option<CatalogSnapshot>,
}

impl Catalog {
    /// Start empty with in-memory storage. This performs no discovery or reads.
    pub fn new(source: impl Into<String>) -> Self {
        Self::with_store(source, Arc::new(MemoryCatalogStore::default()))
    }

    /// Bind caller-selected storage without reading it implicitly.
    pub fn with_store(source: impl Into<String>, store: Arc<dyn CatalogStore>) -> Self {
        Self {
            source: source.into().trim_end_matches('/').into(),
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
            unix_seconds().saturating_sub(snapshot.fetched_at) <= max_age.as_secs()
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

    /// Fetch, validate and save both artifacts before publishing the replacement.
    /// Failed/disabled refreshes retain the previous snapshot and its timestamp.
    pub async fn refresh(
        &mut self,
        client: &reqwest::Client,
        policy: fetch::NetworkPolicy,
    ) -> Result<Durability, CatalogError> {
        let data = fetch::fetch_registry(client, &self.source, policy).await?;
        validate(&data)?;
        let snapshot = CatalogSnapshot {
            fetched_at: unix_seconds(),
            data,
        };
        let durability = self.store.save(&self.source, &snapshot)?;
        self.current = Some(snapshot);
        Ok(durability)
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn validate(data: &RegistryData) -> Result<(), CatalogError> {
    let mut providers = BTreeSet::new();
    let mut canonical = BTreeSet::new();
    for provider in &data.providers {
        if provider.name.trim().is_empty() || !providers.insert(&provider.name) {
            return Err(CatalogError::InvalidData);
        }
        let mut models = BTreeSet::new();
        for model in &provider.models {
            if model.id.trim().is_empty()
                || model.provider_model_id.trim().is_empty()
                || !models.insert(&model.id)
                || model.api_protocol.to_vec().is_empty()
            {
                return Err(CatalogError::InvalidData);
            }
        }
    }
    for model in &data.canonical {
        if model.id.trim().is_empty() || !canonical.insert(&model.id) {
            return Err(CatalogError::InvalidData);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
