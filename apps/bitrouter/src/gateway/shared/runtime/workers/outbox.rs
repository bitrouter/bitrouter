//! Bounded terminal delivery on the runtime's retained Worker root.
//!
//! Delivery commits Projected + Delivered before ACK. Cancellation never ACKs
//! or compensates: the durable receipt and fenced lease govern the next attempt.

use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::gateway_state::SharedGatewayConfig;
use crate::gateway::shared::engine::Engine;

#[cfg(test)]
pub(in crate::gateway::shared::runtime) mod tests;

/// Process-local attempt counters, not a census of unique durable events.
#[derive(Clone, Copy, Debug, Default)]
pub(in crate::gateway::shared::runtime) struct Report {
    pub claimed: u64,
    pub delivered: u64,
    pub acknowledged: u64,
    /// Observed claims without a confirmed ACK, including interrupted work.
    pub deferred: u64,
    pub failures: u64,
    pub late_pass_completed: bool,
    /// None means Unknown. Neither a bounded pass nor an empty claim samples
    /// the namespace backlog, which other replicas can continue extending.
    pub backlog: Option<u64>,
}

#[derive(Clone, Copy)]
pub(super) struct Budget {
    batch: u64,
    interval: Duration,
    claim: Duration,
    delivery: Duration,
    transaction: Duration,
    lease_ms: u64,
    retry_ms: u64,
}

impl Budget {
    pub fn new(config: &SharedGatewayConfig) -> Result<Self> {
        let transaction_ms = config.transaction_timeout_ms;
        ensure!(
            (100..=30_000).contains(&transaction_ms)
                && (1..=256).contains(&config.worker_batch_size)
                && (100..=30_000).contains(&config.worker_interval_ms),
            "invalid terminal outbox worker bounds"
        );
        // claim: clock + due scan + fenced claim, delivery: two durable phases,
        // ACK/retry: one transaction each, plus one transaction of headroom.
        // Store::retry bounds all abort retries inside its transaction budget.
        // This is deliberately independent of the renewable instance lease.
        let lease_ms = transaction_ms
            .checked_mul(8)
            .context("terminal outbox lease budget overflow")?;
        ensure!(
            (100..=300_000).contains(&lease_ms),
            "terminal outbox lease cannot cover delivery budgets"
        );
        Ok(Self {
            batch: config.worker_batch_size,
            interval: Duration::from_millis(config.worker_interval_ms),
            claim: Duration::from_millis(3 * transaction_ms),
            delivery: Duration::from_millis(2 * transaction_ms),
            transaction: Duration::from_millis(transaction_ms),
            lease_ms,
            retry_ms: config.worker_interval_ms.max(lease_ms),
        })
    }
}

pub(super) struct Control {
    finish: watch::Sender<Option<Instant>>,
    report: watch::Receiver<Report>,
}

impl Control {
    pub fn finish(&self, deadline: Instant) -> Result<()> {
        if let Some(original) = *self.finish.borrow() {
            ensure!(
                original == deadline,
                "terminal outbox deadline cannot change"
            );
            return Ok(());
        }
        self.finish.send_replace(Some(deadline));
        Ok(())
    }

    pub fn report(&self) -> Report {
        *self.report.borrow()
    }
}

pub(super) fn start(
    engine: Engine,
    budget: Budget,
    stop: CancellationToken,
) -> (Control, impl Future<Output = Result<()>> + Send + 'static) {
    let (finish, requests) = watch::channel(None);
    let (reports, report) = watch::channel(Report::default());
    let control = Control { finish, report };
    let work = async move {
        run(budget.interval, requests, &reports, stop, || {
            let engine = &engine;
            let reports = &reports;
            pass(
                budget,
                reports,
                move || async move {
                    let mut claims = engine.claim_outbox(1, budget.lease_ms).await?;
                    ensure!(
                        claims.len() <= 1,
                        "terminal outbox single-claim bound exceeded"
                    );
                    Ok(claims.pop())
                },
                move |claim| async move {
                    complete(
                        budget,
                        reports,
                        engine.deliver(&claim.event),
                        engine.acknowledge_outbox(&claim),
                        engine.retry_outbox(&claim, budget.retry_ms),
                    )
                    .await;
                },
            )
        })
        .await
    };
    (control, work)
}

async fn run<F, Fut>(
    interval: Duration,
    mut finish: watch::Receiver<Option<Instant>>,
    reports: &watch::Sender<Report>,
    stop: CancellationToken,
    mut pass: F,
) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    let work = async {
        loop {
            // Copy before awaiting; no watch guard is retained over a suspension.
            let deadline = *finish.borrow_and_update();
            if let Some(deadline) = deadline {
                ensure!(
                    Instant::now() < deadline,
                    "terminal outbox deadline expired"
                );
                tokio::time::timeout_at(deadline, pass())
                    .await
                    .context("terminal outbox late pass deadline exceeded")?;
                reports.send_modify(|report| report.late_pass_completed = true);
                return Ok(());
            }
            tokio::select! {
                biased;
                changed = finish.changed() => {
                    changed.context("terminal outbox control dropped")?;
                    continue;
                }
                () = tokio::time::sleep(interval) => {}
            }
            let current = pass();
            tokio::pin!(current);
            tokio::select! {
                biased;
                changed = finish.changed() => {
                    changed.context("terminal outbox control dropped")?;
                    let deadline = (*finish.borrow())
                        .context("terminal outbox finish deadline missing")?;
                    ensure!(Instant::now() < deadline, "terminal outbox deadline expired");
                    // Keep the in-flight pass, but its wait and the subsequent
                    // single late pass share D. Never start a new grace budget.
                    tokio::time::timeout_at(deadline, &mut current)
                        .await
                        .context("terminal outbox active pass deadline exceeded")?;
                }
                () = &mut current => {}
            }
        }
    };
    tokio::select! {
        biased;
        () = stop.cancelled() => Ok(()),
        result = work => {
            if result.is_err() {
                failed(reports, "worker");
            }
            result
        }
    }
}

async fn pass<C, Claim, Claimed, Deliver, Delivered>(
    budget: Budget,
    reports: &watch::Sender<Report>,
    mut claim: Claim,
    mut deliver: Deliver,
) where
    Claim: FnMut() -> Claimed,
    Claimed: Future<Output = Result<Option<C>>>,
    Deliver: FnMut(C) -> Delivered,
    Delivered: Future<Output = ()>,
{
    for _ in 0..budget.batch {
        let claimed = match tokio::time::timeout(budget.claim, claim()).await {
            Ok(Ok(Some(claim))) => claim,
            Ok(Ok(None)) => break,
            Ok(Err(_)) | Err(_) => {
                // A timed-out claim may already have committed its lease.
                failed(reports, "claim");
                break;
            }
        };
        reports.send_modify(|report| {
            report.claimed = report.claimed.saturating_add(1);
            report.deferred = report.deferred.saturating_add(1);
        });
        deliver(claimed).await;
    }
}

async fn complete(
    budget: Budget,
    reports: &watch::Sender<Report>,
    delivery: impl Future<Output = Result<()>>,
    acknowledge: impl Future<Output = Result<()>>,
    retry: impl Future<Output = Result<()>>,
) {
    if !matches!(
        tokio::time::timeout(budget.delivery, delivery).await,
        Ok(Ok(()))
    ) {
        failed(reports, "delivery");
        if !matches!(
            tokio::time::timeout(budget.transaction, retry).await,
            Ok(Ok(()))
        ) {
            failed(reports, "retry");
        }
        return;
    }
    reports.send_modify(|report| report.delivered = report.delivered.saturating_add(1));
    if matches!(
        tokio::time::timeout(budget.transaction, acknowledge).await,
        Ok(Ok(()))
    ) {
        reports.send_modify(|report| {
            report.acknowledged = report.acknowledged.saturating_add(1);
            report.deferred = report.deferred.saturating_sub(1);
        });
    } else {
        // ACK uncertainty/lease replacement is not delivery failure. Do not
        // reschedule or clear the lease; the next fenced claimant can replay.
        failed(reports, "acknowledge");
    }
}

fn failed(reports: &watch::Sender<Report>, phase: &'static str) {
    reports.send_modify(|report| report.failures = report.failures.saturating_add(1));
    tracing::warn!(phase, "shared terminal outbox work deferred");
}
