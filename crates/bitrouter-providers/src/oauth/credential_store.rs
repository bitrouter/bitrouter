//! Persistent credential store — one JSON file under the bitrouter data
//! directory, keyed by `(provider_id, label)`.
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
//! - default path is `$XDG_DATA_HOME/bitrouter/oauth-tokens.json` (with
//!   `~/.local/share/bitrouter/...` and `%LOCALAPPDATA%\bitrouter\data\...`
//!   fallbacks). Follows the XDG Base Directory spec
//!   (<https://specifications.freedesktop.org/basedir-spec/latest/>). The
//!   filename is preserved across the format change so existing copilot
//!   logins survive a daemon upgrade with no extra steps.
//! - file permissions are 0600 on Unix — these credentials grant access to
//!   the user's upstream account; a co-tenant on the box must not be able
//!   to read them.
//! - writes are atomic-renamed from a sibling `.tmp` file, so a crash
//!   mid-write can't truncate the store.

use std::collections::{BTreeMap, HashMap};
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

/// One stored credential — either a static API key or an OAuth credential.
///
/// Adjacently tagged (`{ "type": …, "data": { … } }`) so the OAuth variant
/// can wrap the existing [`OAuthToken`] as-is without duplicating its fields.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Credential {
    /// A static API key — what `bitrouter login <provider>` stores when the
    /// user picks "paste an API key" instead of a browser OAuth flow.
    /// Treated as never-expiring.
    ApiKey {
        /// The plaintext key value (e.g. `sk-ant-api03-…`, `sk-…`).
        value: String,
    },
    /// An OAuth credential plus optional refresh metadata.
    Oauth(OAuthToken),
    /// A tokenless marker meaning "resolve the credential live from the Claude
    /// Code CLI's own store (`~/.claude`) at request time, and write any
    /// refresh back there". No token is copied into this store, so bitrouter
    /// and Claude Code share one credential and can't refresh-rotate each other
    /// out (RFC 6749 §6). Set by `bitrouter login anthropic`; consumed by the
    /// Anthropic `AuthApplier`. Serialized as the unit-variant `{"type":
    /// "claude_code_cli"}`.
    ClaudeCodeCli,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credential::ApiKey { value } => f
                .debug_struct("ApiKey")
                .field(
                    "value",
                    &if value.is_empty() {
                        "<empty>"
                    } else {
                        "<redacted>"
                    },
                )
                .finish(),
            Credential::Oauth(token) => f.debug_tuple("Oauth").field(token).finish(),
            Credential::ClaudeCodeCli => f.write_str("ClaudeCodeCli"),
        }
    }
}

impl Credential {
    /// Whether this credential is currently usable. API keys are always
    /// considered valid; OAuth credentials defer to [`OAuthToken::is_valid`].
    pub fn is_valid(&self) -> bool {
        match self {
            Credential::ApiKey { .. } => true,
            Credential::Oauth(t) => t.is_valid(),
            // Liveness is resolved at request time from the Claude Code store;
            // the marker is never itself "expired".
            Credential::ClaudeCodeCli => true,
        }
    }

    /// Build an OAuth credential from an [`OAuthToken`].
    pub fn from_oauth_token(token: OAuthToken) -> Self {
        Credential::Oauth(token)
    }

    /// Build an API-key credential.
    pub fn api_key(value: impl Into<String>) -> Self {
        Credential::ApiKey {
            value: value.into(),
        }
    }

    /// Borrow the inner OAuth token, if this is the OAuth variant.
    pub fn as_oauth(&self) -> Option<&OAuthToken> {
        match self {
            Credential::Oauth(t) => Some(t),
            Credential::ApiKey { .. } | Credential::ClaudeCodeCli => None,
        }
    }

    /// Borrow the API-key value, if this is the API-key variant.
    pub fn as_api_key(&self) -> Option<&str> {
        match self {
            Credential::ApiKey { value } => Some(value.as_str()),
            Credential::Oauth(_) | Credential::ClaudeCodeCli => None,
        }
    }

    /// Short, log-safe description of the credential kind. Used by CLI
    /// messages — never includes the credential itself.
    pub fn kind_label(&self) -> &'static str {
        match self {
            Credential::ApiKey { .. } => "API key",
            Credential::Oauth(_) => "OAuth",
            Credential::ClaudeCodeCli => "Claude Code session",
        }
    }
}

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
    /// No home / data directory could be resolved.
    #[error("could not resolve a data directory for the credential store")]
    NoDataDir,
}

/// Persistent credential store backed by a single JSON file. See the module
/// docs for the on-disk layout and migration behaviour.
#[derive(Debug)]
pub struct CredentialStore {
    path: PathBuf,
    /// `provider_id -> label -> Credential`. `BTreeMap` for the inner map so
    /// the serialised label order is deterministic (helps human-diffing
    /// the file).
    creds: HashMap<String, BTreeMap<String, Credential>>,
}

/// Default filename inside the bitrouter data directory. Preserved from the
/// pre-credential-store TokenStore so existing copilot logins survive a
/// daemon upgrade without manual migration.
pub const DEFAULT_FILENAME: &str = "oauth-tokens.json";

/// Label used when a provider stores a single credential and the caller
/// hasn't picked one explicitly.
pub const DEFAULT_LABEL: &str = "default";

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
    /// [`DEFAULT_LABEL`].
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
        let path = path.into();
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
                    m.insert(DEFAULT_LABEL.to_string(), Credential::Oauth(token));
                    (id, m)
                })
                .collect(),
        };
        Ok(Self { path, creds })
    }

    /// Resolve the default store path under `$XDG_DATA_HOME/bitrouter/`.
    pub fn default_path() -> Result<Self, CredentialStoreError> {
        let dir = default_data_dir()?;
        Self::load(dir.join(DEFAULT_FILENAME))
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
        self.creds
            .entry(provider_id.to_string())
            .or_default()
            .insert(label.to_string(), credential);
        self.flush()
    }

    /// Remove `(provider_id, label)`'s credential and persist. Returns the
    /// removed credential if any was present.
    pub fn remove(
        &mut self,
        provider_id: &str,
        label: &str,
    ) -> Result<Option<Credential>, CredentialStoreError> {
        let removed = self
            .creds
            .get_mut(provider_id)
            .and_then(|m| m.remove(label));
        // Clean up the provider entry if no labels remain.
        if let Some(m) = self.creds.get(provider_id)
            && m.is_empty()
        {
            self.creds.remove(provider_id);
        }
        if removed.is_some() {
            self.flush()?;
        }
        Ok(removed)
    }

    /// Remove every credential for `provider_id`. Returns the number of
    /// credentials that were removed.
    pub fn remove_all_for(&mut self, provider_id: &str) -> Result<usize, CredentialStoreError> {
        let removed = self.creds.remove(provider_id).map(|m| m.len()).unwrap_or(0);
        if removed > 0 {
            self.flush()?;
        }
        Ok(removed)
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
        let parent = self.path.parent().ok_or(CredentialStoreError::NoDataDir)?;
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
                .map_err(|source| CredentialStoreError::Io {
                    path: tmp.clone(),
                    source,
                })?;
        }
        #[cfg(not(unix))]
        {
            fs::write(&tmp, &bytes).map_err(|source| CredentialStoreError::Io {
                path: tmp.clone(),
                source,
            })?;
        }
        fs::rename(&tmp, &self.path).map_err(|source| CredentialStoreError::Io {
            path: self.path.clone(),
            source,
        })?;
        Ok(())
    }
}

/// Resolve the bitrouter data directory. XDG Base Directory on Unix,
/// `%LOCALAPPDATA%\bitrouter\data` on Windows.
fn default_data_dir() -> Result<PathBuf, CredentialStoreError> {
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
    Err(CredentialStoreError::NoDataDir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-credential-store-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trip_oauth() {
        let dir = tmp_dir();
        let path = dir.join("oauth-tokens.json");
        {
            let mut store = CredentialStore::load(&path).unwrap();
            assert!(store.get("anthropic", DEFAULT_LABEL).is_none());
            store
                .set(
                    "anthropic",
                    DEFAULT_LABEL,
                    Credential::from_oauth_token(OAuthToken {
                        access_token: "sk-ant-oat-test".into(),
                        expires_at: 0,
                        refresh_token: Some("refresh".into()),
                    }),
                )
                .unwrap();
        }
        let reloaded = CredentialStore::load(&path).unwrap();
        let got = reloaded.get("anthropic", DEFAULT_LABEL).unwrap();
        let oauth = got.as_oauth().unwrap();
        assert_eq!(oauth.access_token, "sk-ant-oat-test");
        assert_eq!(oauth.refresh_token.as_deref(), Some("refresh"));
    }

    #[test]
    fn round_trip_api_key() {
        let dir = tmp_dir();
        let path = dir.join("oauth-tokens.json");
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set(
                    "anthropic",
                    "work",
                    Credential::api_key("sk-ant-api03-secret"),
                )
                .unwrap();
        }
        let reloaded = CredentialStore::load(&path).unwrap();
        let got = reloaded.get("anthropic", "work").unwrap();
        assert_eq!(got.as_api_key(), Some("sk-ant-api03-secret"));
    }

    #[test]
    fn round_trip_claude_code_cli_marker() {
        let dir = tmp_dir();
        let path = dir.join("oauth-tokens.json");
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set("anthropic", DEFAULT_LABEL, Credential::ClaudeCodeCli)
                .unwrap();
        }
        let reloaded = CredentialStore::load(&path).unwrap();
        let got = reloaded.get_any("anthropic", DEFAULT_LABEL).unwrap();
        assert!(matches!(got, Credential::ClaudeCodeCli));
        assert_eq!(got.kind_label(), "Claude Code session");
        assert!(got.as_oauth().is_none());
        assert!(got.as_api_key().is_none());
        // The marker itself is always "valid"; whether a live session exists is
        // decided at request time by the applier.
        assert!(got.is_valid());
        assert!(reloaded.get("anthropic", DEFAULT_LABEL).is_some());
        // Serialized as the adjacently-tagged unit variant, no `data` payload.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("claude_code_cli"), "got: {raw}");
    }

    #[test]
    fn multiple_labels_per_provider() {
        let dir = tmp_dir();
        let mut store = CredentialStore::load(dir.join("oauth-tokens.json")).unwrap();
        store
            .set(
                "anthropic",
                "pro-max",
                Credential::from_oauth_token(OAuthToken {
                    access_token: "sk-ant-oat-1".into(),
                    expires_at: 0,
                    refresh_token: None,
                }),
            )
            .unwrap();
        store
            .set(
                "anthropic",
                "work-key",
                Credential::api_key("sk-ant-api03-2"),
            )
            .unwrap();
        let mut labels = store.labels("anthropic");
        labels.sort();
        assert_eq!(labels, vec!["pro-max", "work-key"]);
        assert!(store.get("anthropic", "pro-max").is_some());
        assert!(store.get("anthropic", "work-key").is_some());
    }

    #[test]
    fn expired_oauth_returns_none_from_get_but_some_from_get_any() {
        let dir = tmp_dir();
        let mut store = CredentialStore::load(dir.join("oauth-tokens.json")).unwrap();
        store
            .set(
                "anthropic",
                DEFAULT_LABEL,
                Credential::from_oauth_token(OAuthToken {
                    access_token: "x".into(),
                    expires_at: 1, // far past
                    refresh_token: Some("r".into()),
                }),
            )
            .unwrap();
        assert!(store.get("anthropic", DEFAULT_LABEL).is_none());
        assert!(store.get_any("anthropic", DEFAULT_LABEL).is_some());
    }

    #[test]
    fn api_key_never_expires() {
        let dir = tmp_dir();
        let mut store = CredentialStore::load(dir.join("oauth-tokens.json")).unwrap();
        store
            .set("anthropic", "k", Credential::api_key("static"))
            .unwrap();
        assert!(store.get("anthropic", "k").is_some());
    }

    #[test]
    fn legacy_flat_format_migrates_to_default_label() {
        let dir = tmp_dir();
        let path = dir.join("oauth-tokens.json");
        // Write the legacy flat layout — what the pre-pkce TokenStore wrote.
        let legacy_json = r#"{
          "github-copilot": { "access_token": "ghu_legacy_test", "expires_at": 0 }
        }"#;
        fs::write(&path, legacy_json).unwrap();
        let store = CredentialStore::load(&path).unwrap();
        let got = store.get("github-copilot", DEFAULT_LABEL).unwrap();
        let oauth = got.as_oauth().unwrap();
        assert_eq!(oauth.access_token, "ghu_legacy_test");
    }

    #[test]
    fn legacy_migration_persists_in_new_format_on_next_write() {
        let dir = tmp_dir();
        let path = dir.join("oauth-tokens.json");
        // Seed legacy.
        let legacy_json = r#"{
          "github-copilot": { "access_token": "ghu_legacy", "expires_at": 0 }
        }"#;
        fs::write(&path, legacy_json).unwrap();
        let mut store = CredentialStore::load(&path).unwrap();
        // Mutate to force a write — adding a new credential is enough.
        store
            .set("anthropic", DEFAULT_LABEL, Credential::api_key("k"))
            .unwrap();
        // On-disk file should now be the new labeled format. Re-load and
        // confirm both entries are addressable via the new API.
        let reloaded = CredentialStore::load(&path).unwrap();
        assert!(reloaded.get("github-copilot", DEFAULT_LABEL).is_some());
        assert!(reloaded.get("anthropic", DEFAULT_LABEL).is_some());
        // The raw bytes should now carry the new "type"/"data" tagging,
        // not legacy flat OAuthToken structs.
        let bytes = fs::read_to_string(&path).unwrap();
        assert!(
            bytes.contains("\"type\""),
            "expected new format, got: {bytes}"
        );
    }

    #[test]
    fn remove_clears_empty_provider_entry() {
        let dir = tmp_dir();
        let path = dir.join("oauth-tokens.json");
        let mut store = CredentialStore::load(&path).unwrap();
        store
            .set("anthropic", "only", Credential::api_key("k"))
            .unwrap();
        store.remove("anthropic", "only").unwrap();
        assert!(store.providers().is_empty());
    }

    #[test]
    fn remove_all_for_drops_every_label() {
        let dir = tmp_dir();
        let mut store = CredentialStore::load(dir.join("oauth-tokens.json")).unwrap();
        store
            .set("anthropic", "a", Credential::api_key("1"))
            .unwrap();
        store
            .set("anthropic", "b", Credential::api_key("2"))
            .unwrap();
        let n = store.remove_all_for("anthropic").unwrap();
        assert_eq!(n, 2);
        assert!(store.labels("anthropic").is_empty());
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = tmp_dir();
        let store = CredentialStore::load(dir.join("never-written.json")).unwrap();
        assert!(store.providers().is_empty());
    }

    #[test]
    fn corrupt_file_errors() {
        let dir = tmp_dir();
        let path = dir.join("oauth-tokens.json");
        fs::write(&path, b"not json").unwrap();
        let err = CredentialStore::load(&path).unwrap_err();
        assert!(matches!(err, CredentialStoreError::Json(_)));
    }

    #[test]
    fn debug_redacts_oauth_and_api_key() {
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
    }

    #[cfg(unix)]
    #[test]
    fn file_perms_are_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir();
        let path = dir.join("oauth-tokens.json");
        let mut store = CredentialStore::load(&path).unwrap();
        store
            .set("anthropic", DEFAULT_LABEL, Credential::api_key("x"))
            .unwrap();
        let meta = fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
    }
}
