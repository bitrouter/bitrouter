//! Actual cancelled bootstrap waiter, checked-out transaction and SQLx pool clone.
use super::*;
use sea_orm::TransactionTrait;
use std::{future::Future, task::Poll};

#[tokio::test]
#[ignore = "isolated real native startup and pool custody; select this parent"]
async fn native_recovery_cancelled_startup_keeps_actual_pool_and_transaction_owner() -> Result<()> {
    campaign(&["startup-cancel-pool"]).await
}

pub(super) async fn cancelled_startup_retains_pool_and_transaction(root: &Path) -> Result<()> {
    let stopped = Stopped::produce(root, "startup-cancel-pool").await?;
    let result = async {
        let before = payload(&stopped.bound.fixture.db).await?;
        stopped.bound.fixture.service.shutdown().await?;
        stopped.bound.fixture.db.clone().close().await?;
        // The stopped fixture still contains Service clones; release those
        // explicitly below before expecting the canonical file to be reusable.
        Ok::<_, anyhow::Error>(before)
    }
    .await;
    // Keep providers outside the fallible setup to join them on every path.
    let Stopped {
        generation,
        mut choice,
        spec,
        bound,
    } = stopped;
    drop(bound);
    let checked = async {
        let before = result?;
        wait_file(root).await?;
        let database = crate::db::connect_owned_tenancy(&format!(
            "sqlite://{}",
            root.join("owned.db").display()
        ))
        .await?;
        let db = database.database().clone();
        let transaction = db.begin().await?;
        let pool = db.get_sqlite_connection_pool().clone();
        let config = crate::config::tenancy::TenancyConfig {
            enabled: true,
            wrapping_key_env: Some(KEY.into()),
            ..Default::default()
        };
        let continuations = crate::config::ContinuationConfig::default();
        let mut opening = Box::pin(Service::open_owned_profile(
            database,
            &config,
            &continuations,
        ));
        // One real poll consumes the claim and blocks on the actual single-
        // connection pool. No timer or synthetic pending signal proves this.
        std::future::poll_fn(|cx| match opening.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(Ok(())),
            Poll::Ready(_) => {
                Poll::Ready(Err(anyhow::anyhow!("bootstrap did not await held pool")))
            }
        })
        .await?;
        drop(opening);
        drop(db);
        locked(root)?;
        let mut closing = Box::pin(pool.close());
        std::future::poll_fn(|cx| match closing.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(Ok(())),
            Poll::Ready(()) => {
                Poll::Ready(Err(anyhow::anyhow!("pool closed with live transaction")))
            }
        })
        .await?;
        locked(root)?;
        transaction.rollback().await?;
        tokio::time::timeout(TIMEOUT, closing).await?;
        drop(pool);
        wait_file(root).await?;
        let rebound = spec.connect(root).await?;
        let reopened = async {
            ensure!(payload(&rebound.fixture.db).await? == before);
            ensure!(terminals(&rebound.fixture.db).await?.is_empty());
            let session = fixture::open(
                &rebound,
                &generation,
                &root.join("workspace"),
                Arc::new(HostSpies::default()),
            )
            .await?;
            ensure!(
                session
                    .core()
                    .snapshot()
                    .await
                    .run
                    .as_ref()
                    .is_some_and(|run| run.status == RunStatus::Cancelled)
            );
            ensure!(terminals(&rebound.fixture.db).await?.len() == 2);
            session.close().await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let closed = rebound.fixture.service.shutdown().await;
        let pool_closed = rebound.fixture.db.clone().close().await;
        reopened?;
        closed?;
        rebound.fixture.service.cleanup.ensure_clean()?;
        pool_closed?;
        ensure!(generation.calls() == 0 && choice.calls() == 1);
        Ok(())
    }
    .await;
    choice.release();
    let choice_closed = choice.finish().await;
    let generation_closed = generation.finish().await;
    checked?;
    choice_closed?;
    generation_closed
}
