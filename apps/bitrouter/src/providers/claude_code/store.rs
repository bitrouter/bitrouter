//! Selected marker and live CLI source are held under one composite transaction.
//! This coordinates BitRouter callers in process; the external CLI must provide
//! its own compatible process lock to exclude simultaneous rotation/writes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use bitrouter_ai::auth::credentials::Credential;
use bitrouter_ai::auth::store::{
    CredentialKey, CredentialStore, CredentialTransaction, Durability, StoreError,
};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::providers::import::claude_code::{ClaudeCodeStore, LiveCredential};
use bitrouter_ai::auth::file::backend::FileCredentialStore;
use bitrouter_ai::auth::file::snapshot::CredentialStore as FileStore;

type LiveStores = HashMap<(Option<&'static str>, PathBuf), Arc<AsyncMutex<LiveState>>>;
static LIVE_STORES: OnceLock<Mutex<LiveStores>> = OnceLock::new();
#[derive(Default)]
struct LiveState {
    pending: Option<Pending>,
}
struct Pending {
    original: LiveCredential,
    replacement: Credential,
}

/// Application credential backend that resolves selected CLI-adoption markers.
/// Coordination and pending rotations are shared in this process only.
pub struct ClaudeStore {
    path: PathBuf,
    marker: FileCredentialStore,
    live: Option<ClaudeCodeStore>,
}
impl ClaudeStore {
    /// Bind an explicit marker file and permitted live CLI source.
    pub fn new(path: PathBuf, live: Option<ClaudeCodeStore>) -> Result<Self, StoreError> {
        let marker = FileCredentialStore::new(&path)?;
        let path = marker.path().to_path_buf();
        Ok(Self { marker, path, live })
    }
}

struct LiveTransaction {
    // Holding the marker lease also serializes ordinary stored-token callers.
    _marker: Box<dyn CredentialTransaction>,
    path: PathBuf,
    key: CredentialKey,
    live: ClaudeCodeStore,
    observed: Credential,
    original: LiveCredential,
    state: OwnedMutexGuard<LiveState>,
}

#[async_trait]
impl CredentialStore for ClaudeStore {
    async fn begin(
        &self,
        key: &CredentialKey,
    ) -> Result<Box<dyn CredentialTransaction>, StoreError> {
        let marker = self.marker.begin(key).await?;
        if !matches!(marker.credential(), Some(Credential::ClaudeCodeCli)) {
            return Ok(marker);
        }
        let Some(live) = self.live.as_ref() else {
            return Ok(marker);
        };
        let gate = LIVE_STORES
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| StoreError::Unavailable)?
            .entry(live.coordination_identity())
            .or_default()
            .clone();
        let mut state = gate.lock_owned().await;
        let current = live.read().map_err(|_| StoreError::Unavailable)?;
        if state.pending.as_ref().is_some_and(|pending| {
            current.as_ref().is_none_or(|current| {
                current.source != pending.original.source
                    || (current.token != pending.original.token
                        && Some(&pending.replacement)
                            != Some(&Credential::Oauth(current.token.clone())))
            })
        }) {
            state.pending = None;
            return Err(StoreError::Conflict);
        }
        let Some(current) = current else {
            return Ok(marker);
        };
        Ok(Box::new(LiveTransaction {
            _marker: marker,
            path: self.path.clone(),
            key: key.clone(),
            live: live.clone(),
            observed: Credential::Oauth(current.token.clone()),
            original: current,
            state,
        }))
    }
}

#[async_trait]
impl CredentialTransaction for LiveTransaction {
    fn credential(&self) -> Option<&Credential> {
        Some(&self.observed)
    }
    fn pending(&self) -> Option<&Credential> {
        self.state.pending.as_ref().map(|p| &p.replacement)
    }
    fn stage(&mut self, credential: Credential) {
        self.state.pending = Some(Pending {
            original: self.original.clone(),
            replacement: credential,
        });
    }
    async fn commit(&mut self) -> Result<Durability, StoreError> {
        let Some(pending) = self.state.pending.as_ref() else {
            return Ok(Durability::Persistent);
        };
        let marker = FileStore::load(&self.path).map_err(|_| StoreError::Unavailable)?;
        if marker.get_any(&self.key.provider, &self.key.account) != Some(&Credential::ClaudeCodeCli)
        {
            // Removing this alias does not delete the external CLI account.
            // A still-authorized adoption can finish this pending rotation.
            return Err(StoreError::Conflict);
        }
        let token = pending.replacement.as_oauth().ok_or(StoreError::Conflict)?;
        match self.live.write_back_checked(token, &pending.original) {
            Ok(()) => {
                self.observed = pending.replacement.clone();
                self.original.token = token.clone();
                self.state.pending = None;
                Ok(Durability::Persistent)
            }
            Err(StoreError::Conflict) => {
                self.state.pending = None;
                Err(StoreError::Conflict)
            }
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_ai::auth::credentials::OAuthToken;
    type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
    fn key(account: &str) -> CredentialKey {
        CredentialKey {
            provider: "claude-code".into(),
            account: account.into(),
        }
    }
    fn replacement() -> Credential {
        Credential::Oauth(OAuthToken {
            access_token: "rotated-access".into(),
            expires_at: 0,
            refresh_token: Some("rotated-refresh".into()),
        })
    }
    fn seed(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        std::fs::write(path, br#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"old-refresh","expiresAt":1000,"scopes":["inference"]},"unrelated":42}"#)?;
        Ok(())
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn retargeted_marker_alias_cannot_authorize_a_revoked_live_rotation() -> TestResult {
        let directory = tempfile::tempdir()?;
        let original = directory.path().join("original.json");
        let other = directory.path().join("other.json");
        let alias = directory.path().join("alias.json");
        let cli = directory.path().join("cli.json");
        seed(&cli)?;
        FileStore::load(&original)?.set("claude-code", "first", Credential::ClaudeCodeCli)?;
        FileStore::load(&other)?.set("claude-code", "first", Credential::ClaudeCodeCli)?;
        std::os::unix::fs::symlink(&original, &alias)?;
        let backend = ClaudeStore::new(alias.clone(), Some(ClaudeCodeStore::file_only(&cli)))?;
        let mut transaction = backend.begin(&key("first")).await?;
        transaction.stage(replacement());
        FileStore::load(&original)?.remove("claude-code", "first")?;
        std::fs::remove_file(&alias)?;
        std::os::unix::fs::symlink(&other, &alias)?;
        assert_eq!(transaction.commit().await, Err(StoreError::Conflict));
        assert_eq!(
            ClaudeCodeStore::file_only(&cli)
                .read()?
                .ok_or("missing CLI state")?
                .token
                .access_token,
            "old"
        );
        Ok(())
    }
    #[tokio::test]
    async fn live_commit_failure_retains_rotation_across_aliases_and_preserves_marker() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("adoptions.json");
        let cli = directory.path().join("cli.json");
        seed(&cli)?;
        let mut file = FileStore::load(&path)?;
        file.set("claude-code", "first", Credential::ClaudeCodeCli)?;
        file.set("claude-code", "second", Credential::ClaudeCodeCli)?;
        let backend = ClaudeStore::new(path.clone(), Some(ClaudeCodeStore::file_only(&cli)))?;
        let mut transaction = backend.begin(&key("first")).await?;
        transaction.stage(replacement());
        let blocked = directory.path().join("cli.json.tmp");
        std::fs::create_dir(&blocked)?;
        assert_eq!(transaction.commit().await, Err(StoreError::Unavailable));
        drop(transaction);
        std::fs::remove_dir(&blocked)?;
        let another = ClaudeStore::new(path.clone(), Some(ClaudeCodeStore::file_only(&cli)))?;
        let mut transaction = another.begin(&key("second")).await?;
        assert_eq!(transaction.pending(), Some(&replacement()));
        assert_eq!(transaction.commit().await?, Durability::Persistent);
        let stored = ClaudeCodeStore::file_only(&cli)
            .read()?
            .ok_or("missing CLI state")?;
        assert_eq!(
            stored.token,
            replacement().as_oauth().ok_or("wrong token type")?.clone()
        );
        let body: serde_json::Value = serde_json::from_slice(&std::fs::read(&cli)?)?;
        assert_eq!(body["unrelated"], 42);
        assert_eq!(body["claudeAiOauth"]["scopes"][0], "inference");
        assert_eq!(
            FileStore::load(path)?.get_any("claude-code", "first"),
            Some(&Credential::ClaudeCodeCli)
        );
        Ok(())
    }
    #[tokio::test]
    async fn live_login_replacement_and_logout_reject_old_rotation() -> TestResult {
        for deleted in [false, true] {
            let directory = tempfile::tempdir()?;
            let path = directory.path().join("adoptions.json");
            let cli = directory.path().join("cli.json");
            seed(&cli)?;
            let mut file = FileStore::load(&path)?;
            file.set("claude-code", "first", Credential::ClaudeCodeCli)?;
            let backend = ClaudeStore::new(path, Some(ClaudeCodeStore::file_only(&cli)))?;
            let mut transaction = backend.begin(&key("first")).await?;
            transaction.stage(replacement());
            if deleted {
                std::fs::remove_file(&cli)?;
            } else {
                std::fs::write(&cli, br#"{"claudeAiOauth":{"accessToken":"new-login"}}"#)?;
            }
            assert_eq!(transaction.commit().await, Err(StoreError::Conflict));
            assert!(transaction.pending().is_none());
            if !deleted {
                assert_eq!(
                    ClaudeCodeStore::file_only(&cli)
                        .read()?
                        .ok_or("missing login")?
                        .token
                        .access_token,
                    "new-login"
                );
            }
        }
        Ok(())
    }
    #[tokio::test]
    async fn detached_alias_never_reappears_and_shared_cli_rotation_stays_pending() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("adoptions.json");
        let cli = directory.path().join("cli.json");
        seed(&cli)?;
        let mut file = FileStore::load(&path)?;
        file.set("claude-code", "first", Credential::ClaudeCodeCli)?;
        file.set("claude-code", "second", Credential::ClaudeCodeCli)?;
        let backend = ClaudeStore::new(path.clone(), Some(ClaudeCodeStore::file_only(&cli)))?;
        let mut transaction = backend.begin(&key("first")).await?;
        transaction.stage(replacement());
        file.remove("claude-code", "first")?;
        assert_eq!(transaction.commit().await, Err(StoreError::Conflict));
        assert!(transaction.pending().is_some());
        drop(transaction);
        let mut transaction = backend.begin(&key("second")).await?;
        transaction.commit().await?;
        assert!(
            FileStore::load(path)?
                .get_any("claude-code", "first")
                .is_none()
        );
        assert_eq!(
            ClaudeCodeStore::file_only(cli)
                .read()?
                .ok_or("missing CLI state")?
                .token,
            replacement().as_oauth().ok_or("wrong token")?.clone()
        );
        Ok(())
    }
}
