//! File-based on-disk cache for the [models.dev](https://models.dev) catalog.
//!
//! Layout: a single JSON file containing `{ "fetched_at": <unix-secs>, "catalog": <Catalog> }`.
//! Default path: `$XDG_CACHE_HOME/bitrouter/models-dev.json` (falling back to
//! `~/.cache/bitrouter/models-dev.json` on Unix or `%LOCALAPPDATA%\bitrouter\cache\models-dev.json`
//! on Windows). The location matches the XDG Base Directory spec
//! (<https://specifications.freedesktop.org/basedir-spec/latest/>).
//!
//! TTL: 24h. After expiry the cache is "stale" — callers should re-fetch,
//! but [`DiskCache::read_any`] still returns the stale data so a network
//! outage doesn't blank the model list.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::catalog::types::Catalog;

/// Freshness window — 24 hours. After this the cached payload is "stale".
pub const TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Default filename inside the bitrouter cache directory.
pub const DEFAULT_FILENAME: &str = "models-dev.json";

/// Errors raised by the disk cache.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// File I/O failure (open / write / chmod / mkdir).
    #[error("cache I/O error at {path}: {source}")]
    Io {
        /// The path that failed.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// JSON parse / serialise failure.
    #[error("cache JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// No home / cache directory could be resolved (no `HOME` env var, no
    /// `XDG_CACHE_HOME`).
    #[error("could not resolve a cache directory")]
    NoCacheDir,
}

/// One on-disk cache slot.
pub struct DiskCache {
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedPayload {
    /// Unix seconds when the catalog was fetched.
    fetched_at: u64,
    catalog: Catalog,
}

impl DiskCache {
    /// Use an explicit cache path. Tests use this; the binary calls
    /// [`Self::default_path`].
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Resolve the default cache file under `$XDG_CACHE_HOME/bitrouter/`.
    pub fn default_path() -> Result<Self, CacheError> {
        let dir = default_cache_dir()?;
        Ok(Self::at(dir.join(DEFAULT_FILENAME)))
    }

    /// Path the cache reads + writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the cached catalog if it exists AND is within the TTL window.
    /// Returns `Ok(None)` for missing or stale entries — callers that don't
    /// care about freshness should call [`Self::read_any`].
    pub fn read_fresh(&self) -> Result<Option<Catalog>, CacheError> {
        let Some((payload, age)) = self.read_with_age()? else {
            return Ok(None);
        };
        if age <= TTL {
            Ok(Some(payload.catalog))
        } else {
            Ok(None)
        }
    }

    /// Read the cached catalog whether or not it is fresh. Returns `Ok(None)`
    /// only when the file is absent or unreadable. Use after [`fetch_catalog`]
    /// fails so a transient network outage still serves model metadata.
    ///
    /// [`fetch_catalog`]: super::fetch::fetch_catalog
    pub fn read_any(&self) -> Result<Option<Catalog>, CacheError> {
        Ok(self.read_with_age()?.map(|(p, _)| p.catalog))
    }

    /// Write a freshly-fetched catalog, stamping the current time.
    pub fn write(&self, catalog: &Catalog) -> Result<(), CacheError> {
        let parent = self.path.parent().ok_or(CacheError::NoCacheDir)?;
        fs::create_dir_all(parent).map_err(|source| CacheError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let payload = CachedPayload {
            fetched_at: now,
            catalog: catalog.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&payload)?;
        let tmp = self.path.with_extension("json.tmp");
        // atomic-replace: write to a sibling file then rename, so a crash
        // mid-write never leaves a half-truncated cache.
        fs::write(&tmp, &bytes).map_err(|source| CacheError::Io {
            path: tmp.clone(),
            source,
        })?;
        fs::rename(&tmp, &self.path).map_err(|source| CacheError::Io {
            path: self.path.clone(),
            source,
        })?;
        Ok(())
    }

    fn read_with_age(&self) -> Result<Option<(CachedPayload, Duration)>, CacheError> {
        let bytes = match fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(CacheError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let payload: CachedPayload = serde_json::from_slice(&bytes)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let age = Duration::from_secs(now.saturating_sub(payload.fetched_at));
        Ok(Some((payload, age)))
    }
}

/// Resolve the bitrouter cache directory. Follows the XDG Base Directory
/// spec on Unix; uses `%LOCALAPPDATA%` on Windows.
fn default_cache_dir() -> Result<PathBuf, CacheError> {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir).join("bitrouter"));
    }
    #[cfg(windows)]
    if let Some(dir) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir).join("bitrouter").join("cache"));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(home).join(".cache").join("bitrouter"));
    }
    Err(CacheError::NoCacheDir)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::catalog::types::{ModelMetadata, ProviderCatalogEntry};

    fn sample_catalog() -> Catalog {
        let mut models = BTreeMap::new();
        models.insert(
            "gpt-4o".to_string(),
            ModelMetadata {
                id: "gpt-4o".into(),
                name: "GPT-4o".into(),
                family: Some("gpt".into()),
                attachment: true,
                reasoning: false,
                tool_call: true,
                structured_output: true,
                temperature: true,
                knowledge: None,
                release_date: None,
                last_updated: None,
                modalities: Default::default(),
                open_weights: false,
                limit: Default::default(),
                cost: Default::default(),
            },
        );
        let mut cat = BTreeMap::new();
        cat.insert(
            "openai".into(),
            ProviderCatalogEntry {
                id: "openai".into(),
                name: "OpenAI".into(),
                env: vec!["OPENAI_API_KEY".into()],
                api: Some("https://api.openai.com/v1".into()),
                npm: None,
                doc: None,
                models,
            },
        );
        cat
    }

    /// Unique tmp dir per call — tests run in parallel and a shared directory
    /// races with itself (creator A removes the directory while reader B is
    /// opening files inside it, surfacing as `EINVAL` on macOS).
    fn tmp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-providers-cache-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_and_reads_fresh() {
        let dir = tmp_dir();
        let cache = DiskCache::at(dir.join("models.json"));
        let sample = sample_catalog();
        cache.write(&sample).unwrap();
        let got = cache.read_fresh().unwrap().expect("fresh read");
        assert_eq!(got.len(), 1);
        assert!(got.contains_key("openai"));
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = tmp_dir();
        let cache = DiskCache::at(dir.join("absent.json"));
        assert!(cache.read_fresh().unwrap().is_none());
        assert!(cache.read_any().unwrap().is_none());
    }

    #[test]
    fn stale_payload_only_visible_via_read_any() {
        let dir = tmp_dir();
        let path = dir.join("models.json");
        // Hand-craft a cached file dated 1970 — definitely past the 24h TTL.
        let stale = CachedPayload {
            fetched_at: 0,
            catalog: sample_catalog(),
        };
        fs::write(&path, serde_json::to_vec_pretty(&stale).unwrap()).unwrap();
        let cache = DiskCache::at(&path);
        assert!(
            cache.read_fresh().unwrap().is_none(),
            "TTL must reject stale"
        );
        let any = cache.read_any().unwrap();
        assert!(any.is_some(), "read_any must still surface stale data");
    }

    #[test]
    fn rejects_corrupt_payload() {
        let dir = tmp_dir();
        let path = dir.join("models.json");
        fs::write(&path, b"not json").unwrap();
        let cache = DiskCache::at(&path);
        assert!(matches!(cache.read_any(), Err(CacheError::Json(_))));
    }
}
