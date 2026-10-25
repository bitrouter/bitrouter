//! Publication fencing and deployment-private key epochs.

use anyhow::{Context, Result, ensure};
use ring::rand::SystemRandom;
use sea_orm::DatabaseTransaction;
use serde::{Deserialize, Serialize};

use crate::continuation::ContinuationKey;
use crate::trajectory::canonical::CorrelationKey;

use super::store::{Kind, NewRecord, Store, Stored};
use crate::runtime_crypto::{DeploymentKey, SealedSecret, SecretIdentity};

const MAX_CORRELATION_EPOCHS: usize = 16;
const HEAD: &str = "head";

/// Both components belong to the immutable application selected for admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Revision {
    pub config_generation: u64,
    pub key_generation: u64,
}

impl Revision {
    pub fn advances(self, previous: Self) -> bool {
        self != previous
            && self.config_generation >= previous.config_generation
            && self.key_generation >= previous.key_generation
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Head {
    format: u32,
    deployment_key_id: String,
    pub config_generation: u64,
    pub key_generation: u64,
}

impl Head {
    pub fn revision(&self) -> Revision {
        Revision {
            config_generation: self.config_generation,
            key_generation: self.key_generation,
        }
    }
}

/// Only encrypted instances of this type are persisted; it has no Debug impl.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct KeyRing {
    continuation: [u8; 32],
    correlations: Vec<[u8; 32]>,
}

impl KeyRing {
    pub fn continuation(&self) -> Result<ContinuationKey> {
        ContinuationKey::from_bytes(self.continuation)
    }

    /// Oldest to newest. Existing histories keep their original epoch.
    pub fn correlations(&self) -> Result<Vec<CorrelationKey>> {
        ensure!(
            !self.correlations.is_empty() && self.correlations.len() <= MAX_CORRELATION_EPOCHS,
            "invalid shared correlation epoch count"
        );
        self.correlations
            .iter()
            .map(|secret| CorrelationKey::from_bytes(*secret))
            .collect()
    }

    pub fn active_correlation(&self) -> Result<CorrelationKey> {
        CorrelationKey::from_bytes(
            *self
                .correlations
                .last()
                .context("missing shared correlation epoch")?,
        )
    }
}

#[derive(Clone)]
pub(super) struct Authority {
    store: Store,
    key: DeploymentKey,
}

impl Authority {
    pub fn new(store: Store, key: DeploymentKey) -> Self {
        Self { store, key }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn operator_fingerprint(&self, purpose: &str, input: &[u8]) -> Result<String> {
        self.key.fingerprint(self.store.namespace(), purpose, input)
    }

    pub fn with_database(&self, db: sea_orm::DatabaseConnection) -> Result<Self> {
        Ok(Self::new(self.store.with_database(db)?, self.key.clone()))
    }

    pub fn seal(&self, record: &str, revision: u64, plaintext: &[u8]) -> Result<SealedSecret> {
        self.key.seal(
            SecretIdentity {
                namespace: self.store.namespace(),
                record,
                revision,
            },
            plaintext,
        )
    }

    pub fn open(&self, record: &str, revision: u64, ciphertext: &SealedSecret) -> Result<Vec<u8>> {
        self.key.open(
            SecretIdentity {
                namespace: self.store.namespace(),
                record,
                revision,
            },
            ciphertext,
        )
    }

    /// Explicit operator initialization. Joining never creates an authority.
    #[cfg(test)]
    pub async fn initialize(&self) -> Result<Head> {
        self.store
            .retry(|| async {
                let tx = self.store.begin().await?;
                let (created, head) = self.initialize_or_lock(&tx).await?;
                ensure!(created, "shared namespace is already initialized");
                tx.commit().await?;
                Ok(head.value)
            })
            .await
    }

    /// Explicit bootstrap only. Existing authority is exclusively fenced and
    /// validated, never replaced; the caller must verify its bootstrap receipt.
    pub async fn initialize_or_lock(
        &self,
        tx: &DatabaseTransaction,
    ) -> Result<(bool, Stored<Head>)> {
        let initial = Head {
            format: 1,
            deployment_key_id: self.key.id().into(),
            config_generation: 0,
            key_generation: 1,
        };
        let now = Store::now(tx).await?;
        let created = self
            .store
            .create(
                tx,
                NewRecord {
                    kind: Kind::Authority,
                    key: HEAD,
                    status: "active",
                    due_at_ms: None,
                    now_ms: now,
                    value: &initial,
                },
            )
            .await?;
        let head = self.fence_exclusive(tx).await?;
        if created {
            let ring = KeyRing {
                continuation: random_key()?,
                correlations: vec![random_key()?],
            };
            self.write_keys(tx, &ring, initial.key_generation, now)
                .await?;
        }
        Ok((created, head))
    }

    /// Hold this shared fence through the complete admission/dispatch transaction.
    /// Writers use an exclusive lock, so a stale authorization cannot commit
    /// after publication, even when serializable commit order differs.
    pub async fn fence(&self, tx: &DatabaseTransaction) -> Result<Stored<Head>> {
        let head: Stored<Head> = self.store.lock_shared(tx, Kind::Authority, HEAD).await?;
        self.validate_head(&head.value)?;
        Ok(head)
    }

    /// Select this mode before acquiring any receipt or subordinate row; never
    /// upgrade a shared head lock after taking another lock.
    pub async fn fence_exclusive(&self, tx: &DatabaseTransaction) -> Result<Stored<Head>> {
        let head: Stored<Head> = self.store.lock(tx, Kind::Authority, HEAD).await?;
        self.validate_head(&head.value)?;
        Ok(head)
    }

    /// Validate native continuation authority under the same publication fence.
    /// Correlation-only rotation does not revoke an unchanged continuation key.
    pub async fn fence_continuation_key(
        &self,
        tx: &DatabaseTransaction,
        expected: &ContinuationKey,
    ) -> Result<()> {
        let head = self.fence(tx).await?;
        let keys = self.read_keys(tx, head.value.key_generation).await?;
        ensure!(
            keys.continuation()? == *expected,
            "shared continuation key authority changed"
        );
        Ok(())
    }

    pub async fn head(&self) -> Result<Head> {
        self.store
            .bounded(async {
                let head: Stored<Head> = self
                    .store
                    .get(Kind::Authority, HEAD)
                    .await?
                    .context("shared namespace has not been initialized")?;
                self.validate_head(&head.value)?;
                Ok(head.value)
            })
            .await
    }

    pub async fn keys(&self, head: &Head) -> Result<KeyRing> {
        self.validate_head(head)?;
        self.store
            .bounded(async {
                let tx = self.store.begin().await?;
                let keys = self.read_keys(&tx, head.key_generation).await?;
                tx.commit().await?;
                Ok(keys)
            })
            .await
    }

    /// Rotation retains every prior epoch. At the bound operators must retain
    /// history rather than silently dropping a key that may still be referenced.
    #[cfg(test)]
    pub async fn rotate_correlation(&self, expected_generation: u64) -> Result<Head> {
        self.store
            .retry(|| async {
                let tx = self.store.begin().await?;
                let mut head = self.fence_exclusive(&tx).await?;
                self.rotate_correlation_in(&tx, &mut head, expected_generation)
                    .await?;
                tx.commit().await?;
                Ok(head.value)
            })
            .await
    }

    /// The caller holds the exclusive head fence and owns the receipt commit.
    pub async fn rotate_correlation_in(
        &self,
        tx: &DatabaseTransaction,
        head: &mut Stored<Head>,
        expected_generation: u64,
    ) -> Result<()> {
        ensure!(
            head.value.key_generation == expected_generation,
            super::engine::Rejection::AuthorityChanged
        );
        let mut ring = self.read_keys(tx, expected_generation).await?;
        ensure!(
            ring.correlations.len() < MAX_CORRELATION_EPOCHS,
            super::engine::Rejection::AuthorityChanged
        );
        ring.correlations.push(random_key()?);
        head.value.key_generation = expected_generation
            .checked_add(1)
            .context("key generation exhausted")?;
        let now = Store::now(tx).await?;
        self.write_keys(tx, &ring, head.value.key_generation, now)
            .await?;
        self.store.save(tx, head, "active", None, now).await?;
        Ok(())
    }

    async fn read_keys(
        &self,
        connection: &DatabaseTransaction,
        generation: u64,
    ) -> Result<KeyRing> {
        let record = format!("keys/{generation}");
        let sealed: Stored<SealedSecret> = self
            .store
            .read(connection, Kind::Authority, &record)
            .await?
            .context("shared key generation is missing")?;
        ensure!(
            sealed.revision()? == 0,
            "immutable shared key generation was modified"
        );
        let plaintext = self.key.open(
            SecretIdentity {
                namespace: self.store.namespace(),
                record: &record,
                revision: generation,
            },
            &sealed.value,
        )?;
        let ring: KeyRing =
            serde_json::from_slice(&plaintext).context("invalid shared key ring")?;
        ring.correlations()?;
        Ok(ring)
    }

    async fn write_keys(
        &self,
        tx: &DatabaseTransaction,
        ring: &KeyRing,
        generation: u64,
        now_ms: i64,
    ) -> Result<()> {
        let record = format!("keys/{generation}");
        let sealed = self.key.seal(
            SecretIdentity {
                namespace: self.store.namespace(),
                record: &record,
                revision: generation,
            },
            &serde_json::to_vec(ring)?,
        )?;
        ensure!(
            self.store
                .create(
                    tx,
                    NewRecord {
                        kind: Kind::Authority,
                        key: &record,
                        status: "retained",
                        due_at_ms: None,
                        now_ms,
                        value: &sealed,
                    }
                )
                .await?,
            "shared key generation already exists"
        );
        Ok(())
    }

    pub(super) fn validate_head(&self, head: &Head) -> Result<()> {
        ensure!(head.format == 1, "unsupported shared authority format");
        ensure!(
            head.deployment_key_id == self.key.id(),
            "shared deployment key does not match authority"
        );
        ensure!(head.key_generation > 0, "invalid shared key generation");
        Ok(())
    }
}

fn random_key() -> Result<[u8; 32]> {
    ring::rand::generate(&SystemRandom::new())
        .map(|key| key.expose())
        .map_err(|_| anyhow::anyhow!("generating shared authority key failed"))
}
