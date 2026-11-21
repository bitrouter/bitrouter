//! Explicit account storage for provider lifecycle fixtures.

use super::*;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

#[derive(Default)]
struct MemoryAccount {
    credential: Option<Credential>,
    pending: Option<Credential>,
}

/// Explicit in-memory storage with no ambient credential loading.
#[derive(Default)]
pub struct MemoryCredentialStore {
    accounts: Mutex<HashMap<CredentialKey, Arc<AsyncMutex<MemoryAccount>>>>,
}

impl MemoryCredentialStore {
    /// Construct storage from caller-supplied account credentials.
    pub fn new(credentials: impl IntoIterator<Item = (CredentialKey, Credential)>) -> Self {
        Self {
            accounts: Mutex::new(
                credentials
                    .into_iter()
                    .map(|(key, credential)| {
                        (
                            key,
                            Arc::new(AsyncMutex::new(MemoryAccount {
                                credential: Some(credential),
                                pending: None,
                            })),
                        )
                    })
                    .collect(),
            ),
        }
    }
}

struct MemoryTransaction(OwnedMutexGuard<MemoryAccount>);

#[async_trait]
impl CredentialTransaction for MemoryTransaction {
    fn credential(&self) -> Option<&Credential> {
        self.0.credential.as_ref()
    }
    fn pending(&self) -> Option<&Credential> {
        self.0.pending.as_ref()
    }
    fn stage(&mut self, credential: Credential) {
        self.0.pending = Some(credential);
    }
    async fn commit(&mut self) -> std::result::Result<Durability, StoreError> {
        if let Some(credential) = self.0.pending.take() {
            self.0.credential = Some(credential);
        }
        Ok(Durability::Memory)
    }
}

#[async_trait]
impl CredentialStore for MemoryCredentialStore {
    async fn begin(
        &self,
        key: &CredentialKey,
    ) -> std::result::Result<Box<dyn CredentialTransaction>, StoreError> {
        let account = self
            .accounts
            .lock()
            .map_err(|_| StoreError::Unavailable)?
            .entry(key.clone())
            .or_default()
            .clone();
        Ok(Box::new(MemoryTransaction(account.lock_owned().await)))
    }
}
