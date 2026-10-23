//! Source-bound, atomically persisted complete Registry v1 documents.

use super::RegistryData;
use super::{
    Catalog,
    store::{CatalogSnapshot, CatalogStore, Durability, StoreError},
};
use crate::config::RegistryConfig;
use bitrouter_registry::document::{MAX_DOCUMENT_BYTES, integer, parse_json};
use registry_json as serde_json;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

pub(super) const TTL: Duration = Duration::from_secs(24 * 60 * 60);
static MEMORY: std::sync::OnceLock<
    std::sync::Mutex<std::collections::BTreeMap<String, CatalogSnapshot>>,
> = std::sync::OnceLock::new();
pub(crate) fn remember(source: &str, snapshot: &CatalogSnapshot) {
    MEMORY
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(source.into(), snapshot.clone());
}
/// A single namespace uses one independent cache file per exact source and major.
pub(crate) struct DiskCache {
    directory: PathBuf,
}
impl DiskCache {
    pub(crate) fn at(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }
    pub(crate) fn default_path() -> Result<Self, StoreError> {
        if let Some(path) = std::env::var_os("XDG_CACHE_HOME").filter(|s| !s.is_empty()) {
            return Ok(Self::at(PathBuf::from(path).join("bitrouter/registry-v1")));
        }
        #[cfg(windows)]
        if let Some(path) = std::env::var_os("LOCALAPPDATA").filter(|s| !s.is_empty()) {
            return Ok(Self::at(
                PathBuf::from(path).join("bitrouter/cache/registry-v1"),
            ));
        }
        let home = std::env::var_os("HOME")
            .filter(|s| !s.is_empty())
            .ok_or(StoreError::Unavailable)?;
        let base = if cfg!(target_os = "macos") {
            "Library/Caches/bitrouter/registry-v1"
        } else {
            ".cache/bitrouter/registry-v1"
        };
        Ok(Self::at(PathBuf::from(home).join(base)))
    }
    fn path(&self, source: &str) -> PathBuf {
        self.directory.join(format!(
            "{}.json",
            hex::encode(Sha256::digest(format!("1\0{source}").as_bytes()))
        ))
    }
}
impl CatalogStore for DiskCache {
    fn load(&self, source: &str) -> Result<Option<CatalogSnapshot>, StoreError> {
        let mut file = match fs::File::open(self.path(source)) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(StoreError::Unavailable),
        };
        let mut bytes = Vec::new();
        std::io::Read::by_ref(&mut file)
            .take((MAX_DOCUMENT_BYTES + 65537) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| StoreError::Unavailable)?;
        if bytes.len() > MAX_DOCUMENT_BYTES + 65536 {
            return Err(StoreError::Invalid);
        }
        let split = bytes
            .iter()
            .position(|b| *b == b'\n')
            .filter(|n| *n <= 65536)
            .ok_or(StoreError::Invalid)?;
        let header = parse_json(&bytes[..split]).map_err(|_| StoreError::Invalid)?;
        if integer(&header["cache_version"]) != Some(1) || header["source"] != source {
            return Ok(None);
        }
        let data = super::decode::from_slice(&bytes[split + 1..]).map_err(|error| {
            tracing::warn!(%error, "registry cache admission failed; select a supported registry revision or upgrade BitRouter");
            StoreError::Invalid
        })?;
        Ok(Some(CatalogSnapshot {
            fetched_at: integer(&header["checked_at"]).unwrap_or(0),
            data,
        }))
    }
    fn save(&self, source: &str, snapshot: &CatalogSnapshot) -> Result<Durability, StoreError> {
        let raw = snapshot.data.document.as_ref().ok_or(StoreError::Invalid)?;
        let body = serde_json::to_vec(raw).map_err(|_| StoreError::Invalid)?;
        super::decode::from_slice(&body).map_err(|_| StoreError::Invalid)?;
        let header = serde_json::to_vec(
            &json!({"cache_version":1,"source":source,"checked_at":snapshot.fetched_at}),
        )
        .map_err(|_| StoreError::Invalid)?;
        if header.len() > 65536 {
            return Err(StoreError::Invalid);
        }
        fs::create_dir_all(&self.directory).map_err(|_| StoreError::Unavailable)?;
        let mut temp = tempfile::NamedTempFile::new_in(&self.directory)
            .map_err(|_| StoreError::Unavailable)?;
        temp.write_all(&header)
            .and_then(|_| temp.write_all(b"\n"))
            .and_then(|_| temp.write_all(&body))
            .and_then(|_| temp.as_file().sync_all())
            .map_err(|_| StoreError::Unavailable)?;
        temp.persist(self.path(source))
            .map_err(|_| StoreError::Unavailable)?;
        #[cfg(unix)]
        fs::File::open(&self.directory)
            .and_then(|dir| dir.sync_all())
            .map_err(|_| StoreError::Unavailable)?;
        Ok(Durability::Persistent)
    }
}
pub(crate) fn cached_catalog(source: &str) -> Catalog {
    if let Some(snapshot) = MEMORY
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(source)
        .cloned()
    {
        let mut catalog = match DiskCache::default_path() {
            Ok(cache) => Catalog::with_store(source, Arc::new(cache)),
            Err(_) => Catalog::new(source),
        };
        catalog.seed(snapshot);
        return catalog;
    }
    let mut catalog = match DiskCache::default_path() {
        Ok(cache) => Catalog::with_store(source, Arc::new(cache)),
        Err(_) => Catalog::new(source),
    };
    if let Err(error) = catalog.load() {
        tracing::warn!(%error,"registry cache unavailable or invalid; retained local fallback");
    }
    catalog
}
/// Local-only startup and CLI metadata. Network I/O belongs to the host worker.
pub(crate) async fn load(registry: &RegistryConfig) -> Option<RegistryData> {
    if !registry.enabled {
        return None;
    }
    cached_catalog(registry.source())
        .snapshot()
        .map(|s| s.data.clone())
}
/// Credential variable names from the build-selected local registry only.
pub fn credential_env_var_providers() -> Vec<(String, String)> {
    let cached = cached_catalog(crate::config::DEFAULT_REGISTRY_URL)
        .snapshot()
        .map(|s| s.data.clone());
    let local = super::bundled::supplement(&RegistryConfig::default(), cached)
        .ok()
        .flatten();
    crate::providers::apply::zero_config_env_var_providers(local.as_ref())
}
#[cfg(test)]
mod tests;
