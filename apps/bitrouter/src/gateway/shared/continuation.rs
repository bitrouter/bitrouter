//! Transaction authority for shared native continuation publication.
//!
//! The producer gate proves that an earlier bind cannot commit after closure.
//! It does not prove that Rust producers have joined or that native rows have
//! been compensated. The generation owner must establish those other barriers.

use std::future::Future;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DatabaseTransaction, IsolationLevel,
    Statement,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::continuation::ContinuationKey;

use super::authority::Authority;
use super::store::{Kind, NewRecord, Stored};

/// A single immutable generation's authority, never inferred from its backend.
#[derive(Clone)]
pub(crate) struct SharedContinuationDb {
    authority: Authority,
    key: ContinuationKey,
    producer_id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Producer {
    instance_id: String,
    closed_at_us: Option<i64>,
}

/// Evidence for one exact committed producer closure, not a process drain.
pub(crate) struct ProducerClosure {
    namespace: String,
    producer_id: String,
}

impl SharedContinuationDb {
    /// Allocate a new producer only against an explicitly initialized authority.
    /// Failure never returns a capability with an unconfirmed producer record.
    pub(super) async fn create(authority: Authority, key: ContinuationKey) -> Result<Self> {
        let context = Self {
            authority,
            key,
            producer_id: uuid::Uuid::new_v4().to_string(),
        };
        context
            .retry(|| async {
                let tx = context.authority.store().begin().await?;
                context.fence_key(&tx).await?;
                let now = Self::now(&tx).await?;
                let created = context
                    .authority
                    .store()
                    .create(
                        &tx,
                        NewRecord {
                            kind: Kind::ContinuationProducer,
                            key: &context.producer_id,
                            status: "open",
                            due_at_ms: None,
                            now_ms: now.timestamp_millis(),
                            value: &Producer {
                                instance_id: context.producer_id.clone(),
                                closed_at_us: None,
                            },
                        },
                    )
                    .await?;
                ensure!(created, "continuation producer identity already exists");
                tx.commit().await?;
                Ok(())
            })
            .await?;
        Ok(context)
    }

    pub(crate) fn database(&self) -> &DatabaseConnection {
        self.authority.store().database()
    }

    pub(crate) fn namespace(&self) -> &str {
        self.authority.store().namespace()
    }

    pub(crate) fn producer_id(&self) -> &str {
        &self.producer_id
    }

    pub(crate) fn key(&self) -> ContinuationKey {
        self.key.clone()
    }

    #[cfg(test)]
    pub(crate) async fn fixture(database: DatabaseConnection) -> Result<Self> {
        use ring::rand::{SecureRandom, SystemRandom};

        let namespace = format!("native-fixture-{}", uuid::Uuid::new_v4());
        let store =
            super::store::Store::new(database, &namespace, std::time::Duration::from_secs(3))?;
        super::schema::initialize(&store).await?;
        let mut secret = [0_u8; 32];
        SystemRandom::new()
            .fill(&mut secret)
            .map_err(|_| anyhow::anyhow!("fixture randomness unavailable"))?;
        let authority = Authority::new(store, super::crypto::DeploymentKey::from_bytes(secret)?);
        let head = authority.initialize().await?;
        let key = authority.keys(&head).await?.continuation()?;
        Self::create(authority, key).await
    }

    /// Bound the entire transaction, including pool checkout and acknowledgement.
    pub(crate) async fn bounded<T>(&self, operation: impl Future<Output = Result<T>>) -> Result<T> {
        self.authority.store().bounded(operation).await
    }

    /// Retry only proven transaction aborts. Recompute ciphertext/time each try.
    pub(crate) async fn retry<T, F, Fut>(&self, operation: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        self.authority.store().retry(operation).await
    }

    /// Lock key authority, open producer, then the exact namespaced identity.
    /// The caller must lock an existing native row before sampling `now`.
    pub(crate) async fn begin_bind(&self, identity: &str) -> Result<DatabaseTransaction> {
        let tx = self.authority.store().begin().await?;
        self.fence_key(&tx).await?;
        let producer = self.lock_producer(&tx).await?;
        ensure!(
            producer.value.closed_at_us.is_none(),
            "continuation producer is closed"
        );
        self.lock_identity(&tx, identity).await?;
        Ok(tx)
    }

    /// Cleanup can proceed after closing a producer, but can never reopen it.
    pub(crate) async fn begin_cleanup(&self, identity: &str) -> Result<DatabaseTransaction> {
        let tx = self.begin_read().await?;
        self.lock_identity(&tx, identity).await?;
        Ok(tx)
    }

    /// A bounded hint scan has no identity authority. Mutations must subsequently
    /// acquire their identity and row locks and revalidate the current snapshot.
    pub(crate) async fn begin_read(&self) -> Result<DatabaseTransaction> {
        let tx = self.authority.store().begin().await?;
        self.fence_key(&tx).await?;
        self.lock_producer(&tx).await?;
        Ok(tx)
    }

    /// Use only after pipeline sources/tasks join and the worker restart gate
    /// closes. No Rust task is joined while holding this database transaction.
    pub(crate) async fn close_binds(&self) -> Result<ProducerClosure> {
        self.retry(|| async {
            let tx = self.authority.store().begin().await?;
            self.fence_key(&tx).await?;
            // FOR UPDATE conflicts with each bind's FOR SHARE. A writer which
            // already passed the gate commits or aborts before this can close.
            // https://www.postgresql.org/docs/current/explicit-locking.html#LOCKING-ROWS
            let mut producer: Stored<Producer> = self
                .authority
                .store()
                .lock(&tx, Kind::ContinuationProducer, &self.producer_id)
                .await?;
            self.validate_producer(&producer)?;
            if producer.value.closed_at_us.is_none() {
                let now = Self::now(&tx).await?;
                producer.value.closed_at_us = Some(now.timestamp_micros());
                self.authority
                    .store()
                    .save(&tx, &mut producer, "closed", None, now.timestamp_millis())
                    .await?;
            }
            tx.commit().await?;
            Ok(self.closure())
        })
        .await
    }

    /// Resolve a lost close acknowledgement from a fresh primary snapshot.
    /// An error or an open/missing record is never evidence of closure.
    pub(crate) async fn confirm_closed(&self) -> Result<Option<ProducerClosure>> {
        self.bounded(async {
            let tx = self
                .authority
                .store()
                .begin_with_isolation(IsolationLevel::ReadCommitted)
                .await?;
            self.fence_key(&tx).await?;
            let producer = self.lock_producer(&tx).await?;
            let closed = producer.value.closed_at_us.is_some();
            tx.commit().await?;
            Ok(closed.then(|| self.closure()))
        })
        .await
    }

    /// Inspect an uncertain bind only after the same producer's durable close.
    /// Absence observed in this transaction cannot be invalidated by a late bind.
    pub(crate) async fn begin_closed_identity(
        &self,
        closure: &ProducerClosure,
        identity: &str,
    ) -> Result<DatabaseTransaction> {
        ensure!(
            closure.namespace == self.namespace() && closure.producer_id == self.producer_id,
            "continuation closure belongs to another producer"
        );
        // Each statement sees commits preceding the acquired gate/identity lock.
        // https://www.postgresql.org/docs/current/transaction-iso.html#XACT-READ-COMMITTED
        let tx = self
            .authority
            .store()
            .begin_with_isolation(IsolationLevel::ReadCommitted)
            .await?;
        self.fence_key(&tx).await?;
        let producer = self.lock_producer(&tx).await?;
        ensure!(
            producer.value.closed_at_us.is_some(),
            "continuation producer closure is not durable"
        );
        self.lock_identity(&tx, identity).await?;
        Ok(tx)
    }

    /// Microsecond precision matches the authenticated native timestamp format.
    /// Call after every possibly blocking ownership lock, never before it.
    pub(crate) async fn now(connection: &impl ConnectionTrait) -> Result<DateTime<Utc>> {
        // CURRENT_TIMESTAMP is the transaction start, not the decision time.
        // https://www.postgresql.org/docs/current/functions-datetime.html#FUNCTIONS-DATETIME-CURRENT
        let micros: i64 = connection
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000000)::bigint AS now_us",
            ))
            .await?
            .context("continuation database clock returned no row")?
            .try_get("", "now_us")?;
        DateTime::from_timestamp_micros(micros)
            .context("continuation database clock outside supported range")
    }

    async fn fence_key(&self, tx: &DatabaseTransaction) -> Result<()> {
        self.authority.fence_continuation_key(tx, &self.key).await
    }

    async fn lock_producer(&self, tx: &DatabaseTransaction) -> Result<Stored<Producer>> {
        let producer = self
            .authority
            .store()
            .lock_shared(tx, Kind::ContinuationProducer, &self.producer_id)
            .await?;
        self.validate_producer(&producer)?;
        Ok(producer)
    }

    fn validate_producer(&self, producer: &Stored<Producer>) -> Result<()> {
        ensure!(
            producer.value.instance_id == self.producer_id,
            "continuation producer record identity mismatch"
        );
        Ok(())
    }

    fn closure(&self) -> ProducerClosure {
        ProducerClosure {
            namespace: self.namespace().into(),
            producer_id: self.producer_id.clone(),
        }
    }

    async fn lock_identity(&self, tx: &DatabaseTransaction, identity: &str) -> Result<()> {
        ensure!(
            !identity.is_empty() && identity.len() <= 256,
            "invalid shared continuation identity length"
        );
        let digest = Sha256::digest(serde_json::to_vec(&(
            "gateway-native-continuation-fence-v1",
            self.namespace(),
            identity,
        ))?);
        let lock_key = i64::from_be_bytes(digest[..8].try_into()?);
        // Row locks do not cover absence. This transaction advisory lock fences
        // insertion for one identity; a hash collision only adds contention.
        // https://www.postgresql.org/docs/current/explicit-locking.html#ADVISORY-LOCKS
        tx.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT pg_advisory_xact_lock($1)",
            [lock_key.into()],
        ))
        .await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "continuation_tests.rs"]
mod tests;
