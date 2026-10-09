//! Explicit-path credential file snapshots and administrative mutations.
//! Default locations, account choice and credential-source discovery belong to
//! the caller. Refresh uses the sibling backend's selected-account transactions.
//!
//! ## On-disk layout
//!
//! ```json
//! {
//!   "anthropic": {
//!     "default":  { "type": "oauth",   "data": { "access_token": "sk-ant-oat…", "expires_at": 1234567890, "refresh_token": "…" } },
//!     "work-key": { "type": "api_key", "data": { "value": "sk-ant-api03-…" } }
//!   },
//!   "openai-codex":   { "default": { "type": "oauth",   "data": { … } } },
//!   "github-copilot": { "default": { "type": "oauth",   "data": { … } } }
//! }
//! ```
//!
//! The legacy flat shape — `{ "<provider_id>": OAuthToken }`, written by the
//! pre-feature-`pkce` device-code login — is detected at load time and
//! migrated transparently: each `(provider_id, OAuthToken)` becomes
//! `(provider_id, { "default": Credential::Oauth(OAuthToken) })`. The
//! migrated store is written back on the next mutating call; read-only loads
//! never touch disk.
//!
//! Filesystem details:
//! - file permissions are 0600 on Unix — these credentials grant access to
//!   the user's upstream account; a co-tenant on the box must not be able
//!   to read them.
//! - writes are atomic-renamed from a sibling `.tmp` file, so a crash
//!   mid-write can't truncate the store.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

// Only the bounded file read-modify-write is serialized. No network I/O runs
// under this lock; independent account refreshes use their own async leases.
static STORE_WRITES: Mutex<()> = Mutex::new(());

use crate::auth::credentials::{Credential, OAuthToken};
use crate::auth::store::DEFAULT_ACCOUNT;
use serde::Deserialize;

/// Errors raised by the credential store.
#[derive(Debug, thiserror::Error)]
pub enum CredentialStoreError {
    /// File I/O failure (open / write / chmod / mkdir).
    #[error("credential-store I/O error at {path}: {source}")]
    Io {
        /// The path that failed.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// JSON parse / serialise failure.
    #[error("credential-store JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// The selected path cannot name a credential file.
    #[error("selected credential path does not name a file")]
    InvalidPath,
    /// Another writer replaced the selected credential during refresh.
    #[error("selected credential changed before refresh commit")]
    Conflict,
    /// A previous file writer failed while holding the process lock.
    #[error("credential-store write lock unavailable")]
    WriteLock,
}

/// Persistent credential store backed by a single JSON file. See the module
/// docs for the on-disk layout and migration behaviour.
pub struct CredentialStore {
    path: PathBuf,
    /// `provider_id -> label -> Credential`. `BTreeMap` for the inner map so
    /// the serialised label order is deterministic (helps human-diffing
    /// the file).
    creds: HashMap<String, BTreeMap<String, Credential>>,
}

impl std::fmt::Debug for CredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialStore")
            .field("path", &"<caller-selected path>")
            .field("providers", &self.creds.len())
            .field(
                "slots",
                &self.creds.values().map(BTreeMap::len).sum::<usize>(),
            )
            .finish()
    }
}

/// On-disk wire format. Used both for the new labeled layout and to detect
/// the legacy flat-keyed layout produced by pre-feature-`pkce` device-code
/// logins (each value parses straight as an [`OAuthToken`]).
#[derive(Deserialize)]
#[serde(untagged)]
enum WireFormat {
    /// New labeled layout — what this store writes from here on.
    Labeled(HashMap<String, BTreeMap<String, Credential>>),
    /// Legacy flat layout — `{ "<provider_id>": OAuthToken }`. Migrated
    /// into the labeled layout on read, with each entry placed under
    /// [`DEFAULT_ACCOUNT`].
    Legacy(HashMap<String, OAuthToken>),
}

impl CredentialStore {
    /// Load the store from `path`. Missing file → empty store. Parse failure
    /// → error (deliberately not silent — a corrupt credential file is
    /// something the operator must see).
    ///
    /// Detects the legacy flat-keyed layout transparently; the migrated
    /// store is held in memory and persisted on the next mutating call.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, CredentialStoreError> {
        let input = path.into();
        let path = super::selected_path(&input).map_err(|source| CredentialStoreError::Io {
            path: input,
            source,
        })?;
        if path.file_name().is_none() {
            return Err(CredentialStoreError::InvalidPath);
        }
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    creds: HashMap::new(),
                });
            }
            Err(source) => {
                return Err(CredentialStoreError::Io {
                    path: path.clone(),
                    source,
                });
            }
        };
        let creds = match serde_json::from_slice::<WireFormat>(&bytes)? {
            WireFormat::Labeled(m) => m,
            WireFormat::Legacy(flat) => flat
                .into_iter()
                .map(|(id, token)| {
                    let mut m = BTreeMap::new();
                    m.insert(DEFAULT_ACCOUNT.to_string(), Credential::Oauth(token));
                    (id, m)
                })
                .collect(),
        };
        Ok(Self { path, creds })
    }

    /// Path the store reads + writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Borrow `(provider_id, label)`'s credential. Returns `None` for an
    /// unknown `(provider, label)` AND for OAuth credentials past their
    /// `expires_at` — callers either refresh or re-run the login flow.
    /// API-key credentials have no expiry and are always returned.
    pub fn get(&self, provider_id: &str, label: &str) -> Option<&Credential> {
        let c = self.creds.get(provider_id)?.get(label)?;
        c.is_valid().then_some(c)
    }

    /// Borrow `(provider_id, label)`'s credential regardless of expiry —
    /// useful when the caller wants to attempt a refresh using a stored
    /// `refresh_token`.
    pub fn get_any(&self, provider_id: &str, label: &str) -> Option<&Credential> {
        self.creds.get(provider_id)?.get(label)
    }

    /// Store `credential` at `(provider_id, label)` and persist to disk.
    pub fn set(
        &mut self,
        provider_id: &str,
        label: &str,
        credential: Credential,
    ) -> Result<(), CredentialStoreError> {
        let _guard = STORE_WRITES
            .lock()
            .map_err(|_| CredentialStoreError::WriteLock)?;
        let mut latest = Self::load(&self.path)?;
        latest
            .creds
            .entry(provider_id.to_string())
            .or_default()
            .insert(label.to_string(), credential);
        latest.flush()?;
        self.creds = latest.creds;
        Ok(())
    }

    /// Remove the selected credential from the latest file state and persist.
    pub fn remove(
        &mut self,
        provider_id: &str,
        label: &str,
    ) -> Result<Option<Credential>, CredentialStoreError> {
        let _guard = STORE_WRITES
            .lock()
            .map_err(|_| CredentialStoreError::WriteLock)?;
        let mut latest = Self::load(&self.path)?;
        let removed = latest
            .creds
            .get_mut(provider_id)
            .and_then(|m| m.remove(label));
        if latest
            .creds
            .get(provider_id)
            .is_some_and(BTreeMap::is_empty)
        {
            latest.creds.remove(provider_id);
        }
        if removed.is_some() {
            latest.flush()?;
        }
        self.creds = latest.creds;
        Ok(removed)
    }

    /// Remove all labels for a provider from the latest file state and persist.
    pub fn remove_all_for(&mut self, provider_id: &str) -> Result<usize, CredentialStoreError> {
        let _guard = STORE_WRITES
            .lock()
            .map_err(|_| CredentialStoreError::WriteLock)?;
        let mut latest = Self::load(&self.path)?;
        let removed = latest
            .creds
            .remove(provider_id)
            .map(|m| m.len())
            .unwrap_or(0);
        if removed > 0 {
            latest.flush()?;
        }
        self.creds = latest.creds;
        Ok(removed)
    }

    /// Commit a rotated token only while the selected original credential still
    /// owns its slot. An already-written replacement is safe to acknowledge again.
    pub(super) fn compare_exchange(
        &mut self,
        provider_id: &str,
        label: &str,
        expected: Option<&Credential>,
        replacement: Credential,
    ) -> Result<(), CredentialStoreError> {
        let _guard = STORE_WRITES
            .lock()
            .map_err(|_| CredentialStoreError::WriteLock)?;
        let mut latest = Self::load(&self.path)?;
        let current = latest.get_any(provider_id, label);
        if current != expected && current != Some(&replacement) {
            return Err(CredentialStoreError::Conflict);
        }
        latest
            .creds
            .entry(provider_id.to_string())
            .or_default()
            .insert(label.to_string(), replacement);
        latest.flush()?;
        self.creds = latest.creds;
        Ok(())
    }

    /// List the labels stored for `provider_id`, in deterministic order.
    pub fn labels(&self, provider_id: &str) -> Vec<&str> {
        self.creds
            .get(provider_id)
            .map(|m| m.keys().map(String::as_str).collect())
            .unwrap_or_default()
    }

    /// List every provider id that has at least one stored credential.
    pub fn providers(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.creds.keys().map(String::as_str).collect();
        out.sort_unstable();
        out
    }

    fn flush(&self) -> Result<(), CredentialStoreError> {
        let parent = self
            .path
            .parent()
            .ok_or(CredentialStoreError::InvalidPath)?;
        fs::create_dir_all(parent).map_err(|source| CredentialStoreError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        let bytes = serde_json::to_vec_pretty(&self.creds)?;
        let tmp = self.path.with_extension("json.tmp");
        // Create the temp file owner-only (0600) from the instant it exists, so
        // the tokens never sit on a world-/group-readable file even for the
        // width of the write — the exact co-tenant read window a `fs::write`
        // followed by a later `chmod` leaves open. A stale temp from a crashed
        // run is cleared first so `create_new` always makes a fresh 0600 file
        // (and refuses to follow a symlink a co-tenant may have planted).
        #[cfg(unix)]
        {
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            let _ = fs::remove_file(&tmp);
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|source| CredentialStoreError::Io {
                    path: tmp.clone(),
                    source,
                })?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|source| CredentialStoreError::Io {
                    path: tmp.clone(),
                    source,
                })?;
        }
        #[cfg(not(unix))]
        {
            use std::io::Write as _;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)
                .map_err(|source| CredentialStoreError::Io {
                    path: tmp.clone(),
                    source,
                })?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|source| CredentialStoreError::Io {
                    path: tmp.clone(),
                    source,
                })?;
        }
        fs::rename(&tmp, &self.path).map_err(|source| CredentialStoreError::Io {
            path: self.path.clone(),
            source,
        })?;
        #[cfg(unix)]
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| CredentialStoreError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
    use super::*;

    fn tmp_dir() -> TestResult<PathBuf> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-credential-store-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    #[test]
    fn round_trip_oauth() -> TestResult {
        let dir = tmp_dir()?;
        let path = dir.join("oauth-tokens.json");
        {
            let mut store = CredentialStore::load(&path)?;
            assert!(store.get("anthropic", DEFAULT_ACCOUNT).is_none());
            store.set(
                "anthropic",
                DEFAULT_ACCOUNT,
                Credential::from_oauth_token(OAuthToken {
                    access_token: "sk-ant-oat-test".into(),
                    expires_at: 0,
                    refresh_token: Some("refresh".into()),
                }),
            )?;
        }
        let reloaded = CredentialStore::load(&path)?;
        let got = reloaded
            .get("anthropic", DEFAULT_ACCOUNT)
            .ok_or("missing credential")?;
        let oauth = got.as_oauth().ok_or("missing credential")?;
        assert_eq!(oauth.access_token, "sk-ant-oat-test");
        assert_eq!(oauth.refresh_token.as_deref(), Some("refresh"));
        Ok(())
    }

    #[test]
    fn round_trip_api_key() -> TestResult {
        let dir = tmp_dir()?;
        let path = dir.join("oauth-tokens.json");
        {
            let mut store = CredentialStore::load(&path)?;
            store.set(
                "anthropic",
                "work",
                Credential::api_key("sk-ant-api03-secret"),
            )?;
        }
        let reloaded = CredentialStore::load(&path)?;
        let got = reloaded
            .get("anthropic", "work")
            .ok_or("missing credential")?;
        assert_eq!(got.as_api_key(), Some("sk-ant-api03-secret"));
        Ok(())
    }

    #[test]
    fn round_trip_claude_code_cli_marker() -> TestResult {
        let dir = tmp_dir()?;
        let path = dir.join("oauth-tokens.json");
        {
            let mut store = CredentialStore::load(&path)?;
            store.set("anthropic", DEFAULT_ACCOUNT, Credential::ClaudeCodeCli)?;
        }
        let reloaded = CredentialStore::load(&path)?;
        let got = reloaded
            .get_any("anthropic", DEFAULT_ACCOUNT)
            .ok_or("missing credential")?;
        assert!(matches!(got, Credential::ClaudeCodeCli));
        assert_eq!(got.kind_label(), "Claude Code session");
        assert!(got.as_oauth().is_none());
        assert!(got.as_api_key().is_none());
        // The marker itself is always "valid"; whether a live session exists is
        // decided at request time by the applier.
        assert!(got.is_valid());
        assert!(reloaded.get("anthropic", DEFAULT_ACCOUNT).is_some());
        // Serialized as the adjacently-tagged unit variant, no `data` payload.
        let raw = std::fs::read_to_string(&path)?;
        assert!(raw.contains("claude_code_cli"), "got: {raw}");
        Ok(())
    }

    #[test]
    fn multiple_labels_per_provider() -> TestResult {
        let dir = tmp_dir()?;
        let mut store = CredentialStore::load(dir.join("oauth-tokens.json"))?;
        store.set(
            "anthropic",
            "pro-max",
            Credential::from_oauth_token(OAuthToken {
                access_token: "sk-ant-oat-1".into(),
                expires_at: 0,
                refresh_token: None,
            }),
        )?;
        store.set(
            "anthropic",
            "work-key",
            Credential::api_key("sk-ant-api03-2"),
        )?;
        let mut labels = store.labels("anthropic");
        labels.sort();
        assert_eq!(labels, vec!["pro-max", "work-key"]);
        assert!(store.get("anthropic", "pro-max").is_some());
        assert!(store.get("anthropic", "work-key").is_some());
        Ok(())
    }

    #[test]
    fn expired_oauth_returns_none_from_get_but_some_from_get_any() -> TestResult {
        let dir = tmp_dir()?;
        let mut store = CredentialStore::load(dir.join("oauth-tokens.json"))?;
        store.set(
            "anthropic",
            DEFAULT_ACCOUNT,
            Credential::from_oauth_token(OAuthToken {
                access_token: "x".into(),
                expires_at: 1, // far past
                refresh_token: Some("r".into()),
            }),
        )?;
        assert!(store.get("anthropic", DEFAULT_ACCOUNT).is_none());
        assert!(store.get_any("anthropic", DEFAULT_ACCOUNT).is_some());
        Ok(())
    }

    #[test]
    fn api_key_never_expires() -> TestResult {
        let dir = tmp_dir()?;
        let mut store = CredentialStore::load(dir.join("oauth-tokens.json"))?;
        store.set("anthropic", "k", Credential::api_key("static"))?;
        assert!(store.get("anthropic", "k").is_some());
        Ok(())
    }

    #[test]
    fn legacy_flat_format_migrates_to_default_label() -> TestResult {
        let dir = tmp_dir()?;
        let path = dir.join("oauth-tokens.json");
        // Write the legacy flat layout — what the pre-pkce TokenStore wrote.
        let legacy_json = r#"{
          "github-copilot": { "access_token": "ghu_legacy_test", "expires_at": 0 }
        }"#;
        fs::write(&path, legacy_json)?;
        let store = CredentialStore::load(&path)?;
        let got = store
            .get("github-copilot", DEFAULT_ACCOUNT)
            .ok_or("missing credential")?;
        let oauth = got.as_oauth().ok_or("missing credential")?;
        assert_eq!(oauth.access_token, "ghu_legacy_test");
        Ok(())
    }

    #[test]
    fn legacy_migration_persists_in_new_format_on_next_write() -> TestResult {
        let dir = tmp_dir()?;
        let path = dir.join("oauth-tokens.json");
        // Seed legacy.
        let legacy_json = r#"{
          "github-copilot": { "access_token": "ghu_legacy", "expires_at": 0 }
        }"#;
        fs::write(&path, legacy_json)?;
        let mut store = CredentialStore::load(&path)?;
        // Mutate to force a write — adding a new credential is enough.
        store.set("anthropic", DEFAULT_ACCOUNT, Credential::api_key("k"))?;
        // On-disk file should now be the new labeled format. Re-load and
        // confirm both entries are addressable via the new API.
        let reloaded = CredentialStore::load(&path)?;
        assert!(reloaded.get("github-copilot", DEFAULT_ACCOUNT).is_some());
        assert!(reloaded.get("anthropic", DEFAULT_ACCOUNT).is_some());
        // The raw bytes should now carry the new "type"/"data" tagging,
        // not legacy flat OAuthToken structs.
        let bytes = fs::read_to_string(&path)?;
        assert!(
            bytes.contains("\"type\""),
            "expected new format, got: {bytes}"
        );
        Ok(())
    }

    #[test]
    fn remove_clears_empty_provider_entry() -> TestResult {
        let dir = tmp_dir()?;
        let path = dir.join("oauth-tokens.json");
        let mut store = CredentialStore::load(&path)?;
        store.set("anthropic", "only", Credential::api_key("k"))?;
        store.remove("anthropic", "only")?;
        assert!(store.providers().is_empty());
        Ok(())
    }

    #[test]
    fn remove_all_for_drops_every_label() -> TestResult {
        let dir = tmp_dir()?;
        let mut store = CredentialStore::load(dir.join("oauth-tokens.json"))?;
        store.set("anthropic", "a", Credential::api_key("1"))?;
        store.set("anthropic", "b", Credential::api_key("2"))?;
        let n = store.remove_all_for("anthropic")?;
        assert_eq!(n, 2);
        assert!(store.labels("anthropic").is_empty());
        Ok(())
    }

    #[test]
    fn missing_file_loads_empty() -> TestResult {
        let dir = tmp_dir()?;
        let store = CredentialStore::load(dir.join("never-written.json"))?;
        assert!(store.providers().is_empty());
        Ok(())
    }

    #[test]
    fn corrupt_file_errors() -> TestResult {
        let dir = tmp_dir()?;
        let path = dir.join("oauth-tokens.json");
        fs::write(&path, b"not json")?;
        let err = CredentialStore::load(&path)
            .err()
            .ok_or("expected failure")?;
        assert!(matches!(err, CredentialStoreError::Json(_)));
        Ok(())
    }

    #[test]
    fn debug_redacts_oauth_and_api_key() -> TestResult {
        let oauth = Credential::from_oauth_token(OAuthToken {
            access_token: "very-secret-token".into(),
            expires_at: 1700000000,
            refresh_token: Some("also-secret".into()),
        });
        let api_key = Credential::api_key("sk-ant-api03-very-secret");
        let oauth_dbg = format!("{oauth:?}");
        let api_dbg = format!("{api_key:?}");
        assert!(!oauth_dbg.contains("very-secret-token"));
        assert!(!oauth_dbg.contains("also-secret"));
        assert!(!api_dbg.contains("very-secret"));
        assert!(oauth_dbg.contains("<redacted>"));
        assert!(api_dbg.contains("<redacted>"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn file_perms_are_0600_on_unix() -> TestResult {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir()?;
        let path = dir.join("oauth-tokens.json");
        let mut store = CredentialStore::load(&path)?;
        store.set("anthropic", DEFAULT_ACCOUNT, Credential::api_key("x"))?;
        let meta = fs::metadata(&path)?;
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
        Ok(())
    }
}
