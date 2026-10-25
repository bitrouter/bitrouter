//! Encrypted account credentials and immutable dispatch bindings.

use anyhow::{Context, Result, ensure};
use sea_orm::DatabaseTransaction;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::gateway::providers::auth::credentials::Credential;
use crate::gateway::providers::auth::store::CredentialKey;

use super::authority::Authority;
use super::engine::Rejection;
use super::store::{Kind, NewRecord, Store, Stored};
use crate::runtime_crypto::SealedSecret;

pub(crate) mod observation;
mod refresh;

/// An application-owned binding; neither tokens nor backend errors are public.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CredentialStamp {
    pub id: String,
    pub pool_id: String,
    pub version: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Account {
    stamp: CredentialStamp,
    secret: SealedSecret,
    #[serde(default)]
    lease: Option<RefreshLease>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshLease {
    token: String,
    expires_at_ms: i64,
    started: bool,
}

#[derive(Clone)]
pub(super) struct Credentials {
    authority: Authority,
}

impl Credentials {
    pub fn new(authority: Authority) -> Self {
        Self { authority }
    }

    /// Explicit import is the only path that replaces account authority. A
    /// replacement keeps its resource identity; rebinding requires a new slot.
    #[cfg(test)]
    pub async fn import(
        &self,
        slot: &CredentialKey,
        pool_id: &str,
        expected_version: Option<u64>,
        credential: &Credential,
    ) -> Result<CredentialStamp> {
        let prepared = PreparedImport::new(slot, pool_id, expected_version, credential)?;
        let store = self.authority.store();
        store
            .retry(|| async {
                let tx = store.begin().await?;
                self.authority.fence(&tx).await?;
                let stamp = self.import_in(&tx, &prepared).await?;
                tx.commit().await?;
                Ok(stamp)
            })
            .await
    }

    /// The caller holds the head fence and commits the mutation and its receipt.
    pub async fn import_in(
        &self,
        tx: &DatabaseTransaction,
        input: &PreparedImport,
    ) -> Result<CredentialStamp> {
        let store = self.authority.store();
        let PreparedImport {
            id,
            pool_id,
            expected_version,
            bytes,
        } = input;
        let pool_id = pool_id.as_str();
        let expected_version = *expected_version;
        let _: Stored<serde_json::Value> = store
            .lock_optional(tx, Kind::Resource, pool_id)
            .await?
            .ok_or(Rejection::NotFound)?;
        let now = Store::now(tx).await?;
        let version = match expected_version {
            Some(previous) => previous
                .checked_add(1)
                .context("credential version exhausted")?,
            None => 1,
        };
        let stamp = CredentialStamp {
            id: id.clone(),
            pool_id: pool_id.into(),
            version,
        };
        let secret = self.authority.seal(id, version, bytes)?;
        if let Some(expected) = expected_version {
            let mut account: Stored<Account> = store
                .lock_optional(tx, Kind::Credential, id)
                .await?
                .ok_or(Rejection::NotFound)?;
            ensure!(
                account.value.stamp.version == expected && account.value.stamp.pool_id == pool_id,
                Rejection::AuthorityChanged
            );
            account.value = Account {
                stamp: stamp.clone(),
                secret,
                lease: None,
            };
            store.save(tx, &mut account, "ready", None, now).await?;
        } else {
            ensure!(
                store
                    .create(
                        tx,
                        NewRecord {
                            kind: Kind::Credential,
                            key: id,
                            status: "ready",
                            due_at_ms: None,
                            now_ms: now,
                            value: &Account {
                                stamp: stamp.clone(),
                                secret,
                                lease: None,
                            },
                        }
                    )
                    .await?,
                Rejection::AuthorityChanged
            );
        }

        Ok(stamp)
    }

    /// Metadata reads never decrypt or return provider material.
    pub async fn metadata_in(
        &self,
        tx: &DatabaseTransaction,
        slot: &CredentialKey,
    ) -> Result<CredentialStamp> {
        let id = account_id(slot)?;
        let account: Stored<Account> = self
            .authority
            .store()
            .read(tx, Kind::Credential, &id)
            .await?
            .ok_or(Rejection::NotFound)?;
        ensure!(
            account.value.stamp.id == id,
            "shared credential identity mismatch"
        );
        Ok(account.value.stamp)
    }

    /// Read a versioned snapshot without acquiring a refresh lease. The caller
    /// must bind this exact stamp to its request and fence it before dispatch.
    pub async fn load(&self, slot: &CredentialKey) -> Result<(CredentialStamp, Credential)> {
        let store = self.authority.store();
        store
            .retry(|| async {
                let tx = store.begin().await?;
                self.authority.fence(&tx).await?;
                let result = self.load_in(&tx, slot).await?;
                tx.commit().await?;
                Ok(result)
            })
            .await
    }

    /// Validate encrypted material under the caller's head fence. Publication
    /// cannot bless a corrupt credential merely because its metadata is present.
    pub async fn load_in(
        &self,
        tx: &DatabaseTransaction,
        slot: &CredentialKey,
    ) -> Result<(CredentialStamp, Credential)> {
        let store = self.authority.store();
        let id = account_id(slot)?;
        let account: Stored<Account> = store
            .read(tx, Kind::Credential, &id)
            .await?
            .context("shared credential is missing")?;
        ensure!(
            account.value.stamp.id == id
                && !account
                    .value
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.started),
            "shared credential identity mismatch"
        );
        let bytes = self
            .authority
            .open(&id, account.value.stamp.version, &account.value.secret)?;
        let credential =
            serde_json::from_slice(&bytes).context("shared credential encoding is invalid")?;

        Ok((account.value.stamp, credential))
    }

    /// Participates in the caller's short transaction after the resource lock.
    pub async fn fence(&self, tx: &DatabaseTransaction, expected: &CredentialStamp) -> Result<()> {
        let account: Stored<Account> = self
            .authority
            .store()
            .lock_shared(tx, Kind::Credential, &expected.id)
            .await?;
        ensure!(
            account.value.stamp == *expected
                && !account
                    .value
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.started),
            Rejection::AuthorityChanged
        );
        Ok(())
    }
}

fn account_id(slot: &CredentialKey) -> Result<String> {
    super::engine::validate_id(&slot.provider)?;
    super::engine::validate_id(&slot.account)?;
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
        "gateway-credential-slot-v1",
        &slot.provider,
        &slot.account,
    ))?)))
}

/// Validated write-only material; never formatted or persisted without sealing.
pub(super) struct PreparedImport {
    id: String,
    pool_id: String,
    expected_version: Option<u64>,
    bytes: Vec<u8>,
}

impl PreparedImport {
    pub fn new(
        slot: &CredentialKey,
        pool_id: &str,
        expected_version: Option<u64>,
        credential: &Credential,
    ) -> Result<Self> {
        let id = account_id(slot)?;
        super::engine::validate_id(pool_id)?;
        match credential {
            Credential::ApiKey { value } => {
                ensure!(!value.is_empty(), "shared credential cannot be empty")
            }
            Credential::Oauth(token) => ensure!(
                !token.access_token.is_empty(),
                "shared credential cannot be empty"
            ),
            Credential::ClaudeCodeCli => {
                anyhow::bail!("shared credentials cannot reference a process-local CLI store")
            }
        }
        let bytes = serde_json::to_vec(credential)?;
        ensure!(
            bytes.len() <= 1024 * 1024,
            "shared credential exceeds byte limit"
        );

        ensure!(
            expected_version != Some(u64::MAX),
            "credential version exhausted"
        );
        Ok(Self {
            id,
            pool_id: pool_id.into(),
            expected_version,
            bytes,
        })
    }
}
