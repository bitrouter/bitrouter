//! Persistent OAuth token store — one JSON file under the bitrouter data
//! directory, keyed by provider id.
//!
//! Layout:
//! - default path is `$XDG_DATA_HOME/bitrouter/oauth-tokens.json` (with
//!   `~/.local/share/bitrouter/...` and `%LOCALAPPDATA%\bitrouter\data\...`
//!   fallbacks). Follows the XDG Base Directory spec
//!   (<https://specifications.freedesktop.org/basedir-spec/latest/>).
//! - file permissions are 0600 on Unix — these tokens grant access to the
//!   user's upstream account; a co-tenant on the box must not be able to
//!   read them.
//! - the file is JSON: `{ "<provider_id>": OAuthToken, … }`.
//!
//! Writes are atomic-renamed from a sibling `.tmp` file, so a crash
//! mid-write can't truncate the store.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// One stored OAuth credential.
///
/// `Debug` redacts `access_token` and `refresh_token` so a future
/// `tracing::error!(?token, …)` can't dump the credential to the log stream.
#[derive(Clone, Serialize, Deserialize)]
pub struct OAuthToken {
    /// The credential the upstream API expects on `Authorization: Bearer …`.
    pub access_token: String,
    /// Unix seconds at which `access_token` becomes invalid. `0` means
    /// non-expiring (treat as valid forever).
    #[serde(default)]
    pub expires_at: u64,
    /// Optional refresh token. Some providers (GitHub OAuth Apps) don't issue
    /// one; OAuth Device Flow is then re-run when the access token expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

impl std::fmt::Debug for OAuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthToken")
            .field(
                "access_token",
                &if self.access_token.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .field("expires_at", &self.expires_at)
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl OAuthToken {
    /// Whether `access_token` is still valid at the current wall-clock time.
    /// Non-expiring tokens (`expires_at == 0`) always count as valid.
    pub fn is_valid(&self) -> bool {
        if self.expires_at == 0 {
            return true;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now < self.expires_at
    }
}

/// Errors raised by the token store.
#[derive(Debug, thiserror::Error)]
pub enum TokenStoreError {
    /// File I/O failure (open / write / chmod / mkdir).
    #[error("token-store I/O error at {path}: {source}")]
    Io {
        /// The path that failed.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// JSON parse / serialise failure.
    #[error("token-store JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// No home / data directory could be resolved.
    #[error("could not resolve a data directory for the token store")]
    NoDataDir,
}

/// Persistent OAuth token store backed by a single JSON file.
#[derive(Debug)]
pub struct TokenStore {
    path: PathBuf,
    tokens: HashMap<String, OAuthToken>,
}

/// Default filename inside the bitrouter data directory.
pub const DEFAULT_FILENAME: &str = "oauth-tokens.json";

impl TokenStore {
    /// Load the store from `path`. Missing file → empty store. Parse failure
    /// → error (deliberately not silent — a corrupt token file is something
    /// the operator must see).
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, TokenStoreError> {
        let path = path.into();
        let tokens = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => HashMap::new(),
            Err(source) => {
                return Err(TokenStoreError::Io {
                    path: path.clone(),
                    source,
                });
            }
        };
        Ok(Self { path, tokens })
    }

    /// Resolve the default store path under `$XDG_DATA_HOME/bitrouter/`.
    pub fn default_path() -> Result<Self, TokenStoreError> {
        let dir = default_data_dir()?;
        Self::load(dir.join(DEFAULT_FILENAME))
    }

    /// Path the store reads + writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Look up `provider_id`'s token. Returns `None` for unknown providers
    /// AND for tokens past their `expires_at` — callers either re-run the
    /// device flow or refresh.
    pub fn get(&self, provider_id: &str) -> Option<&OAuthToken> {
        let token = self.tokens.get(provider_id)?;
        token.is_valid().then_some(token)
    }

    /// Look up `provider_id`'s token regardless of expiry — useful when the
    /// caller wants to attempt a refresh using a stored `refresh_token`.
    pub fn get_any(&self, provider_id: &str) -> Option<&OAuthToken> {
        self.tokens.get(provider_id)
    }

    /// Store a token for `provider_id` and persist to disk.
    pub fn set(&mut self, provider_id: &str, token: OAuthToken) -> Result<(), TokenStoreError> {
        self.tokens.insert(provider_id.to_string(), token);
        self.flush()
    }

    /// Remove `provider_id`'s token and persist. Returns the removed token if
    /// any was present.
    pub fn remove(&mut self, provider_id: &str) -> Result<Option<OAuthToken>, TokenStoreError> {
        let removed = self.tokens.remove(provider_id);
        if removed.is_some() {
            self.flush()?;
        }
        Ok(removed)
    }

    fn flush(&self) -> Result<(), TokenStoreError> {
        let parent = self.path.parent().ok_or(TokenStoreError::NoDataDir)?;
        fs::create_dir_all(parent).map_err(|source| TokenStoreError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        let bytes = serde_json::to_vec_pretty(&self.tokens)?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, &bytes).map_err(|source| TokenStoreError::Io {
            path: tmp.clone(),
            source,
        })?;
        // chmod 0600 BEFORE the rename, so the file is never visible to
        // co-tenants with world-readable permissions even for an instant.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            if let Err(source) = fs::set_permissions(&tmp, perms) {
                return Err(TokenStoreError::Io {
                    path: tmp.clone(),
                    source,
                });
            }
        }
        fs::rename(&tmp, &self.path).map_err(|source| TokenStoreError::Io {
            path: self.path.clone(),
            source,
        })?;
        Ok(())
    }
}

/// Resolve the bitrouter data directory. XDG Base Directory on Unix,
/// `%LOCALAPPDATA%\bitrouter\data` on Windows.
fn default_data_dir() -> Result<PathBuf, TokenStoreError> {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir).join("bitrouter"));
    }
    #[cfg(windows)]
    if let Some(dir) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir).join("bitrouter").join("data"));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("bitrouter"));
    }
    Err(TokenStoreError::NoDataDir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("bitrouter-token-store-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trip() {
        let dir = tmp_dir();
        let path = dir.join("tokens.json");
        {
            let mut store = TokenStore::load(&path).unwrap();
            assert!(store.get("github-copilot").is_none());
            store
                .set(
                    "github-copilot",
                    OAuthToken {
                        access_token: "ghu_test".into(),
                        expires_at: 0,
                        refresh_token: None,
                    },
                )
                .unwrap();
        }
        let reloaded = TokenStore::load(&path).unwrap();
        assert_eq!(
            reloaded.get("github-copilot").map(|t| &t.access_token),
            Some(&"ghu_test".to_string())
        );
    }

    #[test]
    fn expired_returns_none_from_get_but_some_from_get_any() {
        let dir = tmp_dir();
        let mut store = TokenStore::load(dir.join("tokens.json")).unwrap();
        store
            .set(
                "expired",
                OAuthToken {
                    access_token: "x".into(),
                    expires_at: 1, // far past
                    refresh_token: None,
                },
            )
            .unwrap();
        assert!(store.get("expired").is_none());
        assert!(store.get_any("expired").is_some());
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = tmp_dir();
        let store = TokenStore::load(dir.join("never-written.json")).unwrap();
        assert!(store.get("anything").is_none());
    }

    #[test]
    fn corrupt_file_errors() {
        let dir = tmp_dir();
        let path = dir.join("tokens.json");
        fs::write(&path, b"not json").unwrap();
        let err = TokenStore::load(&path).unwrap_err();
        assert!(matches!(err, TokenStoreError::Json(_)));
    }

    #[test]
    fn debug_redacts() {
        let token = OAuthToken {
            access_token: "very-secret".into(),
            expires_at: 1700000000,
            refresh_token: Some("also-secret".into()),
        };
        let dbg = format!("{token:?}");
        assert!(!dbg.contains("very-secret"));
        assert!(!dbg.contains("also-secret"));
        assert!(dbg.contains("<redacted>"));
    }

    #[cfg(unix)]
    #[test]
    fn file_perms_are_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir();
        let path = dir.join("tokens.json");
        let mut store = TokenStore::load(&path).unwrap();
        store
            .set(
                "x",
                OAuthToken {
                    access_token: "x".into(),
                    expires_at: 0,
                    refresh_token: None,
                },
            )
            .unwrap();
        let meta = fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
    }
}
