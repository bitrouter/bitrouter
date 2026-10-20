//! Encrypted account credentials and immutable dispatch bindings.

use anyhow::{Context, Result, ensure};
use sea_orm::DatabaseTransaction;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::gateway::providers::auth::credentials::Credential;
use crate::gateway::providers::auth::store::CredentialKey;

use super::authority::Authority;
use super::crypto::SealedSecret;
use super::engine::Rejection;
use super::store::{Kind, NewRecord, Store, Stored};

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
    pub async fn import(
        &self,
        slot: &CredentialKey,
        pool_id: &str,
        expected_version: Option<u64>,
        credential: &Credential,
    ) -> Result<CredentialStamp> {
        let id = account_id(slot)?;
        ensure!(
            !pool_id.is_empty() && pool_id.len() <= 256,
            "invalid resource pool identity"
        );
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
        let store = self.authority.store();
        store
            .retry(|| async {
                let tx = store.begin().await?;
                self.authority.fence(&tx).await?;
                let _: Stored<serde_json::Value> = store.lock(&tx, Kind::Resource, pool_id).await?;
                let now = Store::now(&tx).await?;
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
                let secret = self.authority.seal(&id, version, &bytes)?;
                if let Some(expected) = expected_version {
                    let mut account: Stored<Account> =
                        store.lock(&tx, Kind::Credential, &id).await?;
                    ensure!(
                        account.value.stamp.version == expected
                            && account.value.stamp.pool_id == pool_id,
                        Rejection::AuthorityChanged
                    );
                    account.value = Account {
                        stamp: stamp.clone(),
                        secret,
                        lease: None,
                    };
                    store.save(&tx, &mut account, "ready", None, now).await?;
                } else {
                    ensure!(
                        store
                            .create(
                                &tx,
                                NewRecord {
                                    kind: Kind::Credential,
                                    key: &id,
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
                tx.commit().await?;
                Ok(stamp)
            })
            .await
    }

    pub async fn load(&self, slot: &CredentialKey) -> Result<(CredentialStamp, Credential)> {
        let id = account_id(slot)?;
        let store = self.authority.store();
        store
            .bounded(async {
                let tx = store.begin().await?;
                self.authority.fence(&tx).await?;
                let account: Stored<Account> = store
                    .read(&tx, Kind::Credential, &id)
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
                let bytes =
                    self.authority
                        .open(&id, account.value.stamp.version, &account.value.secret)?;
                let credential = serde_json::from_slice(&bytes)
                    .context("shared credential encoding is invalid")?;
                tx.commit().await?;
                Ok((account.value.stamp, credential))
            })
            .await
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
    ensure!(
        !slot.provider.is_empty()
            && slot.provider.len() <= 256
            && !slot.account.is_empty()
            && slot.account.len() <= 256,
        "invalid shared credential slot"
    );
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
        "gateway-credential-slot-v1",
        &slot.provider,
        &slot.account,
    ))?)))
}
