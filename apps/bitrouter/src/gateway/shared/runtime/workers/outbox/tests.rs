use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{Notify, Semaphore};

use super::super::{Worker, Workers};

fn budget(batch: u64) -> Result<Budget> {
    Budget::new(&serde_json::from_value(serde_json::json!({
        "namespace": "outbox-worker-test",
        "deployment_key_env": "UNUSED_OUTBOX_FIXTURE_KEY",
        "transaction_timeout_ms": 1000,
        "worker_batch_size": batch,
        "worker_interval_ms": 100
    }))?)
}

/// Test-only delivery barriers, installed on the real retained Worker/run/pass
/// path. No engine, SQL, coordinator, or task ownership is mocked into production.
pub(in crate::gateway::shared::runtime) struct Probe {
    pub entered: Notify,
    pub release: Semaphore,
    pub passes: AtomicU64,
    pub dropped: AtomicU64,
    pub acknowledgements: AtomicU64,
    pub retries: AtomicU64,
    pending: AtomicU64,
}

impl Probe {
    pub fn publish(&self) {
        self.pending.fetch_add(1, Ordering::SeqCst);
    }
}

struct DeliveryDrop(Arc<Probe>);

impl Drop for DeliveryDrop {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

fn owned(interval: Duration) -> Result<(Worker, Control, Arc<Probe>)> {
    let budget = budget(2)?;
    let probe = Arc::new(Probe {
        entered: Notify::new(),
        release: Semaphore::new(0),
        passes: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
        acknowledgements: AtomicU64::new(0),
        retries: AtomicU64::new(0),
        pending: AtomicU64::new(0),
    });
    let (finish, requests) = watch::channel(None);
    let (reports, report) = watch::channel(Report::default());
    let control = Control { finish, report };
    let stop = CancellationToken::new();
    let task_stop = stop.clone();
    let task_probe = probe.clone();
    let work = async move {
        run(interval, requests, &reports, task_stop, || {
            task_probe.passes.fetch_add(1, Ordering::SeqCst);
            let probe = &task_probe;
            let reports = &reports;
            pass(
                budget,
                reports,
                move || async move {
                    let mut pending = probe.pending.load(Ordering::SeqCst);
                    loop {
                        let Some(next) = pending.checked_sub(1) else {
                            return Ok(None);
                        };
                        match probe.pending.compare_exchange_weak(
                            pending,
                            next,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        ) {
                            Ok(previous) => return Ok(Some(previous)),
                            Err(actual) => pending = actual,
                        }
                    }
                },
                move |_| async move {
                    complete(
                        budget,
                        reports,
                        async {
                            let _delivery = DeliveryDrop(probe.clone());
                            probe.entered.notify_one();
                            probe.release.acquire().await?.forget();
                            Ok(())
                        },
                        async {
                            probe.acknowledgements.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        },
                        async {
                            probe.retries.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        },
                    )
                    .await;
                },
            )
        })
        .await
    };
    Ok((Worker::start("outbox", stop, work), control, probe))
}

fn workers(interval: Duration) -> Result<(Workers, Arc<Probe>)> {
    let (outbox, control, probe) = owned(interval)?;
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    Ok((
        Workers {
            heartbeat: Worker::start("heartbeat", stop, async move {
                stopped.cancelled().await;
                Ok(())
            }),
            publication: None,
            custody: None,
            recovery: None,
            retirement: None,
            outbox: Some(outbox),
            outbox_control: Some(control),
        },
        probe,
    ))
}

pub(in crate::gateway::shared::runtime) async fn install(
    workers: &mut Workers,
    interval: Duration,
) -> Result<Arc<Probe>> {
    if let Some(previous) = &mut workers.outbox {
        previous.force();
        previous.join().await;
    }
    let (worker, control, probe) = owned(interval)?;
    workers.outbox = Some(worker);
    workers.outbox_control = Some(control);
    Ok(probe)
}

pub(in crate::gateway::shared::runtime) fn finish_deadline(workers: &Workers) -> Option<Instant> {
    workers
        .outbox_control
        .as_ref()
        .and_then(|control| *control.finish.borrow())
}

#[test]
fn budgets_cover_the_sequence_without_using_the_instance_lease() -> Result<()> {
    for transaction_ms in [100, 30_000] {
        let config = serde_json::from_value(serde_json::json!({
            "namespace": "outbox-worker-budget",
            "deployment_key_env": "UNUSED_OUTBOX_FIXTURE_KEY",
            "transaction_timeout_ms": transaction_ms,
            "owner_lease_ms": 1000
        }))?;
        let budget = Budget::new(&config)?;
        ensure!(
            Duration::from_millis(budget.lease_ms)
                > budget.claim + budget.delivery + 2 * budget.transaction
        );
        ensure!(budget.lease_ms <= 300_000 && budget.retry_ms <= 300_000);
    }
    Ok(())
}

#[tokio::test]
async fn pass_is_finite_even_when_every_claim_finds_more_work() -> Result<()> {
    let budget = budget(3)?;
    let (reports, _) = watch::channel(Report::default());
    let claims = AtomicU64::new(0);
    let stage = AtomicU64::new(0);
    pass(
        budget,
        &reports,
        || async {
            claims.fetch_add(1, Ordering::SeqCst);
            Ok(Some(()))
        },
        |_| async {
            complete(
                budget,
                &reports,
                async {
                    stage.store(1, Ordering::SeqCst);
                    Ok(())
                },
                async {
                    ensure!(
                        stage.swap(0, Ordering::SeqCst) == 1,
                        "ACK preceded delivery"
                    );
                    Ok(())
                },
                async { anyhow::bail!("successful delivery must not retry") },
            )
            .await;
        },
    )
    .await;
    let report = *reports.borrow();
    ensure!(claims.load(Ordering::SeqCst) == 3);
    ensure!(report.delivered == 3 && report.acknowledged == 3);
    ensure!(report.failures == 0 && report.deferred == 0 && report.backlog.is_none());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn failed_or_timed_out_delivery_never_acks_and_retries_once() -> Result<()> {
    for timed_out in [false, true] {
        let (reports, _) = watch::channel(Report {
            claimed: 1,
            deferred: 1,
            ..Report::default()
        });
        let acknowledgements = AtomicU64::new(0);
        let retries = AtomicU64::new(0);
        complete(
            budget(1)?,
            &reports,
            async {
                if timed_out {
                    std::future::pending::<()>().await;
                }
                anyhow::bail!("receiver failed after a possible commit")
            },
            async {
                acknowledgements.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            async {
                retries.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("lease replaced before retry")
            },
        )
        .await;
        ensure!(acknowledgements.load(Ordering::SeqCst) == 0);
        ensure!(retries.load(Ordering::SeqCst) == 1);
        let report = *reports.borrow();
        ensure!(report.delivered == 0 && report.acknowledged == 0 && report.deferred == 1);
        ensure!(report.failures == 2);
    }
    Ok(())
}

#[tokio::test]
async fn replaced_ack_lease_preserves_delivered_and_does_not_retry() -> Result<()> {
    let (reports, _) = watch::channel(Report {
        claimed: 1,
        deferred: 1,
        ..Report::default()
    });
    let retries = AtomicU64::new(0);
    complete(
        budget(1)?,
        &reports,
        async { Ok(()) },
        async { anyhow::bail!("claim lease replaced") },
        async {
            retries.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    )
    .await;
    let report = *reports.borrow();
    ensure!(report.delivered == 1 && report.acknowledged == 0);
    ensure!(report.deferred == 1 && report.failures == 1);
    ensure!(retries.load(Ordering::SeqCst) == 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn blocked_claim_has_a_finite_budget_and_is_reported_as_unknown() -> Result<()> {
    let (reports, _) = watch::channel(Report::default());
    pass(
        budget(2)?,
        &reports,
        std::future::pending::<Result<Option<()>>>,
        |_| async {},
    )
    .await;
    let report = *reports.borrow();
    ensure!(report.claimed == 0 && report.failures == 1 && report.backlog.is_none());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn cancelled_finish_waiter_retains_root_deadline_and_one_late_pass() -> Result<()> {
    let (mut workers, probe) = workers(Duration::from_secs(60))?;
    workers.stop_early();
    workers.join_early().await;
    ensure!(workers.reports().iter().all(|(_, report)| !report.joined));
    probe.publish();
    let deadline = Instant::now() + Duration::from_secs(5);
    {
        let wait = workers.finish_outbox_until(deadline);
        tokio::pin!(wait);
        tokio::select! {
            result = &mut wait => { result?; anyhow::bail!("held delivery joined"); }
            () = probe.entered.notified() => {}
        }
    }
    ensure!(finish_deadline(&workers) == Some(deadline));
    ensure!(
        workers
            .finish_outbox_until(deadline + Duration::from_secs(1))
            .await
            .is_err()
    );
    ensure!(probe.dropped.load(Ordering::SeqCst) == 0);
    probe.release.add_permits(1);
    workers.finish_outbox_until(deadline).await?;
    workers.finish_outbox_until(deadline).await?;
    workers.stop_heartbeat().await;
    let report = workers.outbox_report().context("outbox report missing")?;
    ensure!(report.late_pass_completed && report.acknowledged == 1 && report.backlog.is_none());
    ensure!(report.deferred == 0 && report.failures == 0);
    ensure!(probe.passes.load(Ordering::SeqCst) == 1);
    ensure!(probe.dropped.load(Ordering::SeqCst) == 1);
    ensure!(
        workers
            .reports()
            .iter()
            .all(|(_, report)| report.is_clean())
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn deadline_expires_without_a_waiter_and_without_ack_or_cleanup() -> Result<()> {
    let (mut workers, probe) = workers(Duration::from_secs(60))?;
    probe.publish();
    let deadline = Instant::now() + Duration::from_millis(50);
    {
        let wait = workers.finish_outbox_until(deadline);
        tokio::pin!(wait);
        tokio::select! {
            result = &mut wait => { result?; anyhow::bail!("held delivery joined"); }
            () = probe.entered.notified() => {}
        }
    }
    tokio::time::advance(Duration::from_millis(50)).await;
    ensure!(workers.wait_any().await == "outbox");
    ensure!(finish_deadline(&workers) == Some(deadline));
    let report = workers.outbox_report().context("outbox report missing")?;
    ensure!(report.claimed == 1 && report.deferred == 1 && report.failures == 1);
    ensure!(!report.late_pass_completed && report.acknowledged == 0);
    ensure!(probe.dropped.load(Ordering::SeqCst) == 1);
    ensure!(probe.acknowledgements.load(Ordering::SeqCst) == 0);
    ensure!(probe.retries.load(Ordering::SeqCst) == 0);
    workers.force();
    workers.join_forced().await;
    ensure!(workers.reports().iter().all(|(_, report)| report.joined));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn finish_clamps_an_active_regular_pass_to_the_original_deadline() -> Result<()> {
    let (mut workers, probe) = workers(Duration::from_millis(100))?;
    probe.publish();
    probe.entered.notified().await;
    let deadline = Instant::now() + Duration::from_millis(50);
    {
        let wait = workers.finish_outbox_until(deadline);
        tokio::pin!(wait);
        ensure!(futures::poll!(&mut wait).is_pending());
    }
    tokio::time::advance(Duration::from_millis(50)).await;
    ensure!(workers.wait_any().await == "outbox");
    ensure!(probe.passes.load(Ordering::SeqCst) == 1);
    ensure!(probe.acknowledgements.load(Ordering::SeqCst) == 0);
    ensure!(
        workers
            .outbox_report()
            .is_some_and(|report| !report.late_pass_completed)
    );
    workers.force();
    workers.join_forced().await;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn final_pass_leaves_remaining_work_durable_instead_of_waiting_for_empty() -> Result<()> {
    let (mut workers, probe) = workers(Duration::from_millis(100))?;
    for _ in 0..5 {
        probe.publish();
    }
    probe.entered.notified().await;
    let deadline = Instant::now() + Duration::from_secs(5);
    {
        let wait = workers.finish_outbox_until(deadline);
        tokio::pin!(wait);
        ensure!(futures::poll!(&mut wait).is_pending());
    }
    // The regular pass has two slots; the single late pass has two more.
    // A fifth item remains available, as if another replica kept publishing.
    probe.release.add_permits(4);
    workers.finish_outbox_until(deadline).await?;
    workers.stop_heartbeat().await;
    ensure!(probe.passes.load(Ordering::SeqCst) == 2);
    ensure!(probe.pending.load(Ordering::SeqCst) == 1);
    let report = workers.outbox_report().context("outbox report missing")?;
    ensure!(report.late_pass_completed && report.acknowledged == 4);
    ensure!(report.deferred == 0 && report.failures == 0 && report.backlog.is_none());
    ensure!(
        workers
            .reports()
            .iter()
            .all(|(_, report)| report.is_clean())
    );
    Ok(())
}
