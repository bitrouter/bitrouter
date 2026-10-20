use super::*;
use sea_orm::DatabaseConnection;

impl Engine {
    /// Keep the same owner, namespace, deployment key and lease. Joining again
    /// would create a second incarnation and could not renew the first one.
    pub fn heartbeat_handle(&self, database: DatabaseConnection) -> Result<Heartbeat> {
        Ok(Heartbeat {
            engine: Self {
                authority: self.authority.with_database(database)?,
                owner: self.owner.clone(),
                lease_ms: self.lease_ms,
            },
        })
    }

    /// A heartbeat cannot resurrect an incarnation after its lease expired.
    /// An error without Rejection::Unavailable can represent an ambiguous ACK;
    /// it is not proof of either a renewed lease or definite expiry.
    pub async fn heartbeat(&self) -> Result<LeaseObservation> {
        self.store()
            .retry(|| async {
                let tx = self.store().begin().await?;
                self.authority.fence(&tx).await?;
                let mut instance: Stored<Instance> =
                    self.store().lock(&tx, Kind::Instance, &self.owner).await?;
                let now = Store::now(&tx).await?;
                ensure!(instance.value.expires_at_ms > now, Rejection::Unavailable);
                instance.value.expires_at_ms = self.deadline(now)?;
                let status = if instance.value.accepting {
                    "live"
                } else {
                    "draining"
                };
                let observation = LeaseObservation {
                    database_now_ms: now,
                    expires_at_ms: instance.value.expires_at_ms,
                    accepting: instance.value.accepting,
                };
                self.store()
                    .save(
                        &tx,
                        &mut instance,
                        status,
                        Some(observation.expires_at_ms),
                        now,
                    )
                    .await?;
                tx.commit().await?;
                Ok(observation)
            })
            .await
    }
}

impl Heartbeat {
    pub async fn renew(&self) -> Result<LeaseObservation> {
        self.engine.heartbeat().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::gateway::shared::{crypto::DeploymentKey, schema, store};
    use sea_orm::TransactionTrait;
    use sha2::Digest;

    #[tokio::test]
    #[ignore = "requires BITROUTER_TEST_DATABASE_URL for a dedicated PostgreSQL test database"]
    async fn dedicated_pool_renews_only_the_original_incarnation() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(15), exercise())
            .await
            .context("dedicated heartbeat regression deadline exceeded")?
    }

    async fn exercise() -> Result<()> {
        let url = std::env::var("BITROUTER_TEST_DATABASE_URL")
            .context("set BITROUTER_TEST_DATABASE_URL; no database fallback is used")?;
        let timeout = Duration::from_millis(350);
        let main_db = store::connect(&url, 1, timeout).await?;
        let heartbeat_db = store::connect(&url, 1, timeout).await?;
        let result = exercise_with_pools(main_db.clone(), heartbeat_db.clone(), timeout).await;
        let main_closed = main_db.close().await;
        let heartbeat_closed = heartbeat_db.close().await;
        result?;
        main_closed?;
        heartbeat_closed?;
        Ok(())
    }

    async fn exercise_with_pools(
        main_db: DatabaseConnection,
        heartbeat_db: DatabaseConnection,
        timeout: Duration,
    ) -> Result<()> {
        let namespace = format!("lease-{}", uuid::Uuid::new_v4());
        let store = Store::new(main_db, &namespace, timeout)?;
        schema::initialize(&store).await?;
        let secret = sha2::Sha256::digest(uuid::Uuid::new_v4().as_bytes()).into();
        let authority = Authority::new(store.clone(), DeploymentKey::from_bytes(secret)?);
        authority.initialize().await?;
        let engine = Engine::join(authority, 30_000).await?;
        let heartbeat = engine.heartbeat_handle(heartbeat_db)?;
        ensure!(heartbeat.engine.owner == engine.owner);
        let initial: Stored<Instance> = store
            .get(Kind::Instance, &engine.owner)
            .await?
            .context("missing initial owner")?;

        // Exhaust the main pool. Renewal must not try to acquire its connection.
        let occupied = store.begin().await?;
        let observed = heartbeat.renew().await?;
        ensure!(observed.accepting);
        ensure!(observed.expires_at_ms - observed.database_now_ms == 30_000);
        ensure!(observed.expires_at_ms >= initial.value.expires_at_ms);
        occupied.rollback().await?;
        let renewed: Stored<Instance> = store
            .get(Kind::Instance, &engine.owner)
            .await?
            .context("missing renewed owner")?;
        ensure!(renewed.value.expires_at_ms == observed.expires_at_ms);
        ensure!(renewed.revision()? == initial.revision()? + 1);

        // A dedicated pool does not bypass authoritative row locking.
        // This fixture lock outlives the renewal deadline. The enclosing test
        // owns its transaction and has a separate finite deadline.
        let blocked = store.database().begin().await?;
        let _: Stored<Instance> = store.lock(&blocked, Kind::Instance, &engine.owner).await?;
        let error = heartbeat
            .renew()
            .await
            .err()
            .context("row lock was bypassed")?;
        ensure!(error.downcast_ref::<Rejection>().is_none());
        blocked.rollback().await?;
        ensure!(heartbeat.renew().await?.accepting);

        engine.drain().await?;
        ensure!(!heartbeat.renew().await?.accepting);

        // Database expiry is terminal even after connectivity/lock recovery.
        let tx = store.begin().await?;
        let mut record: Stored<Instance> = store.lock(&tx, Kind::Instance, &engine.owner).await?;
        let now = Store::now(&tx).await?;
        record.value.expires_at_ms = now - 1;
        store
            .save(&tx, &mut record, "draining", Some(now - 1), now)
            .await?;
        tx.commit().await?;
        for _ in 0..2 {
            let error = heartbeat
                .renew()
                .await
                .err()
                .context("expired owner was renewed")?;
            ensure!(matches!(
                error.downcast_ref::<Rejection>(),
                Some(Rejection::Unavailable)
            ));
        }
        ensure!(engine.heartbeat().await.is_err());
        let instances: Vec<Stored<Instance>> = store
            .due_states(Kind::Instance, &["live", "draining"], i64::MAX, 256)
            .await?;
        ensure!(instances.len() == 1 && instances[0].key() == engine.owner);
        ensure!(instances[0].value.expires_at_ms == now - 1);
        Ok(())
    }
}
