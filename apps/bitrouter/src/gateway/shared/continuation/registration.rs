//! Retain the producer identity before the first registration transaction.

use super::*;

/// One candidate owns this intent, including through an uncertain COMMIT.
/// A capability is returned only after the open record is confirmed.
pub(in crate::gateway::shared) struct Registration {
    context: SharedContinuationDb,
}

impl Registration {
    pub fn new(authority: Authority, key: ContinuationKey) -> Self {
        Self {
            context: SharedContinuationDb {
                authority,
                key,
                producer_id: uuid::Uuid::new_v4().to_string(),
            },
        }
    }

    pub fn id(&self) -> &str {
        self.context.producer_id()
    }

    pub fn namespace(&self) -> &str {
        self.context.namespace()
    }

    async fn begin(&self, opening: bool) -> Result<DatabaseTransaction> {
        let tx = self
            .context
            .authority
            .store()
            .begin_with_isolation(IsolationLevel::ReadCommitted)
            .await?;
        if opening {
            self.context.fence_key(&tx).await?;
        } else {
            // A failed unpublished candidate may carry an invalid continuation
            // key. Closing its retained producer identity needs deployment
            // authority, not the key whose validation prevented initialization.
            // Native-row decryption/authentication is never bypassed here.
            self.context.authority.fence(&tx).await?;
        }
        let digest = Sha256::digest(serde_json::to_vec(&(
            "gateway-native-producer-registration-v1",
            self.namespace(),
            self.id(),
        ))?);
        let key = i64::from_be_bytes(digest[..8].try_into()?);
        // The row can be absent while a previous registration still runs. Both
        // creation and closing absence use this identity lock; later statements
        // see commits preceding acquisition under READ COMMITTED.
        // https://www.postgresql.org/docs/current/explicit-locking.html#ADVISORY-LOCKS
        // https://www.postgresql.org/docs/current/transaction-iso.html#XACT-READ-COMMITTED
        tx.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT pg_advisory_xact_lock($1)",
            [key.into()],
        ))
        .await?;
        Ok(tx)
    }

    pub async fn open(&self) -> Result<SharedContinuationDb> {
        self.context
            .retry(|| async {
                let tx = self.begin(true).await?;
                let store = self.context.authority.store();
                if store
                    .read::<Producer>(&tx, Kind::ContinuationProducer, self.id())
                    .await?
                    .is_none()
                {
                    let now = SharedContinuationDb::now(&tx).await?;
                    ensure!(
                        store
                            .create(
                                &tx,
                                NewRecord {
                                    kind: Kind::ContinuationProducer,
                                    key: self.id(),
                                    status: "open",
                                    due_at_ms: None,
                                    now_ms: now.timestamp_millis(),
                                    value: &Producer {
                                        instance_id: self.id().into(),
                                        closed_at_us: None
                                    },
                                }
                            )
                            .await?,
                        "continuation registration identity conflict"
                    );
                }
                let producer = self.context.lock_producer(&tx).await?;
                ensure!(
                    producer.value.closed_at_us.is_none(),
                    "continuation registration is closed"
                );
                tx.commit().await?;
                Ok(self.context.clone())
            })
            .await
    }

    /// Call only after the candidate build has joined. A closed tombstone fences
    /// a delayed registration even when its original outcome was unobservable.
    pub async fn close(&self) -> Result<ProducerClosure> {
        self.context
            .retry(|| async {
                let tx = self.begin(false).await?;
                let store = self.context.authority.store();
                if store
                    .read::<Producer>(&tx, Kind::ContinuationProducer, self.id())
                    .await?
                    .is_none()
                {
                    let now = SharedContinuationDb::now(&tx).await?;
                    ensure!(
                        store
                            .create(
                                &tx,
                                NewRecord {
                                    kind: Kind::ContinuationProducer,
                                    key: self.id(),
                                    status: "closed",
                                    due_at_ms: None,
                                    now_ms: now.timestamp_millis(),
                                    value: &Producer {
                                        instance_id: self.id().into(),
                                        closed_at_us: Some(now.timestamp_micros())
                                    },
                                }
                            )
                            .await?,
                        "continuation close identity conflict"
                    );
                } else {
                    let mut producer = store
                        .lock::<Producer>(&tx, Kind::ContinuationProducer, self.id())
                        .await?;
                    self.context.validate_producer(&producer)?;
                    if producer.value.closed_at_us.is_none() {
                        let now = SharedContinuationDb::now(&tx).await?;
                        producer.value.closed_at_us = Some(now.timestamp_micros());
                        store
                            .save(&tx, &mut producer, "closed", None, now.timestamp_millis())
                            .await?;
                    }
                }
                tx.commit().await?;
                Ok(self.context.closure())
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
    async fn retained_identity_closes_with_a_changed_unpublished_key() -> Result<()> {
        let url = std::env::var("BITROUTER_TEST_DATABASE_URL")?;
        let db = crate::gateway::shared::store::connect(&url, 2, std::time::Duration::from_secs(3))
            .await?;
        let result = async {
            let live = SharedContinuationDb::fixture(db.clone()).await?;
            let mut intent = Registration::new(live.authority.clone(), live.key());
            // This is a confirmed creation with a discarded return value, not
            // an injected loss of the database COMMIT acknowledgement.
            drop(intent.open().await?);
            intent.context.key = ContinuationKey::from_bytes(rand::random())?;
            ensure!(intent.open().await.is_err());
            intent.close().await?;
            intent.context.key = live.key();
            ensure!(intent.open().await.is_err());
            live.begin_bind("unrelated-live-producer")
                .await?
                .rollback()
                .await?;
            Ok(())
        }
        .await;
        let closed = db.close().await;
        result?;
        closed?;
        Ok(())
    }
}
