use super::*;

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context as _, ensure};
use tokio::sync::oneshot;

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(3)
}

async fn joined(owner: &OwnedTasks) -> anyhow::Result<Summary> {
    match owner.drain_until(deadline()).await? {
        Drain::Joined(summary) => Ok(summary),
        retained => anyhow::bail!("tasks were retained: {retained:?}"),
    }
}

#[tokio::test]
async fn cancelled_wait_and_timeout_retain_tasks() -> anyhow::Result<()> {
    let owner = Arc::new(OwnedTasks::default());
    let source = owner.acquire()?;
    let (tx, rx) = oneshot::channel();
    owner.spawn_execution(async move {
        let _ = rx.await;
    });
    owner.close_sources();
    ensure!(owner.acquire().is_err());
    ensure!(owner.force().is_err());
    {
        let mut waiting = Box::pin(owner.drain_until(deadline()));
        ensure!(futures::poll!(&mut waiting).is_pending());
        ensure!(owner.drain_until(deadline()).await.is_err());
    }
    drop(source);
    let retained = owner.drain_until(Instant::now()).await?;
    ensure!(
        retained
            == Drain::Retained(Pending {
                sources: 0,
                executions: 1,
                settlements: 0,
            })
    );
    tx.send(())
        .map_err(|_| anyhow::anyhow!("execution was cancelled"))?;
    let summary = joined(&owner).await?;
    ensure!(summary.joined == 1 && summary.is_clean());
    ensure!(joined(&owner).await? == summary);
    Ok(())
}

#[tokio::test]
async fn existing_source_can_register_after_withdrawal() -> anyhow::Result<()> {
    let owner = Arc::new(OwnedTasks::default());
    let source = owner.acquire()?;
    owner.close_sources();
    let mut waiting = Box::pin(owner.drain_until(deadline()));
    ensure!(futures::poll!(&mut waiting).is_pending());
    let child_owner = owner.clone();
    owner.spawn_execution(async move {
        child_owner.spawn_settlement(async {});
        drop(source);
    });
    let Drain::Joined(summary) = waiting.await? else {
        anyhow::bail!("producer wake was lost");
    };
    ensure!(summary.joined == 2 && summary.is_clean());
    Ok(())
}

struct DropChild {
    owner: Arc<OwnedTasks>,
    remaining: usize,
    polls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    started: Option<oneshot::Sender<()>>,
}

impl Future for DropChild {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        if let Some(started) = self.started.take() {
            let _ = started.send(());
        }
        Poll::Pending
    }
}

impl Drop for DropChild {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        if self.remaining > 0 {
            self.owner.spawn_settlement(Self {
                owner: self.owner.clone(),
                remaining: self.remaining - 1,
                polls: self.polls.clone(),
                drops: self.drops.clone(),
                started: None,
            });
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_joins_drop_descendants_without_polling_them() -> anyhow::Result<()> {
    let owner = Arc::new(OwnedTasks::default());
    let polls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let (started, ready) = oneshot::channel();
    owner.spawn_execution(DropChild {
        owner: owner.clone(),
        remaining: 2,
        polls: polls.clone(),
        drops: drops.clone(),
        started: Some(started),
    });
    tokio::time::timeout(Duration::from_secs(3), ready).await??;
    let initial_polls = polls.load(Ordering::SeqCst);
    owner.close_sources();
    owner.force()?;
    let summary = tokio::time::timeout(Duration::from_secs(3), owner.join_forced()).await??;
    ensure!(summary.joined == 3 && summary.cancelled == 1 && summary.discarded == 2);
    ensure!(!summary.is_clean());
    ensure!(polls.load(Ordering::SeqCst) == initial_polls);
    ensure!(drops.load(Ordering::SeqCst) == 3);
    ensure!(owner.pending().executions == 0 && owner.pending().settlements == 0);
    Ok(())
}

#[tokio::test]
async fn cancelled_forced_join_keeps_aborted_handles() -> anyhow::Result<()> {
    let owner = Arc::new(OwnedTasks::default());
    owner.spawn_execution(std::future::pending());
    owner.close_sources();
    owner.force()?;
    {
        let mut waiting = Box::pin(owner.join_forced());
        ensure!(futures::poll!(&mut waiting).is_pending());
    }
    ensure!(owner.pending().executions == 1);
    let summary = owner.join_forced().await?;
    ensure!(summary.cancelled == 1 && summary.joined == 1);
    Ok(())
}

struct PanicPayload(Arc<OwnedTasks>);

impl Drop for PanicPayload {
    fn drop(&mut self) {
        self.0.spawn_settlement(async {});
    }
}

#[tokio::test]
async fn panic_payload_drop_can_register_compensation() -> anyhow::Result<()> {
    let owner = Arc::new(OwnedTasks::default());
    let payload = PanicPayload(owner.clone());
    owner.spawn_execution(async move {
        // Exercise JoinError handling without a panic hook or panic! macro.
        std::panic::resume_unwind(Box::new(payload));
    });
    owner.close_sources();
    let summary = joined(&owner).await?;
    ensure!(summary.panicked == 1 && !summary.is_clean());
    // A panic payload is dropped outside the ownership mutex. Its descendant
    // must be included in the same completed drain, not merely the next wait.
    ensure!(summary.joined == 2);
    Ok(())
}

#[tokio::test]
async fn failures_survive_reaping_and_snapshot_cannot_share_owner() -> anyhow::Result<()> {
    let owner = Arc::new(OwnedTasks::default());
    owner.bind_snapshot()?;
    ensure!(owner.bind_snapshot().is_err());
    let (done, completed) = oneshot::channel();
    let task_owner = owner.clone();
    owner.spawn_settlement(async move {
        task_owner.record_finalizer_failure();
        let _ = done.send(());
    });
    completed.await.context("settlement did not complete")?;
    owner.spawn_execution(async {});
    owner.close_sources();
    let summary = joined(&owner).await?;
    ensure!(summary.joined == 2 && summary.finalizer_failures == 1);
    ensure!(!summary.is_clean());
    Ok(())
}

#[tokio::test]
async fn reaping_more_than_one_batch_keeps_progress() -> anyhow::Result<()> {
    let owner = Arc::new(OwnedTasks::default());
    let source = owner.acquire()?;
    owner.close_sources();
    let mut waiting = Box::pin(owner.drain_until(deadline()));
    ensure!(futures::poll!(&mut waiting).is_pending());
    for _ in 0..(REAP_BATCH * 3) {
        owner.spawn_execution(async {});
        owner.spawn_settlement(async {});
    }
    drop(source);
    let Drain::Joined(summary) = waiting.await? else {
        anyhow::bail!("bounded reaping lost progress");
    };
    ensure!(summary.joined == (REAP_BATCH * 6) as u64 && summary.is_clean());
    Ok(())
}

#[tokio::test]
async fn joined_generation_seals_late_work_without_polling() -> anyhow::Result<()> {
    let owner = Arc::new(OwnedTasks::default());
    owner.close_sources();
    ensure!(joined(&owner).await?.is_clean());
    let polls = Arc::new(AtomicUsize::new(0));
    let task_polls = polls.clone();
    owner.spawn_execution(async move {
        task_polls.fetch_add(1, Ordering::SeqCst);
    });
    let summary = joined(&owner).await?;
    ensure!(summary.late_registrations == 1 && summary.discarded == 1 && !summary.is_clean());
    ensure!(polls.load(Ordering::SeqCst) == 0);
    owner.force()?;
    ensure!(owner.join_forced().await?.forced);
    Ok(())
}
