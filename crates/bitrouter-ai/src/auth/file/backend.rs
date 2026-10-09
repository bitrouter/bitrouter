//! Explicit file backend for AI account transactions.
//!
//! Leases and pending replacements are shared by canonical store path and account
//! within this process, including separate applier/backend instances. Ordinary
//! file-store writers are compared at commit. Cross-process read-modify-write
//! exclusion is not provided; the application must coordinate multiple processes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::auth::credentials::Credential;
use crate::auth::store::{
    CredentialKey, CredentialStore, CredentialTransaction, Durability, StoreError,
};
use async_trait::async_trait;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use super::snapshot::{CredentialStore as FileStore, CredentialStoreError};

type Accounts = HashMap<(PathBuf, CredentialKey), Arc<AsyncMutex<AccountState>>>;
static ACCOUNTS: OnceLock<Mutex<Accounts>> = OnceLock::new();

#[derive(Default)]
struct AccountState {
    pending: Option<Pending>,
}

struct Pending {
    expected: Option<Credential>,
    replacement: Credential,
}

/// Persistent backend at the exact path selected by the application.
pub struct FileCredentialStore {
    path: PathBuf,
}

impl FileCredentialStore {
    /// Bind a caller-selected path without reading ambient configuration.
    pub fn new(path: impl AsRef<Path>) -> std::result::Result<Self, StoreError> {
        let path = super::selected_path(path.as_ref()).map_err(|_| StoreError::Unavailable)?;
        Ok(Self { path })
    }

    /// Borrow the bound selected path for composite backends using this lease.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

struct FileTransaction {
    path: PathBuf,
    key: CredentialKey,
    observed: Option<Credential>,
    state: OwnedMutexGuard<AccountState>,
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn begin(
        &self,
        key: &CredentialKey,
    ) -> std::result::Result<Box<dyn CredentialTransaction>, StoreError> {
        let account = ACCOUNTS
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| StoreError::Unavailable)?
            .entry((self.path.clone(), key.clone()))
            .or_default()
            .clone();
        let mut state = account.lock_owned().await;
        let store = FileStore::load(&self.path).map_err(|_| StoreError::Unavailable)?;
        let observed = store.get_any(&key.provider, &key.account).cloned();
        if state.pending.as_ref().is_some_and(|pending| {
            observed != pending.expected && observed.as_ref() != Some(&pending.replacement)
        }) {
            state.pending = None;
            return Err(StoreError::Conflict);
        }
        Ok(Box::new(FileTransaction {
            path: self.path.clone(),
            key: key.clone(),
            observed,
            state,
        }))
    }
}

#[async_trait]
impl CredentialTransaction for FileTransaction {
    fn credential(&self) -> Option<&Credential> {
        self.observed.as_ref()
    }
    fn pending(&self) -> Option<&Credential> {
        self.state.pending.as_ref().map(|p| &p.replacement)
    }
    fn stage(&mut self, credential: Credential) {
        self.state.pending = Some(Pending {
            expected: self.observed.clone(),
            replacement: credential,
        });
    }
    async fn commit(&mut self) -> std::result::Result<Durability, StoreError> {
        let Some(pending) = self.state.pending.as_ref() else {
            return Ok(Durability::Persistent);
        };
        let mut store = FileStore::load(&self.path).map_err(|_| StoreError::Unavailable)?;
        match store.compare_exchange(
            &self.key.provider,
            &self.key.account,
            pending.expected.as_ref(),
            pending.replacement.clone(),
        ) {
            Ok(()) => {
                self.observed = Some(pending.replacement.clone());
                self.state.pending = None;
                Ok(Durability::Persistent)
            }
            Err(CredentialStoreError::Conflict) => {
                self.state.pending = None;
                Err(StoreError::Conflict)
            }
            Err(_) => Err(StoreError::Unavailable),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::credentials::OAuthToken;

    type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
    fn key(account: &str) -> CredentialKey {
        CredentialKey {
            provider: "selected".into(),
            account: account.into(),
        }
    }
    fn token(access: &str) -> Credential {
        Credential::Oauth(OAuthToken {
            access_token: access.into(),
            expires_at: 0,
            refresh_token: Some(format!("refresh-{access}")),
        })
    }

    #[tokio::test]
    async fn failed_disk_commit_retains_replacement_across_backend_instances() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("credentials.json");
        let mut store = FileStore::load(&path)?;
        store.set("selected", "one", token("old"))?;
        let backend = FileCredentialStore::new(&path)?;
        let mut transaction = backend.begin(&key("one")).await?;
        transaction.stage(token("rotated"));
        let blocked_tmp = path.with_extension("json.tmp");
        std::fs::create_dir(&blocked_tmp)?;
        assert_eq!(transaction.commit().await, Err(StoreError::Unavailable));
        drop(transaction);
        assert_eq!(
            FileStore::load(&path)?.get_any("selected", "one"),
            Some(&token("old"))
        );
        std::fs::remove_dir(&blocked_tmp)?;
        let backend =
            FileCredentialStore::new(directory.path().join(".").join("credentials.json"))?;
        let mut transaction = backend.begin(&key("one")).await?;
        assert_eq!(transaction.pending(), Some(&token("rotated")));
        assert_eq!(transaction.commit().await?, Durability::Persistent);
        assert_eq!(
            FileStore::load(&path)?.get_any("selected", "one"),
            Some(&token("rotated"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn refresh_commit_never_overwrites_login_replacement_or_resurrects_logout() -> TestResult
    {
        for replacement in [Some(token("new-login")), None] {
            let directory = tempfile::tempdir()?;
            let path = directory.path().join("credentials.json");
            let mut store = FileStore::load(&path)?;
            store.set("selected", "one", token("old"))?;
            let backend = FileCredentialStore::new(&path)?;
            let mut transaction = backend.begin(&key("one")).await?;
            transaction.stage(token("rotated-old"));
            if let Some(replacement) = replacement.clone() {
                store.set("selected", "one", replacement)?;
            } else {
                store.remove("selected", "one")?;
            }
            assert_eq!(transaction.commit().await, Err(StoreError::Conflict));
            assert!(transaction.pending().is_none());
            assert_eq!(
                FileStore::load(&path)?.get_any("selected", "one"),
                replacement.as_ref()
            );
        }
        Ok(())
    }

    #[test]
    fn stale_store_mutations_preserve_unrelated_accounts_and_failed_write_preserves_snapshot()
    -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("credentials.json");
        let mut first = FileStore::load(&path)?;
        let mut stale = FileStore::load(&path)?;
        first.set("selected", "one", token("one"))?;
        stale.set("selected", "two", token("two"))?;
        first.set("elsewhere", "three", token("three"))?;
        let blocked_tmp = path.with_extension("json.tmp");
        std::fs::create_dir(&blocked_tmp)?;
        assert!(
            first
                .set("selected", "one", token("failed-replacement"))
                .is_err()
        );
        assert_eq!(first.get_any("selected", "one"), Some(&token("one")));
        std::fs::remove_dir(&blocked_tmp)?;
        stale.remove("selected", "two")?;
        let final_store = FileStore::load(&path)?;
        assert_eq!(final_store.get_any("selected", "one"), Some(&token("one")));
        assert_eq!(
            final_store.get_any("elsewhere", "three"),
            Some(&token("three"))
        );
        assert!(final_store.get_any("selected", "two").is_none());
        Ok(())
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn selected_path_preserves_symlink_parent_semantics() -> TestResult {
        let directory = tempfile::tempdir()?;
        let nested = directory.path().join("actual").join("nested");
        std::fs::create_dir_all(&nested)?;
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&nested, &alias)?;
        let selected_path = alias.join("..").join("credentials.json");
        let mut store = FileStore::load(&selected_path)?;
        store.set("selected", "one", token("intended-account"))?;
        let backend = FileCredentialStore::new(&selected_path)?;
        let transaction = backend.begin(&key("one")).await?;
        assert_eq!(transaction.credential(), Some(&token("intended-account")));
        Ok(())
    }
}
