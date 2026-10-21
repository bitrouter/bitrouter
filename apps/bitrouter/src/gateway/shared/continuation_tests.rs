//! Independent PostgreSQL pools exercise the durable producer barrier.
//! These fixtures do not replace native-row or official-process acceptance.

use std::time::Duration;

use sea_orm::DatabaseConnection;

use super::*;
use crate::gateway::shared::crypto::DeploymentKey;
use crate::gateway::shared::schema;
use crate::gateway::shared::store::{self, Store};

struct Fixture {
    first: DatabaseConnection,
    second: DatabaseConnection,
    authority: Authority,
    peer: Authority,
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn registration_intent_closes_absence_and_cannot_be_reopened() -> Result<()> {
    let fixture = Fixture::new().await?;
    let result = async {
        let context = fixture.context().await?;
        let pending = registration::Registration::new(fixture.authority.clone(), context.key());
        pending.close().await?;
        ensure!(pending.open().await.is_err());
        let repeated = registration::Registration::new(fixture.authority.clone(), context.key());
        let opened = repeated.open().await?;
        let confirmed = repeated.open().await?;
        ensure!(opened.producer_id() == confirmed.producer_id());
        // Discarding a confirmed result exercises durable reconciliation, not a
        // claim that the wire lost a COMMIT acknowledgement.
        drop(confirmed);
        let bind = opened.begin_bind("retained-initializer").await?;
        let mut close = Box::pin(repeated.close());
        ensure!(futures::poll!(close.as_mut()).is_pending());
        drop(close);
        bind.rollback().await?;
        repeated.close().await?;
        ensure!(opened.begin_bind("after-registration-close").await.is_err());
        ensure!(repeated.open().await.is_err());
        let competing = registration::Registration::new(fixture.authority.clone(), context.key());
        let (opening, closing) = tokio::join!(competing.open(), competing.close());
        closing?;
        if let Ok(capability) = opening {
            ensure!(
                capability
                    .begin_bind("after-competing-close")
                    .await
                    .is_err()
            );
        }
        ensure!(competing.open().await.is_err());
        Ok(())
    }
    .await;
    fixture.close(result).await
}

impl Fixture {
    async fn new() -> Result<Self> {
        let url = std::env::var("BITROUTER_TEST_DATABASE_URL")
            .context("BITROUTER_TEST_DATABASE_URL is required")?;
        let first = store::connect(&url, 2, Duration::from_secs(3)).await?;
        let second = store::connect(&url, 2, Duration::from_secs(3)).await?;
        let namespace = format!("native-gate-{}", uuid::Uuid::new_v4());
        let a = Store::new(first.clone(), &namespace, Duration::from_secs(3))?;
        let b = Store::new(second.clone(), &namespace, Duration::from_secs(3))?;
        schema::initialize(&a).await?;
        let key = DeploymentKey::from_bytes(rand::random())?;
        Ok(Self {
            first,
            second,
            authority: Authority::new(a, key.clone()),
            peer: Authority::new(b, key),
        })
    }

    async fn context(&self) -> Result<SharedContinuationDb> {
        let head = self.authority.initialize().await?;
        let key = self.authority.keys(&head).await?.continuation()?;
        SharedContinuationDb::create(self.authority.clone(), key).await
    }

    fn peer(&self, context: &SharedContinuationDb) -> SharedContinuationDb {
        SharedContinuationDb {
            authority: self.peer.clone(),
            key: context.key.clone(),
            producer_id: context.producer_id.clone(),
        }
    }

    async fn close(self, result: Result<()>) -> Result<()> {
        let first = self.first.close().await;
        let second = self.second.close().await;
        result?;
        first?;
        second?;
        Ok(())
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn close_waits_for_accepted_bind_and_prevents_late_bind() -> Result<()> {
    let fixture = Fixture::new().await?;
    let result = async {
        let context = fixture.context().await?;
        let peer = fixture.peer(&context);
        ensure!(context.confirm_closed().await?.is_none());
        let bind = context.begin_bind("candidate").await?;
        let now = SharedContinuationDb::now(&bind).await?;
        // A transaction-local marker stands for the bind's write. The gate
        // must serialize its commit before the post-close evidence snapshot.
        context
            .authority
            .store()
            .create(
                &bind,
                NewRecord {
                    kind: Kind::EpisodeFence,
                    key: "gate-write",
                    status: "fixture",
                    due_at_ms: None,
                    now_ms: now.timestamp_millis(),
                    value: &true,
                },
            )
            .await?;
        let mut closing = Box::pin(peer.close_binds());
        ensure!(
            tokio::time::timeout(Duration::from_millis(80), closing.as_mut())
                .await
                .is_err(),
            "close crossed an uncommitted accepted bind"
        );
        bind.commit().await?;
        let closure = closing.await?;
        ensure!(peer.confirm_closed().await?.is_some());
        let inspection = peer.begin_closed_identity(&closure, "candidate").await?;
        let row: Stored<bool> = peer
            .authority
            .store()
            .read(&inspection, Kind::EpisodeFence, "gate-write")
            .await?
            .context("post-close snapshot missed earlier bind commit")?;
        ensure!(row.value);
        inspection.commit().await?;
        ensure!(context.begin_bind("candidate").await.is_err());
        ensure!(context.begin_bind("new-identity").await.is_err());
        peer.begin_cleanup("candidate").await?.commit().await?;
        let again = context.close_binds().await?;
        context
            .begin_closed_identity(&again, "candidate")
            .await?
            .commit()
            .await?;
        Ok(())
    }
    .await;
    fixture.close(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn cancelled_close_and_old_snapshot_cannot_reopen_producer() -> Result<()> {
    let fixture = Fixture::new().await?;
    let result = async {
        let context = fixture.context().await?;
        let peer = fixture.peer(&context);
        let bind = context.begin_bind("before-cancel").await?;
        // Cancelling a waiter is not positive closure evidence.
        ensure!(
            tokio::time::timeout(Duration::from_millis(80), peer.close_binds())
                .await
                .is_err()
        );
        bind.rollback().await?;
        ensure!(context.confirm_closed().await?.is_none());

        // This transaction has a snapshot but has not yet passed the producer
        // gate. It must not admit work after a concurrent close commits.
        let delayed = context.authority.store().begin().await?;
        context.fence_key(&delayed).await?;
        let closure = peer.close_binds().await?;
        let observed = context.lock_producer(&delayed).await;
        match observed {
            Ok(record) => ensure!(record.value.closed_at_us.is_some()),
            Err(error) => ensure!(
                format!("{error:#}").contains("could not serialize"),
                "unexpected delayed transaction failure: {error:#}"
            ),
        }
        delayed.rollback().await?;
        ensure!(context.begin_bind("delayed").await.is_err());
        context
            .begin_closed_identity(&closure, "delayed")
            .await?
            .commit()
            .await?;
        Ok(())
    }
    .await;
    fixture.close(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn key_namespace_and_microsecond_clock_authority() -> Result<()> {
    let fixture = Fixture::new().await?;
    let result = async {
        let arbitrary = ContinuationKey::from_bytes(rand::random())?;
        ensure!(
            SharedContinuationDb::create(fixture.authority.clone(), arbitrary.clone())
                .await
                .is_err(),
            "native producer bootstrapped a missing authority"
        );
        let context = fixture.context().await?;
        ensure!(
            SharedContinuationDb::create(fixture.authority.clone(), arbitrary)
                .await
                .is_err(),
            "native producer accepted an unrelated continuation key"
        );
        let head = fixture.authority.head().await?;
        fixture
            .authority
            .rotate_correlation(head.key_generation)
            .await?;
        let before = SharedContinuationDb::now(context.database()).await?;
        let bind = context.begin_bind("same-identity").await?;
        let locked = SharedContinuationDb::now(&bind).await?;
        ensure!(locked >= before);
        // clock_timestamp keeps advancing inside an existing transaction.
        tokio::time::sleep(Duration::from_millis(2)).await;
        let later = SharedContinuationDb::now(&bind).await?;
        ensure!(later > locked);
        ensure!(later.timestamp_micros() - locked.timestamp_micros() >= 1_000);

        let another =
            SharedContinuationDb::create(fixture.peer.clone(), context.key.clone()).await?;
        let mut same_identity = Box::pin(another.begin_bind("same-identity"));
        ensure!(
            tokio::time::timeout(Duration::from_millis(80), same_identity.as_mut())
                .await
                .is_err(),
            "absent identity was not fenced"
        );
        // A distinct namespace with the same identity never shares the gate.
        let namespace = format!("native-other-{}", uuid::Uuid::new_v4());
        let other_authority = Authority::new(
            Store::new(fixture.second.clone(), &namespace, Duration::from_secs(3))?,
            DeploymentKey::from_bytes(rand::random())?,
        );
        let head = other_authority.initialize().await?;
        let key = other_authority.keys(&head).await?.continuation()?;
        let other = SharedContinuationDb::create(other_authority, key).await?;
        other.begin_bind("same-identity").await?.commit().await?;
        bind.commit().await?;
        same_identity.await?.commit().await?;

        let closure = context.close_binds().await?;
        ensure!(
            another
                .begin_closed_identity(&closure, "same-identity")
                .await
                .is_err(),
            "another producer accepted closure evidence"
        );
        ensure!(
            other
                .begin_closed_identity(&closure, "same-identity")
                .await
                .is_err(),
            "another namespace accepted closure evidence"
        );
        Ok(())
    }
    .await;
    fixture.close(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn unobserved_close_result_and_post_lock_evidence_are_recoverable() -> Result<()> {
    let fixture = Fixture::new().await?;
    let result = async {
        let context = fixture.context().await?;
        let peer = fixture.peer(&context);
        let (committed, observed) = tokio::sync::oneshot::channel();
        // Model a lost result at the caller boundary, after the DB operation
        // committed but before its caller received the closure. This is not a
        // simulated PostgreSQL wire disconnect.
        let mut unobserved = Box::pin(async {
            let closure = context.close_binds().await?;
            committed
                .send(())
                .map_err(|_| anyhow::anyhow!("fixture observer disappeared"))?;
            std::future::pending::<()>().await;
            Ok::<_, anyhow::Error>(closure)
        });
        tokio::select! {
            result = unobserved.as_mut() => {
                result?;
                anyhow::bail!("fixture unexpectedly delivered close result");
            }
            result = observed => result.context("close did not commit")?,
        }
        drop(unobserved);
        let closure = peer
            .confirm_closed()
            .await?
            .context("committed close was not confirmed")?;
        ensure!(peer.begin_bind("late").await.is_err());

        // Another live producer can still change this identity. A closed
        // producer's evidence snapshot must include that commit after waiting
        // for its advisory lock, rather than retaining an earlier snapshot.
        let writer = SharedContinuationDb::create(fixture.authority.clone(), context.key()).await?;
        // begin_bind lends a Serializable transaction. Like native store callers,
        // this fixture must retry the complete operation after a proven abort;
        // unrelated namespaces may still conflict through PostgreSQL SSI.
        let (inspecting, evidence_key) = writer
            .retry(|| async {
                let evidence_key = format!("foreign-after-close-{}", uuid::Uuid::new_v4());
                let writing = writer
                    .begin_bind("foreign-identity")
                    .await
                    .context("beginning fixture evidence writer")?;
                let mut inspecting =
                    Box::pin(peer.begin_closed_identity(&closure, "foreign-identity"));
                ensure!(
                    tokio::time::timeout(Duration::from_millis(80), inspecting.as_mut())
                        .await
                        .is_err()
                );
                writer
                    .authority
                    .store()
                    .create(
                        &writing,
                        NewRecord {
                            kind: Kind::EpisodeFence,
                            key: &evidence_key,
                            status: "fixture",
                            due_at_ms: None,
                            now_ms: SharedContinuationDb::now(&writing)
                                .await?
                                .timestamp_millis(),
                            value: &true,
                        },
                    )
                    .await?;
                writing
                    .commit()
                    .await
                    .context("committing fixture evidence writer")?;
                Ok((inspecting, evidence_key))
            })
            .await?;
        // The writer has committed. Inspection failures cannot replay it.
        let inspection = inspecting
            .await
            .context("acquiring post-lock fixture inspection")?;
        let evidence: Stored<bool> = peer
            .authority
            .store()
            .read(&inspection, Kind::EpisodeFence, &evidence_key)
            .await?
            .context("post-lock evidence used an earlier snapshot")?;
        ensure!(evidence.value);
        inspection.commit().await?;
        Ok(())
    }
    .await;
    fixture.close(result).await
}
