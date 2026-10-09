//! Process-local hosted leases retain the full OAuth envelope after failed writes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use bitrouter_ai::auth::store::{Durability, StoreError};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use super::credentials::{CredentialsStore, resolved_path};
use bitrouter_ai::providers::hosted::credentials::StoredCredential;

type Accounts = HashMap<PathBuf, Arc<AsyncMutex<AccountState>>>;
static ACCOUNTS: OnceLock<Mutex<Accounts>> = OnceLock::new();

#[derive(Default)]
struct AccountState {
    pending: Option<Pending>,
}

struct Pending {
    expected: Option<StoredCredential>,
    replacement: StoredCredential,
    rejection: Option<&'static str>,
}

pub(super) struct AccountTransaction {
    path: PathBuf,
    observed: Option<StoredCredential>,
    state: OwnedMutexGuard<AccountState>,
}

impl AccountTransaction {
    pub(super) async fn begin(path: &Path) -> Result<Self, StoreError> {
        let path = resolved_path(path).map_err(|_| StoreError::Unavailable)?;
        let account = ACCOUNTS
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| StoreError::Unavailable)?
            .entry(path.clone())
            .or_default()
            .clone();
        let mut state = account.lock_owned().await;
        let observed = CredentialsStore::load(&path)
            .map_err(|_| StoreError::Unavailable)?
            .current()
            .cloned();
        if state.pending.as_ref().is_some_and(|pending| {
            observed != pending.expected && observed.as_ref() != Some(&pending.replacement)
        }) {
            state.pending = None;
            return Err(StoreError::Conflict);
        }
        Ok(Self {
            path,
            observed,
            state,
        })
    }

    pub(super) fn credential(&self) -> Option<&StoredCredential> {
        self.state
            .pending
            .as_ref()
            .map(|pending| &pending.replacement)
            .or(self.observed.as_ref())
    }

    pub(super) fn rejection(&self) -> Option<&'static str> {
        self.state
            .pending
            .as_ref()
            .and_then(|pending| pending.rejection)
    }

    pub(super) fn stage(&mut self, replacement: StoredCredential, rejection: Option<&'static str>) {
        self.state.pending = Some(Pending {
            expected: self.observed.clone(),
            replacement,
            rejection,
        });
    }

    pub(super) fn commit(&mut self) -> Result<Durability, StoreError> {
        let Some(pending) = self.state.pending.as_ref() else {
            return Ok(Durability::Persistent);
        };
        let mut store = CredentialsStore::load(&self.path).map_err(|_| StoreError::Unavailable)?;
        match store.compare_exchange(pending.expected.as_ref(), &pending.replacement) {
            Ok(()) => {
                self.observed = Some(pending.replacement.clone());
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

pub(super) struct FileHostedStore {
    path: PathBuf,
}

impl FileHostedStore {
    pub(super) fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[async_trait::async_trait]
impl bitrouter_ai::providers::hosted::session::HostedCredentialStore for FileHostedStore {
    async fn begin(
        &self,
        key: &bitrouter_ai::auth::store::CredentialKey,
    ) -> Result<
        Box<dyn bitrouter_ai::providers::hosted::session::HostedCredentialTransaction>,
        StoreError,
    > {
        if key.provider != bitrouter_ai::providers::hosted::PROVIDER_ID
            || key.account != bitrouter_ai::auth::store::DEFAULT_ACCOUNT
        {
            return Err(StoreError::Unavailable);
        }
        Ok(Box::new(AccountTransaction::begin(&self.path).await?))
    }
}

#[async_trait::async_trait]
impl bitrouter_ai::providers::hosted::session::HostedCredentialTransaction for AccountTransaction {
    fn credential(&self) -> Option<&StoredCredential> {
        AccountTransaction::credential(self)
    }
    fn rejection(&self) -> Option<&'static str> {
        AccountTransaction::rejection(self)
    }
    fn stage(&mut self, replacement: StoredCredential, rejection: Option<&'static str>) {
        AccountTransaction::stage(self, replacement, rejection);
    }
    async fn commit(&mut self) -> Result<Durability, StoreError> {
        AccountTransaction::commit(self)
    }
}
