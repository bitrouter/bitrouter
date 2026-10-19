//! Durable refresh intent with no database lock held across provider I/O.

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;

use super::{Account, CredentialStamp, Credentials, RefreshLease, account_id};
use crate::gateway::providers::auth::{
    credentials::Credential,
    store::{CredentialKey, CredentialStore, CredentialTransaction, Durability, StoreError},
};
use crate::gateway::shared::store::{Kind, Store, Stored};

const REFRESH_LEASE_MS: i64 = 60_000;

struct Transaction {
    backend: Credentials,
    stamp: CredentialStamp,
    token: String,
    credential: Credential,
    pending: Option<Credential>,
    completed: bool,
}

#[async_trait]
impl CredentialStore for Credentials {
    async fn begin(
        &self,
        key: &CredentialKey,
    ) -> std::result::Result<Box<dyn CredentialTransaction>, StoreError> {
        let id = account_id(key).map_err(|_| StoreError::Conflict)?;
        let token = uuid::Uuid::new_v4().to_string();
        let store = self.authority.store();
        let (stamp, credential) = store
            .retry(|| async {
                let tx = store.begin().await?;
                self.authority.fence(&tx).await?;
                let mut account: Stored<Account> = store.lock(&tx, Kind::Credential, &id).await?;
                let now = Store::now(&tx).await?;
                if let Some(lease) = &account.value.lease {
                    // Expiry permits reclaiming an unused read lease only. Intent
                    // is permanent uncertainty until its owner commits or an
                    // operator explicitly replaces the account.
                    ensure!(
                        !lease.started && lease.expires_at_ms <= now,
                        StoreError::Conflict
                    );
                }
                ensure!(account.value.stamp.id == id, StoreError::Conflict);
                let plaintext =
                    self.authority
                        .open(&id, account.value.stamp.version, &account.value.secret)?;
                let credential: Credential = serde_json::from_slice(&plaintext)
                    .context("shared credential encoding is invalid")?;
                account.value.lease = Some(RefreshLease {
                    token: token.clone(),
                    expires_at_ms: now
                        .checked_add(REFRESH_LEASE_MS)
                        .context("refresh lease clock overflow")?,
                    started: false,
                });
                store.save(&tx, &mut account, "leased", None, now).await?;
                tx.commit().await?;
                Ok((account.value.stamp, credential))
            })
            .await
            .map_err(redact)?;
        Ok(Box::new(Transaction {
            backend: self.clone(),
            stamp,
            token,
            credential,
            pending: None,
            completed: false,
        }))
    }
}

#[async_trait]
impl CredentialTransaction for Transaction {
    fn credential(&self) -> Option<&Credential> {
        Some(&self.credential)
    }
    fn pending(&self) -> Option<&Credential> {
        self.pending.as_ref()
    }
    fn stage(&mut self, credential: Credential) {
        self.pending = Some(credential);
    }

    async fn prepare_refresh(&mut self) -> std::result::Result<(), StoreError> {
        let store = self.backend.authority.store();
        store
            .retry(|| async {
                let tx = store.begin().await?;
                self.backend.authority.fence(&tx).await?;
                let mut account: Stored<Account> =
                    store.lock(&tx, Kind::Credential, &self.stamp.id).await?;
                let now = Store::now(&tx).await?;
                let lease = self.check_lease(&account.value, now)?;
                ensure!(!lease.started, StoreError::Conflict);
                account
                    .value
                    .lease
                    .as_mut()
                    .context("refresh lease missing")?
                    .started = true;
                store
                    .save(&tx, &mut account, "refreshing", None, now)
                    .await?;
                tx.commit().await?;
                Ok(())
            })
            .await
            .map_err(redact)
    }

    async fn commit(&mut self) -> std::result::Result<Durability, StoreError> {
        let replacement = self.pending.as_ref().ok_or(StoreError::Conflict)?;
        let plaintext = serde_json::to_vec(replacement).map_err(|_| StoreError::Unavailable)?;
        if plaintext.len() > 1024 * 1024 || matches!(replacement, Credential::ClaudeCodeCli) {
            return Err(StoreError::Conflict);
        }
        let store = self.backend.authority.store();
        let version = self
            .stamp
            .version
            .checked_add(1)
            .ok_or(StoreError::Conflict)?;
        let secret = self
            .backend
            .authority
            .seal(&self.stamp.id, version, &plaintext)
            .map_err(redact)?;
        store
            .retry(|| async {
                let tx = store.begin().await?;
                self.backend.authority.fence(&tx).await?;
                let mut account: Stored<Account> =
                    store.lock(&tx, Kind::Credential, &self.stamp.id).await?;
                let now = Store::now(&tx).await?;
                ensure!(
                    self.check_lease(&account.value, now)?.started,
                    StoreError::Conflict
                );
                account.value.stamp.version = version;
                account.value.secret = secret.clone();
                account.value.lease = None;
                store.save(&tx, &mut account, "ready", None, now).await?;
                tx.commit().await?;
                Ok(())
            })
            .await
            .map_err(redact)?;
        self.stamp.version = version;
        self.credential = self.pending.take().ok_or(StoreError::Conflict)?;
        self.completed = true;
        Ok(Durability::Persistent)
    }

    async fn release(&mut self) -> std::result::Result<(), StoreError> {
        if self.completed {
            return Ok(());
        }
        let store = self.backend.authority.store();
        store
            .retry(|| async {
                let tx = store.begin().await?;
                self.backend.authority.fence(&tx).await?;
                let mut account: Stored<Account> =
                    store.lock(&tx, Kind::Credential, &self.stamp.id).await?;
                let now = Store::now(&tx).await?;
                ensure!(
                    !self.check_lease(&account.value, now)?.started,
                    StoreError::Conflict
                );
                account.value.lease = None;
                store.save(&tx, &mut account, "ready", None, now).await?;
                tx.commit().await?;
                Ok(())
            })
            .await
            .map_err(redact)?;
        self.completed = true;
        Ok(())
    }
}

impl Transaction {
    fn check_lease<'a>(&self, account: &'a Account, now: i64) -> Result<&'a RefreshLease> {
        let lease = account.lease.as_ref().ok_or(StoreError::Conflict)?;
        ensure!(
            account.stamp == self.stamp && lease.token == self.token && lease.expires_at_ms > now,
            StoreError::Conflict
        );
        Ok(lease)
    }
}

fn redact(error: anyhow::Error) -> StoreError {
    error
        .downcast_ref::<StoreError>()
        .copied()
        .unwrap_or(StoreError::Unavailable)
}
