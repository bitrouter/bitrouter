//! On-disk OAuth credentials for the `bitrouter auth` user-account flow.
//!
//! Single JSON file at `<data-dir>/account-credentials.json`. The file
//! is owner-only (mode `0o600` on Unix) — these tokens grant access to
//! the user's account on the configured authorization server and a
//! co-tenant on the box must not be able to read them.
//!
//! Schema is intentionally explicit: every field a future caller might
//! need (token type, scope, AS URL, client id) is persisted alongside
//! the bearer so subsequent commands can sanity-check + auto-refresh
//! without depending on global state. Per RFC 9700 §2.4, the refresh
//! token expiry is captured separately so the store can refuse to
//! refresh once that window has elapsed.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::flow;
use super::metadata::AsMetadata;

/// One stored OAuth credential set.
///
/// `Debug` redacts `access_token` and `refresh_token` so a stray
/// `tracing::error!(?credentials, …)` can never dump the bearer to the
/// log stream.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// RFC 6749 §1.4 bearer token. Sent on outgoing requests as
    /// `Authorization: Bearer <access_token>` (RFC 6750 §2.1).
    pub access_token: String,
    /// RFC 6749 §1.5 refresh token. Optional — the AS may decline to
    /// issue one, or `bitrouter auth login` may have been run with a
    /// scope the AS refuses to refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Wall-clock UTC at which `access_token` becomes invalid.
    pub expires_at: DateTime<Utc>,
    /// Wall-clock UTC at which `refresh_token` itself becomes invalid.
    /// Optional — many AS deployments issue refresh tokens with no
    /// declared expiry; absent here means "treat the refresh token as
    /// valid until the AS itself rejects it".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token_expires_at: Option<DateTime<Utc>>,
    /// RFC 6749 §7.1 token type. Bitrouter only supports `Bearer` today.
    pub token_type: String,
    /// RFC 6749 §3.3 scope — the space-delimited list of scopes the AS
    /// granted (which may be narrower than what was requested).
    pub scope: String,
    /// The client id the device-flow was run as. Captured so a later
    /// `current_token` / refresh uses the same client id even if the
    /// env var has changed since.
    pub client_id: String,
    /// The AS base URL the device-flow was run against. Captured for the
    /// same reason as `client_id`.
    pub authorization_server: String,
    /// Namespace the credential is baked into. Every device-flow token
    /// the CLI obtains is namespace-baked, so this is normally `Some`.
    /// Absent for a namespace-null credential (the console web session,
    /// which the CLI never holds) — and for a credential file written
    /// before namespace-scoping shipped, where it signals "re-login to
    /// get a namespace-scoped token". Read once at client construction
    /// to resolve the implicit `{nsid}` in management calls; preserved
    /// verbatim across refreshes since rotation never rebinds the
    /// namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace_id: Option<String>,
    /// Subject identifier returned by the AS (typically `sub` from an
    /// OpenID Connect ID token). Optional — populated when the AS
    /// returned an `id_token` claim the flow could decode. Used by
    /// `whoami`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at", &self.expires_at)
            .field("refresh_token_expires_at", &self.refresh_token_expires_at)
            .field("token_type", &self.token_type)
            .field("scope", &self.scope)
            .field("client_id", &self.client_id)
            .field("authorization_server", &self.authorization_server)
            .field("namespace_id", &self.namespace_id)
            .field("subject", &self.subject)
            .finish()
    }
}

impl Credentials {
    /// Is `access_token` still within its TTL at the current wall clock?
    pub fn access_token_valid(&self) -> bool {
        Utc::now() < self.expires_at
    }

    /// Is the access token within `window` of expiring (or already
    /// expired)? Used by [`CredentialsStore::current_token`] to trigger
    /// a refresh slightly before the token actually becomes invalid.
    pub fn access_token_near_expiry(&self, window: Duration) -> bool {
        Utc::now() + window >= self.expires_at
    }

    /// Has the refresh token itself expired? `None` is treated as
    /// "valid forever" — many AS deployments omit `refresh_token_expires_in`.
    pub fn refresh_token_usable(&self) -> bool {
        match self.refresh_token_expires_at {
            Some(t) => Utc::now() < t,
            None => self.refresh_token.is_some(),
        }
    }
}

/// File-backed credentials store. Single-credential — there is one
/// "current account" per bitrouter install. Multi-account support could
/// layer on top later; not in scope for v1.
#[derive(Debug)]
pub struct CredentialsStore {
    path: PathBuf,
    current: Option<Credentials>,
}

/// Default filename inside the bitrouter data directory.
pub const DEFAULT_FILENAME: &str = "account-credentials.json";

impl CredentialsStore {
    /// Load the store from `path`. Missing file → empty store.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let current = match fs::read(&path) {
            Ok(bytes) => Some(
                serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing credentials file {}", path.display()))?,
            ),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", path.display()));
            }
        };
        Ok(Self { path, current })
    }

    /// Resolve the default credentials path under the bitrouter data
    /// directory and load.
    pub fn default_path() -> Result<Self> {
        let path = default_credentials_path()?;
        Self::load(path)
    }

    /// Path the store reads + writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The current stored credentials, if any. Does NOT trigger a
    /// refresh — see [`current_token`](Self::current_token).
    pub fn current(&self) -> Option<&Credentials> {
        self.current.as_ref()
    }

    /// Persist `credentials` to disk and update the in-memory cache.
    /// Atomically renames a sibling `.tmp` file so a crash mid-write
    /// can't truncate the credentials file.
    pub fn save(&mut self, credentials: Credentials) -> Result<()> {
        let bytes =
            serde_json::to_vec_pretty(&credentials).context("serialising credentials to JSON")?;
        let parent = self
            .path
            .parent()
            .context("credentials file has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("creating credentials dir {}", parent.display()))?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
        // chmod 0600 BEFORE the rename so the file is never world-readable
        // even for an instant. Cross-tenant token theft prevention is the
        // whole point.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            fs::set_permissions(&tmp, perms)
                .with_context(|| format!("chmod 0600 {}", tmp.display()))?;
        }
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), self.path.display()))?;
        self.current = Some(credentials);
        Ok(())
    }

    /// Remove the credentials file and clear the in-memory cache.
    /// Returns the prior credentials (if any) so the caller can attempt
    /// a best-effort revoke before they're discarded.
    pub fn clear(&mut self) -> Result<Option<Credentials>> {
        let prior = self.current.take();
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(prior),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(prior),
            Err(e) => Err(e).with_context(|| format!("removing {}", self.path.display())),
        }
    }

    /// Return a still-valid access token, transparently refreshing via
    /// RFC 6749 §6 when the stored access token is within
    /// `REFRESH_WINDOW` of expiry. The new (possibly rotated, per RFC
    /// 9700 §4.14) refresh token is persisted before the bearer is
    /// returned, so an interruption between refresh and use cannot
    /// strand the caller.
    ///
    /// Returns an error when:
    /// - no credentials are stored,
    /// - the access token is expired AND no refresh token is available,
    /// - the refresh token itself has expired,
    /// - the AS refresh exchange fails.
    pub async fn current_token(
        &mut self,
        client: &reqwest::Client,
        metadata: &AsMetadata,
    ) -> Result<String> {
        let creds = self
            .current
            .as_ref()
            .context("no stored credentials — run `bitrouter auth login` first")?
            .clone();
        if !creds.access_token_near_expiry(REFRESH_WINDOW) {
            return Ok(creds.access_token);
        }
        let refresh_token = creds.refresh_token.as_deref().context(
            "access token expired and no refresh token is stored — run `bitrouter auth login`",
        )?;
        if !creds.refresh_token_usable() {
            anyhow::bail!(
                "refresh token has itself expired — run `bitrouter auth login` to re-authenticate"
            );
        }
        let token_set = flow::refresh(
            client,
            &metadata.token_endpoint,
            &creds.client_id,
            refresh_token,
            Some(&creds.scope),
        )
        .await
        .context("refreshing OAuth access token")?;
        // RFC 9700 §4.14: the AS MAY rotate the refresh token; keep the
        // new one if returned, otherwise stick with the existing one
        // (RFC 6749 §6 keeps it valid until explicitly revoked).
        let refresh_token = token_set
            .refresh_token
            .or_else(|| creds.refresh_token.clone());
        let scope = token_set.scope.unwrap_or(creds.scope.clone());
        let refreshed = Credentials {
            access_token: token_set.access_token,
            refresh_token,
            expires_at: token_set.expires_at,
            refresh_token_expires_at: token_set
                .refresh_token_expires_at
                .or(creds.refresh_token_expires_at),
            token_type: token_set.token_type.unwrap_or(creds.token_type.clone()),
            scope,
            client_id: creds.client_id,
            authorization_server: creds.authorization_server,
            // Rotation never rebinds the namespace: the stored binding
            // wins. Preferring it (over the refresh response) means a
            // server that omits — or, worse, changes — the field on
            // refresh can't silently move the credential to a different
            // namespace. The token-set value is only a fallback for the
            // (not-expected) case where the stored binding was absent.
            namespace_id: creds.namespace_id.or(token_set.namespace_id),
            subject: creds.subject,
        };
        let bearer = refreshed.access_token.clone();
        self.save(refreshed)?;
        Ok(bearer)
    }
}

/// Refresh `current_token` slightly before the access token actually
/// expires so an in-flight request that *just* crossed the boundary
/// still succeeds. 60s matches the constraint in the task brief.
pub const REFRESH_WINDOW: Duration = Duration::seconds(60);

/// Resolve the default credentials file path. Same XDG / `%LOCALAPPDATA%`
/// rules as the upstream-provider token store —
/// `crates/bitrouter-providers/src/oauth/token_store.rs::default_data_dir`
/// — so the two stores live side by side under one bitrouter data dir.
pub fn default_credentials_path() -> Result<PathBuf> {
    Ok(default_data_dir()?.join(DEFAULT_FILENAME))
}

fn default_data_dir() -> Result<PathBuf> {
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
    anyhow::bail!(
        "could not resolve a data directory — set $XDG_DATA_HOME, $HOME, or %LOCALAPPDATA%"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-account-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_credentials() -> Credentials {
        Credentials {
            access_token: "AT".into(),
            refresh_token: Some("RT".into()),
            expires_at: Utc::now() + Duration::seconds(600),
            refresh_token_expires_at: None,
            token_type: "Bearer".into(),
            scope: "inference:invoke".into(),
            client_id: "cid".into(),
            authorization_server: "https://as.example.com".into(),
            namespace_id: Some("ns-1".into()),
            subject: Some("user-42".into()),
        }
    }

    #[test]
    fn round_trip_through_disk() {
        let dir = tmp_dir("rt");
        let path = dir.join(DEFAULT_FILENAME);
        let creds = sample_credentials();
        {
            let mut store = CredentialsStore::load(&path).unwrap();
            assert!(store.current().is_none());
            store.save(creds.clone()).unwrap();
        }
        let reloaded = CredentialsStore::load(&path).unwrap();
        let got = reloaded.current().unwrap();
        assert_eq!(got.access_token, creds.access_token);
        assert_eq!(got.refresh_token, creds.refresh_token);
        assert_eq!(got.scope, creds.scope);
        assert_eq!(got.client_id, creds.client_id);
        assert_eq!(got.authorization_server, creds.authorization_server);
        assert_eq!(got.namespace_id, creds.namespace_id);
        assert_eq!(got.subject, creds.subject);
    }

    #[test]
    fn clear_removes_file_and_returns_prior() {
        let dir = tmp_dir("clear");
        let path = dir.join(DEFAULT_FILENAME);
        let mut store = CredentialsStore::load(&path).unwrap();
        store.save(sample_credentials()).unwrap();
        assert!(path.exists());
        let prior = store.clear().unwrap().unwrap();
        assert_eq!(prior.access_token, "AT");
        assert!(!path.exists());
        // Second clear is a no-op.
        assert!(store.clear().unwrap().is_none());
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = tmp_dir("missing");
        let store = CredentialsStore::load(dir.join(DEFAULT_FILENAME)).unwrap();
        assert!(store.current().is_none());
    }

    #[test]
    fn debug_redacts_tokens() {
        let creds = sample_credentials();
        let rendered = format!("{creds:?}");
        assert!(!rendered.contains("AT"), "access token leaked: {rendered}");
        assert!(!rendered.contains("RT"), "refresh token leaked: {rendered}");
        assert!(rendered.contains("<redacted>"));
        // Non-secret fields are still visible.
        assert!(rendered.contains("user-42"));
        assert!(rendered.contains("https://as.example.com"));
    }

    #[test]
    fn near_expiry_detection() {
        let mut c = sample_credentials();
        c.expires_at = Utc::now() + Duration::seconds(30);
        assert!(c.access_token_near_expiry(REFRESH_WINDOW));
        c.expires_at = Utc::now() + Duration::seconds(600);
        assert!(!c.access_token_near_expiry(REFRESH_WINDOW));
    }

    #[test]
    fn refresh_usability_handles_missing_expiry() {
        let mut c = sample_credentials();
        c.refresh_token_expires_at = None;
        assert!(c.refresh_token_usable());
        c.refresh_token = None;
        assert!(!c.refresh_token_usable());
    }

    #[test]
    fn refresh_usability_respects_explicit_expiry() {
        let mut c = sample_credentials();
        c.refresh_token_expires_at = Some(Utc::now() - Duration::seconds(1));
        assert!(!c.refresh_token_usable());
        c.refresh_token_expires_at = Some(Utc::now() + Duration::seconds(60));
        assert!(c.refresh_token_usable());
    }

    #[cfg(unix)]
    #[test]
    fn file_perms_are_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir("perms");
        let path = dir.join(DEFAULT_FILENAME);
        let mut store = CredentialsStore::load(&path).unwrap();
        store.save(sample_credentials()).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
    }
}
