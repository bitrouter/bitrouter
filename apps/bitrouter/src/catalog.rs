//! Application policy and file persistence for the AI catalog runtime.

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bitrouter_ai::catalog::Catalog;
use bitrouter_ai::catalog::fetch::NetworkPolicy;
use bitrouter_ai::catalog::store::{CatalogSnapshot, CatalogStore, Durability, StoreError};
use bitrouter_ai::catalog::types::RegistryData;
use bitrouter_sdk::config::RegistryConfig;
use serde::{Deserialize, Serialize};

const TTL: Duration = Duration::from_secs(24 * 60 * 60);
static WRITES: Mutex<()> = Mutex::new(());

struct DiskCache {
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct CachedPayload {
    // Pre-migration cache files had no source and may have come from a custom
    // registry. Preserve the file, but never infer its source during load.
    #[serde(default)]
    source: Option<String>,
    #[serde(flatten)]
    snapshot: CatalogSnapshot,
}

impl DiskCache {
    fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn default_path() -> Result<Self, StoreError> {
        let dir = if let Some(dir) = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
            PathBuf::from(dir).join("bitrouter")
        } else {
            #[cfg(windows)]
            if let Some(dir) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
                return Ok(Self::at(
                    PathBuf::from(dir).join("bitrouter/cache/registry.json"),
                ));
            }
            let home = std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .ok_or(StoreError::Unavailable)?;
            PathBuf::from(home).join(".cache/bitrouter")
        };
        Ok(Self::at(dir.join("registry.json")))
    }

    fn payload(&self) -> Result<Option<CachedPayload>, StoreError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(StoreError::Unavailable),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| StoreError::Invalid)
    }
}

impl CatalogStore for DiskCache {
    fn load(&self, source: &str) -> Result<Option<CatalogSnapshot>, StoreError> {
        let Some(payload) = self.payload()? else {
            return Ok(None);
        };
        let Some(bound) = payload
            .source
            .as_deref()
            .map(|source| source.trim_end_matches('/'))
        else {
            return Ok(None);
        };
        Ok((bound == source.trim_end_matches('/')).then_some(payload.snapshot))
    }

    fn save(&self, source: &str, snapshot: &CatalogSnapshot) -> Result<Durability, StoreError> {
        let _write = WRITES.lock().map_err(|_| StoreError::Unavailable)?;
        let parent = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or(StoreError::Unavailable)?;
        fs::create_dir_all(parent).map_err(|_| StoreError::Unavailable)?;
        let payload = CachedPayload {
            source: Some(source.trim_end_matches('/').into()),
            snapshot: snapshot.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&payload).map_err(|_| StoreError::Invalid)?;
        let tmp = self.path.with_extension("json.tmp");
        match fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(StoreError::Unavailable),
        }
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)
            .map_err(|_| StoreError::Unavailable)?;
        file.write_all(&bytes)
            .map_err(|_| StoreError::Unavailable)?;
        file.sync_all().map_err(|_| StoreError::Unavailable)?;
        fs::rename(&tmp, &self.path).map_err(|_| StoreError::Unavailable)?;
        #[cfg(unix)]
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| StoreError::Unavailable)?;
        Ok(Durability::Persistent)
    }
}

fn cached_catalog(source: &str) -> Catalog {
    let mut catalog = match DiskCache::default_path() {
        Ok(cache) => Catalog::with_store(source, Arc::new(cache)),
        Err(error) => {
            tracing::warn!(%error, "catalog cache directory unavailable; using process memory");
            Catalog::new(source)
        }
    };
    if let Err(error) = catalog.load() {
        tracing::warn!(%error, "catalog cache unavailable or invalid");
    }
    catalog
}

pub(crate) async fn load(registry: &RegistryConfig) -> Option<RegistryData> {
    if !registry.enabled {
        return None;
    }
    let mut catalog = cached_catalog(&registry.url);
    if !catalog.is_fresh(TTL) {
        match reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
        {
            Ok(client) => {
                if let Err(error) = catalog
                    .refresh(
                        &client,
                        NetworkPolicy::Allowed {
                            request_timeout: Duration::from_secs(15),
                        },
                    )
                    .await
                {
                    tracing::warn!(%error, stale = !catalog.is_fresh(TTL), available = catalog.snapshot().is_some(), "catalog refresh failed; retaining last complete snapshot");
                }
            }
            Err(error) => tracing::warn!(%error, "could not construct catalog HTTP client"),
        }
    }
    catalog.snapshot().map(|snapshot| snapshot.data.clone())
}

/// Credential-variable metadata for reload/onboarding, without network access.
/// Includes the cache's declared source and the application's public baseline.
/// This lists variable names; it never selects or merges another routing source.
pub fn credential_env_var_providers() -> Vec<(String, String)> {
    credential_variables(DiskCache::default_path().ok())
}

fn credential_variables(cache: Option<DiskCache>) -> Vec<(String, String)> {
    let cached = cache.and_then(|cache| {
        let source = cache.payload().ok().flatten()?.source?;
        let mut catalog = Catalog::with_store(source, Arc::new(cache));
        match catalog.load() {
            Ok(()) => catalog.snapshot().map(|snapshot| snapshot.data.clone()),
            Err(error) => {
                tracing::warn!(%error, "cached credential-variable metadata unavailable");
                None
            }
        }
    });
    let mut variables = crate::providers::apply::zero_config_env_var_providers(cached.as_ref());
    match crate::bundled_registry::supplement(&RegistryConfig::default(), None) {
        Ok(baseline) => {
            for entry in crate::providers::apply::zero_config_env_var_providers(baseline.as_ref()) {
                if !variables.iter().any(|(_, variable)| variable == &entry.1) {
                    variables.push(entry);
                }
            }
        }
        Err(error) => tracing::warn!(%error, "bundled credential-variable metadata unavailable"),
    }
    variables
}

#[cfg(test)]
mod tests;
