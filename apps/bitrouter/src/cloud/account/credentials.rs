//! On-disk credentials for `bro cloud` authentication.
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
use std::sync::Mutex;

use anyhow::{Context, Result};
use bitrouter_ai::providers::hosted::credentials::StoredCredential;

use bitrouter_ai::auth::store::StoreError;

static WRITES: Mutex<()> = Mutex::new(());

/// File-backed credentials store. Single-credential — there is one
/// "current account" per bitrouter install. Multi-account support could
/// layer on top later; not in scope for v1.
#[derive(Debug)]
pub struct CredentialsStore {
    path: PathBuf,
    current: Option<StoredCredential>,
}

/// Default filename inside the bitrouter data directory.
pub const DEFAULT_FILENAME: &str = "account-credentials.json";

impl CredentialsStore {
    /// Load the store from `path`. Missing file → empty store.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = resolved_path(&path.into())?;
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
    /// refresh — request-time resolution belongs to [`super::manager::CredentialManager`].
    pub fn current(&self) -> Option<&StoredCredential> {
        self.current.as_ref()
    }

    /// Persist a login atomically. Writers in this process share a file lock.
    pub fn save(&mut self, credential: impl Into<StoredCredential>) -> Result<()> {
        let _write = WRITES
            .lock()
            .map_err(|_| anyhow::anyhow!("credential write lock unavailable"))?;
        let credential = credential.into();
        self.persist(&credential)?;
        self.current = Some(credential);
        Ok(())
    }

    fn persist(&self, credential: &StoredCredential) -> Result<()> {
        use std::io::Write as _;
        let bytes =
            serde_json::to_vec_pretty(credential).context("serialising credentials to JSON")?;
        let parent = self
            .path
            .parent()
            .context("credentials file has no parent directory")?;
        fs::create_dir_all(parent).context("creating credentials directory")?;
        let tmp = self.path.with_extension("json.tmp");
        match fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("removing stale credential temporary file"),
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp)
            .context("creating credential temporary file")?;
        file.write_all(&bytes)
            .context("writing credential temporary file")?;
        file.sync_all()
            .context("syncing credential temporary file")?;
        fs::rename(&tmp, &self.path).context("replacing credential file")?;
        sync_parent(&self.path)?;
        Ok(())
    }

    /// Remove the current persisted login. Reload under the write lock so a stale
    /// store returns the credential actually removed. Failed removal retains cache.
    pub fn clear(&mut self) -> Result<Option<StoredCredential>> {
        let _write = WRITES
            .lock()
            .map_err(|_| anyhow::anyhow!("credential write lock unavailable"))?;
        let prior = Self::load(&self.path)?.current;
        match fs::remove_file(&self.path) {
            Ok(()) => sync_parent(&self.path)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("removing credential file"),
        }
        self.current = None;
        Ok(prior)
    }

    pub(crate) fn compare_exchange(
        &mut self,
        expected: Option<&StoredCredential>,
        replacement: &StoredCredential,
    ) -> std::result::Result<(), StoreError> {
        let _write = WRITES.lock().map_err(|_| StoreError::Unavailable)?;
        let observed = Self::load(&self.path)
            .map_err(|_| StoreError::Unavailable)?
            .current;
        if observed.as_ref() == Some(replacement) {
            // A prior rename may have succeeded before directory sync failed.
            sync_parent(&self.path).map_err(|_| StoreError::Unavailable)?;
        } else {
            if observed.as_ref() != expected {
                return Err(StoreError::Conflict);
            }
            self.persist(replacement)
                .map_err(|_| StoreError::Unavailable)?;
        }
        self.current = Some(replacement.clone());
        Ok(())
    }
}

pub(crate) fn resolved_path(input: &Path) -> Result<PathBuf> {
    let absolute = if input.is_absolute() {
        input.to_path_buf()
    } else {
        std::env::current_dir()?.join(input)
    };
    // Resolve original filesystem semantics, including symlink/.., first.
    if let Ok(path) = fs::canonicalize(&absolute) {
        return Ok(path);
    }
    match (
        absolute
            .parent()
            .and_then(|parent| fs::canonicalize(parent).ok()),
        absolute.file_name(),
    ) {
        (Some(parent), Some(name)) => Ok(parent.join(name)),
        _ => Ok(absolute),
    }
}

fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(path.parent().context("credential file has no parent")?)?
        .sync_all()
        .context("syncing credential directory")?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Resolve the default credentials file path. Same XDG / `%LOCALAPPDATA%`
/// rules as the upstream-provider token store —
/// the application ordinary-provider location policy in `provider_credentials`.
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
    use bitrouter_ai::providers::hosted::credentials::{
        CredentialKind, Credentials, REFRESH_WINDOW,
    };
    use chrono::{Duration, Utc};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp_dir(label: &str) -> Result<PathBuf> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-account-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir)?;
        Ok(dir)
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
    fn round_trip_through_disk() -> Result<()> {
        let dir = tmp_dir("rt")?;
        let path = dir.join(DEFAULT_FILENAME);
        let creds = sample_credentials();
        {
            let mut store = CredentialsStore::load(&path)?;
            assert!(store.current().is_none());
            store.save(creds.clone())?;
        }
        let reloaded = CredentialsStore::load(&path)?;
        let got = reloaded
            .current()
            .and_then(StoredCredential::oauth)
            .ok_or_else(|| anyhow::anyhow!("OAuth credential was not persisted"))?;
        assert_eq!(got.access_token, creds.access_token);
        assert_eq!(got.refresh_token, creds.refresh_token);
        assert_eq!(got.scope, creds.scope);
        assert_eq!(got.client_id, creds.client_id);
        assert_eq!(got.authorization_server, creds.authorization_server);
        assert_eq!(got.namespace_id, creds.namespace_id);
        assert_eq!(got.subject, creds.subject);
        Ok(())
    }

    #[test]
    fn clear_removes_file_and_returns_prior() -> Result<()> {
        let dir = tmp_dir("clear")?;
        let path = dir.join(DEFAULT_FILENAME);
        let mut store = CredentialsStore::load(&path)?;
        store.save(sample_credentials())?;
        assert!(path.exists());
        let prior = store
            .clear()?
            .ok_or_else(|| anyhow::anyhow!("stored credential was unexpectedly absent"))?;
        let oauth = prior
            .oauth()
            .ok_or_else(|| anyhow::anyhow!("stored credential was unexpectedly an API key"))?;
        assert_eq!(oauth.access_token, "AT");
        assert!(!path.exists());
        // Second clear is a no-op.
        assert!(store.clear()?.is_none());
        Ok(())
    }

    #[test]
    fn missing_file_loads_empty() -> Result<()> {
        let dir = tmp_dir("missing")?;
        let store = CredentialsStore::load(dir.join(DEFAULT_FILENAME))?;
        assert!(store.current().is_none());
        Ok(())
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
    fn tagged_api_key_round_trips_and_redacts() -> Result<()> {
        let path = tmp_dir("api-key")?.join(DEFAULT_FILENAME);
        let credential = StoredCredential::api_key(
            "brk_AAAAAAAAAAAAAAAA.secret-value".to_owned(),
            "https://api.bitrouter.ai".to_owned(),
        );
        let mut store = CredentialsStore::load(&path)?;
        store.save(credential)?;

        let reloaded = CredentialsStore::load(&path)?;
        let current = reloaded
            .current()
            .ok_or_else(|| anyhow::anyhow!("API key credential was not persisted"))?;
        assert_eq!(current.kind(), CredentialKind::ApiKey);
        assert_eq!(current.base_url(), "https://api.bitrouter.ai");
        let rendered = format!("{current:?}");
        assert!(!rendered.contains("secret-value"));
        assert!(rendered.contains("<redacted>"));
        Ok(())
    }

    #[test]
    fn legacy_untagged_oauth_file_still_loads() -> Result<()> {
        let path = tmp_dir("legacy")?.join(DEFAULT_FILENAME);
        fs::write(&path, serde_json::to_vec(&sample_credentials())?)?;

        let store = CredentialsStore::load(path)?;
        let current = store
            .current()
            .ok_or_else(|| anyhow::anyhow!("legacy OAuth credential did not load"))?;
        assert_eq!(current.kind(), CredentialKind::Oauth);
        let oauth = current
            .oauth()
            .ok_or_else(|| anyhow::anyhow!("legacy OAuth credential loaded as API key"))?;
        assert_eq!(oauth.access_token, "AT");
        Ok(())
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
    fn file_perms_are_0600_on_unix() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir("perms")?;
        let path = dir.join(DEFAULT_FILENAME);
        let mut store = CredentialsStore::load(&path)?;
        store.save(sample_credentials())?;
        let mode = fs::metadata(&path)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
        Ok(())
    }
    #[test]
    fn stale_clear_removes_and_returns_the_actual_login() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(DEFAULT_FILENAME);
        let mut stale = CredentialsStore::load(&path)?;
        let mut writer = CredentialsStore::load(&path)?;
        let replacement =
            StoredCredential::api_key("new-login".into(), "https://as.example.com".into());
        writer.save(replacement.clone())?;
        assert_eq!(stale.clear()?, Some(replacement));
        assert!(CredentialsStore::load(path)?.current().is_none());
        Ok(())
    }

    #[test]
    fn failed_writes_and_removal_leave_the_cached_snapshot_unchanged() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(DEFAULT_FILENAME);
        let old: StoredCredential = sample_credentials().into();
        let mut store = CredentialsStore::load(&path)?;
        store.save(old.clone())?;
        let temporary = path.with_extension("json.tmp");
        fs::create_dir(&temporary)?;
        assert!(
            store
                .save(StoredCredential::api_key(
                    "new".into(),
                    "https://as.example.com".into()
                ))
                .is_err()
        );
        assert_eq!(store.current(), Some(&old));
        fs::remove_file(&path)?;
        fs::create_dir(&path)?;
        assert!(store.clear().is_err());
        assert_eq!(store.current(), Some(&old));
        Ok(())
    }

    #[test]
    fn compare_exchange_acknowledges_a_previously_written_replacement() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(DEFAULT_FILENAME);
        let old: StoredCredential = sample_credentials().into();
        let replacement =
            StoredCredential::api_key("replacement".into(), "https://as.example.com".into());
        let mut store = CredentialsStore::load(&path)?;
        store.save(old.clone())?;
        store.save(replacement.clone())?;
        store.compare_exchange(Some(&old), &replacement)?;
        assert_eq!(store.current(), Some(&replacement));
        store.clear()?;
        assert_eq!(
            store.compare_exchange(Some(&old), &replacement),
            Err(StoreError::Conflict)
        );
        assert!(CredentialsStore::load(path)?.current().is_none());
        Ok(())
    }
}
