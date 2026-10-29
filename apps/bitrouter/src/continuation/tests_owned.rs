use super::owned::{Control, Retained};
use super::tests::{finalization_context, pending_publication_count};
use super::*;

use anyhow::ensure;
use tokio::sync::oneshot;
use tokio::time::Instant;

async fn registry() -> Result<ContinuationRegistry> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    ContinuationRegistry::new(
        db,
        ContinuationKeySource::fixed(ContinuationKey::from_bytes(rand::random())?),
        30,
        10,
    )
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(3)
}

fn control(registry: &ContinuationRegistry) -> Result<&Control> {
    registry
        .inner
        .reconciler
        .owned
        .as_ref()
        .context("missing owned continuation control")
}

fn gated_worker(registry: &ContinuationRegistry) -> Result<oneshot::Sender<()>> {
    let (send, receive) = oneshot::channel();
    control(registry)?.start(|| ReconcilerTask {
        cancelled: Arc::new(AtomicBool::new(false)),
        handle: tokio::spawn(async move {
            let _ = receive.await;
        }),
    });
    Ok(send)
}

#[tokio::test]
async fn cancelled_worker_wait_retains_handle_and_restart_gate() -> Result<()> {
    let registry = registry().await?.with_owned_lifecycle()?;
    let release = gated_worker(&registry)?;
    let original = control(&registry)?.task_id();
    ensure!(original.is_some() && registry.owned_worker_healthy());
    {
        let mut waiting = Box::pin(registry.drain_owned_until(deadline()));
        ensure!(futures::poll!(&mut waiting).is_pending());
        ensure!(registry.drain_owned_until(deadline()).await.is_err());
    }
    ensure!(control(&registry)?.task_id() == original);
    ensure!(!registry.owned_worker_healthy());
    registry.start_reconciler();
    ensure!(control(&registry)?.task_id() == original);
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("worker was cancelled"))?;
    let report = registry.drain_owned_until(deadline()).await?;
    ensure!(report.is_clean() && report.worker.started);
    ensure!(control(&registry)?.task_id().is_none());
    registry.start_reconciler();
    ensure!(control(&registry)?.task_id().is_none());
    ensure!(registry.drain_owned_until(deadline()).await? == report);
    ensure!(registry.stop_reconciler().await.is_err());
    Ok(())
}

#[tokio::test]
async fn timeout_and_cancelled_force_keep_worker_ownership() -> Result<()> {
    let registry = registry().await?.with_owned_lifecycle()?;
    let release = gated_worker(&registry)?;
    let original = control(&registry)?.task_id();
    let report = registry.drain_owned_until(Instant::now()).await?;
    ensure!(report.retained == Some(Retained::Deadline) && !report.worker.joined);
    {
        let mut forcing = Box::pin(registry.force_owned());
        ensure!(futures::poll!(&mut forcing).is_pending());
    }
    ensure!(control(&registry)?.task_id() == original);
    let report = registry.force_owned().await?;
    ensure!(report.retained == Some(Retained::Forced));
    ensure!(report.worker.forced && report.worker.cancelled && report.worker.joined);
    ensure!(!report.is_clean());
    ensure!(release.send(()).is_err());
    registry.start_reconciler();
    ensure!(control(&registry)?.task_id().is_none());
    ensure!(registry.drain_owned_until(deadline()).await?.retained == Some(Retained::Forced));
    Ok(())
}

#[tokio::test]
async fn force_broadcast_interrupts_a_graceful_wait_before_joining() -> Result<()> {
    let registry = registry().await?.with_owned_lifecycle()?;
    let release = gated_worker(&registry)?;
    let original = control(&registry)?.task_id();
    ensure!(registry.join_forced_owned().await.is_err());
    let mut waiting = Box::pin(registry.drain_owned_until(deadline()));
    ensure!(futures::poll!(waiting.as_mut()).is_pending());
    registry.request_force_owned()?;
    ensure!(registry.owned_status()?.worker.forced);
    ensure!(registry.owned_status()?.joining && !registry.owned_status()?.is_quiescent());
    ensure!(control(&registry)?.task_id() == original);
    let retained = waiting.await?;
    ensure!(retained.retained == Some(Retained::Forced));
    let joined = registry.join_forced_owned().await?;
    ensure!(joined.worker.joined && joined.worker.forced);
    ensure!(registry.owned_status()?.is_quiescent());
    ensure!(release.send(()).is_err());
    Ok(())
}

struct RestartOnDrop(ContinuationRegistry, Arc<std::sync::atomic::AtomicUsize>);

impl Drop for RestartOnDrop {
    fn drop(&mut self) {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0.start_reconciler();
    }
}

#[tokio::test]
async fn joined_panic_payload_cannot_restart_worker() -> Result<()> {
    let registry = registry().await?.with_owned_lifecycle()?;
    let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let payload = RestartOnDrop(registry.clone(), drops.clone());
    control(&registry)?.start(|| ReconcilerTask {
        cancelled: Arc::new(AtomicBool::new(false)),
        handle: tokio::spawn(async move {
            std::panic::resume_unwind(Box::new(payload));
        }),
    });
    let report = registry.drain_owned_until(deadline()).await?;
    ensure!(report.worker.panicked && report.worker.joined && !report.is_clean());
    ensure!(drops.load(Ordering::SeqCst) == 1);
    ensure!(control(&registry)?.task_id().is_none());
    Ok(())
}

#[tokio::test]
async fn expired_deadline_is_not_refreshed_for_another_batch() -> Result<()> {
    let registry = registry().await?.with_owned_lifecycle()?;
    let release = gated_worker(&registry)?;
    let runtime = ContinuationRuntime::new(registry.clone());
    for index in 0..48 {
        runtime
            .finalize(&finalization_context(
                &format!("owned-batch-{index}"),
                1_000 + index,
                &format!("provider-owned-{index}"),
            ))
            .await?;
    }
    let blocked = registry
        .pending_publication(1_040, None)
        .context("second batch marker missing")?;
    let lock = registry.pending_identity_lock(&blocked.continuation_identity);
    let blocked = lock.lock().await;
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("worker disappeared"))?;
    let cutoff = Instant::now() + Duration::from_millis(500);
    let report =
        tokio::time::timeout(Duration::from_secs(2), registry.drain_owned_until(cutoff)).await??;
    ensure!(report.retained == Some(Retained::Deadline));
    ensure!(report.worker.joined && report.markers.total > 0 && report.markers.total < 48);
    drop(blocked);
    let retained = registry.drain_owned_until(cutoff).await?;
    ensure!(retained.retained == Some(Retained::Deadline));
    ensure!(
        retained.markers == report.markers,
        "expired retry started fresh database work"
    );
    // Explicitly start a later attempt on the same retained owner.
    ensure!(registry.drain_owned_until(deadline()).await?.is_clean());
    ensure!(pending_publication_count(&registry) == 0);
    Ok(())
}

#[tokio::test]
async fn cancelled_database_wait_retains_markers_for_retry() -> Result<()> {
    let registry = registry().await?.with_owned_lifecycle()?;
    let release = gated_worker(&registry)?;
    let runtime = ContinuationRuntime::new(registry.clone());
    let attempt = finalization_context("owned-cancel-db", 1_100, "provider-owned-cancel");
    runtime.finalize(&attempt).await?;
    let marker = registry
        .pending_publication(attempt.delivery_attempt_id, None)
        .context("marker missing")?;
    let lock = registry.pending_identity_lock(&marker.continuation_identity);
    let blocked = lock.lock().await;
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("worker disappeared"))?;
    let cutoff = deadline();
    {
        let mut waiting = Box::pin(registry.drain_owned_until(cutoff));
        tokio::select! {
            result = &mut waiting => anyhow::bail!("blocked reconciliation completed: {result:?}"),
            _ = async {
                while control(&registry).is_ok_and(|control| control.task_id().is_some()) {
                    tokio::task::yield_now().await;
                }
            } => {}
        }
        ensure!(futures::poll!(&mut waiting).is_pending());
    }
    ensure!(pending_publication_count(&registry) == 1);
    ensure!(control(&registry)?.task_id().is_none());
    drop(blocked);
    ensure!(registry.drain_owned_until(cutoff).await?.is_clean());
    Ok(())
}

#[tokio::test]
async fn reserved_unknown_marker_is_never_reported_drained() -> Result<()> {
    let registry = registry().await?.with_owned_lifecycle()?;
    let runtime = ContinuationRuntime::new(registry.clone());
    let attempt = finalization_context("owned-reserved", 1_200, "provider-owned-reserved");
    let (committed, release) = registry.inject_ambiguous_insert_after_commit();
    let finalizing = tokio::spawn({
        let runtime = runtime.clone();
        let attempt = attempt.clone();
        async move { runtime.finalize(&attempt).await }
    });
    committed.await?;
    finalizing.abort();
    let result = finalizing.await;
    ensure!(result.is_err_and(|error| error.is_cancelled()));
    drop(release);
    let report = registry.drain_owned_until(deadline()).await?;
    ensure!(report.retained == Some(Retained::NoProgress) && !report.is_clean());
    ensure!(report.markers.total == 1 && report.markers.reserved == 1);
    let forced = registry.force_owned().await?;
    ensure!(forced.markers == report.markers && forced.retained == Some(Retained::Forced));
    ensure!(pending_publication_count(&registry) == 1);
    // A separate explicit owner-aware compensation removes the test row.
    runtime.rollback(&attempt).await?;
    ensure!(pending_publication_count(&registry) == 0);
    Ok(())
}

#[tokio::test]
async fn backend_failure_retains_markers_and_stopped_worker_for_retry() -> Result<()> {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    let registry = registry().await?.with_owned_lifecycle()?;
    let release = gated_worker(&registry)?;
    let runtime = ContinuationRuntime::new(registry.clone());
    let attempt = finalization_context("owned-backend", 1_300, "provider-owned-backend");
    runtime.finalize(&attempt).await?;
    registry
        .database()
        .execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "ALTER TABLE provider_continuations RENAME TO provider_continuations_hidden".to_owned(),
        ))
        .await?;
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("worker disappeared"))?;
    let report = registry.drain_owned_until(deadline()).await?;
    ensure!(report.retained == Some(Retained::Backend));
    ensure!(report.worker.joined && report.markers.total == 1 && report.markers.reserved == 0);
    registry.start_reconciler();
    ensure!(control(&registry)?.task_id().is_none());
    registry
        .database()
        .execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "ALTER TABLE provider_continuations_hidden RENAME TO provider_continuations".to_owned(),
        ))
        .await?;
    ensure!(registry.drain_owned_until(deadline()).await?.is_clean());
    ensure!(pending_publication_count(&registry) == 0);
    Ok(())
}
