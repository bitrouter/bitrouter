//! Fixed process workers. Each handle remains owned across cancelled waits.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::super::authority::Authority;
use super::super::engine::{Engine, Heartbeat};
use super::super::generations::Generations;
use super::health::Health;
use crate::config::shared_process::Process;

pub(super) mod outbox;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Report {
    pub joined: bool,
    pub failed: bool,
    pub cancelled: bool,
    pub panicked: bool,
}

impl Report {
    pub fn is_clean(self) -> bool {
        self.joined && !self.failed && !self.cancelled && !self.panicked
    }
}

struct Worker {
    name: &'static str,
    task: Option<JoinHandle<Result<()>>>,
    stop: CancellationToken,
    report: Report,
}

impl Worker {
    fn start(
        name: &'static str,
        stop: CancellationToken,
        work: impl Future<Output = Result<()>> + Send + 'static,
    ) -> Self {
        Self {
            name,
            task: Some(tokio::spawn(work)),
            stop,
            report: Report::default(),
        }
    }

    async fn join(&mut self) -> Report {
        if let Some(task) = &mut self.task {
            let result = task.await;
            let report = match &result {
                Ok(Ok(())) => Report {
                    joined: true,
                    ..Report::default()
                },
                Ok(Err(_)) => Report {
                    joined: true,
                    failed: true,
                    ..Report::default()
                },
                Err(error) => Report {
                    joined: true,
                    failed: true,
                    cancelled: error.is_cancelled(),
                    panicked: error.is_panic(),
                },
            };
            // Exclusive borrowing excludes observers until arbitrary returned
            // error/panic payloads have been destroyed outside any owner lock.
            drop(result);
            self.task = None;
            self.report = report;
        }
        self.report
    }

    fn force(&self) {
        self.stop.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
            tracing::error!(
                worker = self.name,
                "shared worker owner dropped before joining"
            );
        }
    }
}

pub(super) struct Workers {
    heartbeat: Worker,
    publication: Option<Worker>,
    custody: Option<Worker>,
    recovery: Option<Worker>,
    retirement: Option<Worker>,
    outbox: Option<Worker>,
    outbox_control: Option<outbox::Control>,
}

impl Workers {
    pub fn new(heartbeat: Heartbeat, health: Arc<Health>, process: &Process) -> Self {
        let stop = CancellationToken::new();
        Self {
            heartbeat: Worker::start(
                "heartbeat",
                stop.clone(),
                heartbeat_loop(
                    heartbeat,
                    health,
                    Duration::from_millis(process.shared.heartbeat_ms),
                    stop,
                ),
            ),
            publication: None,
            custody: None,
            recovery: None,
            retirement: None,
            outbox: None,
            outbox_control: None,
        }
    }

    pub fn start_processing(
        &mut self,
        authority: Authority,
        process: Process,
        generations: Arc<Generations>,
        health: Arc<Health>,
        engine: Engine,
    ) -> Result<()> {
        anyhow::ensure!(
            self.publication.is_none()
                && self.custody.is_none()
                && self.recovery.is_none()
                && self.retirement.is_none()
                && self.outbox.is_none(),
            "shared workers already started"
        );
        // Validate before starting any processing roots. The outbox lease covers
        // its complete transaction sequence, independently of the owner lease.
        let outbox_budget = outbox::Budget::new(&process.shared)?;
        let stop = CancellationToken::new();
        self.publication = Some(Worker::start(
            "publication",
            stop.clone(),
            super::publication::run(
                authority,
                process.clone(),
                generations.clone(),
                health,
                stop,
            ),
        ));
        let stop = CancellationToken::new();
        self.retirement = Some(Worker::start(
            "retirement",
            stop.clone(),
            super::publication::retire(
                generations.clone(),
                Duration::from_millis(process.shared.worker_interval_ms),
                Duration::from_millis(process.shared.drain_timeout_ms),
                stop,
            ),
        ));
        let stop = CancellationToken::new();
        self.custody = Some(Worker::start(
            "custody",
            stop.clone(),
            custody_loop(
                generations,
                process.shared.worker_batch_size,
                Duration::from_millis(process.shared.worker_interval_ms),
                stop,
            ),
        ));
        let stop = CancellationToken::new();
        self.recovery = Some(Worker::start(
            "recovery",
            stop.clone(),
            recovery_loop(
                engine.clone(),
                process.shared.worker_batch_size,
                Duration::from_millis(process.shared.worker_interval_ms),
                stop,
            ),
        ));
        let stop = CancellationToken::new();
        let (control, work) = outbox::start(engine, outbox_budget, stop.clone());
        self.outbox_control = Some(control);
        self.outbox = Some(Worker::start("outbox", stop, work));
        Ok(())
    }

    pub async fn wait_any(&mut self) -> &'static str {
        tokio::select! {
            _ = self.heartbeat.join() => "heartbeat",
            _ = wait_optional(&mut self.publication) => "publication",
            _ = wait_optional(&mut self.custody) => "custody",
            _ = wait_optional(&mut self.recovery) => "recovery",
            _ = wait_optional(&mut self.retirement) => "retirement",
            _ = wait_optional(&mut self.outbox) => "outbox",
        }
    }

    pub fn stop_early(&self) {
        for worker in [
            &self.publication,
            &self.custody,
            &self.recovery,
            &self.retirement,
        ]
        .into_iter()
        .flatten()
        {
            worker.stop.cancel();
        }
    }

    pub async fn join_early(&mut self) {
        tokio::join!(
            join_optional(&mut self.publication),
            join_optional(&mut self.custody),
            join_optional(&mut self.recovery),
            join_optional(&mut self.retirement)
        );
    }

    pub async fn stop_heartbeat(&mut self) {
        self.heartbeat.stop.cancel();
        self.heartbeat.join().await;
    }

    pub async fn finish_outbox_until(&mut self, deadline: Instant) -> Result<()> {
        if let Some(control) = &self.outbox_control {
            // Persist the request before borrowing the join. Cancelling this
            // waiter neither drops the root nor starts another final pass.
            control.finish(deadline)?;
        }
        tokio::time::timeout_at(deadline, join_optional(&mut self.outbox))
            .await
            .context("shared terminal outbox final pass deadline exceeded")?;
        Ok(())
    }

    pub fn outbox_report(&self) -> Option<outbox::Report> {
        self.outbox_control.as_ref().map(outbox::Control::report)
    }

    pub fn force(&self) {
        self.heartbeat.force();
        for worker in [
            &self.publication,
            &self.custody,
            &self.recovery,
            &self.retirement,
            &self.outbox,
        ]
        .into_iter()
        .flatten()
        {
            worker.force();
        }
    }

    pub async fn join_forced(&mut self) {
        tokio::join!(
            self.heartbeat.join(),
            join_optional(&mut self.publication),
            join_optional(&mut self.custody),
            join_optional(&mut self.recovery),
            join_optional(&mut self.retirement),
            join_optional(&mut self.outbox)
        );
    }

    pub fn reports(&self) -> Vec<(&'static str, Report)> {
        std::iter::once(&self.heartbeat)
            .chain(
                [
                    &self.publication,
                    &self.custody,
                    &self.recovery,
                    &self.retirement,
                    &self.outbox,
                ]
                .into_iter()
                .flatten(),
            )
            .map(|worker| (worker.name, worker.report))
            .collect()
    }
}

async fn wait_optional(worker: &mut Option<Worker>) {
    if let Some(worker) = worker {
        worker.join().await;
    } else {
        std::future::pending::<()>().await;
    }
}

async fn join_optional(worker: &mut Option<Worker>) {
    if let Some(worker) = worker {
        worker.join().await;
    }
}

async fn heartbeat_loop(
    heartbeat: Heartbeat,
    health: Arc<Health>,
    interval: Duration,
    stop: CancellationToken,
) -> Result<()> {
    loop {
        let started = Instant::now();
        let deadline = health
            .lease_deadline()
            .context("initial shared lease missing")?;
        let renewal = tokio::time::timeout_at(deadline, heartbeat.renew());
        tokio::pin!(renewal);
        let result = loop {
            health.refresh();
            tokio::select! {
                biased;
                () = stop.cancelled() => return Ok(()),
                result = &mut renewal => break result,
                () = tokio::time::sleep_until(health.next_check()) => {},
            }
        };
        match result {
            Ok(Ok(observation)) => health.lease(started, observation)?,
            Ok(Err(error)) => {
                health.lease_failed();
                if error
                    .downcast_ref::<super::super::engine::Rejection>()
                    .is_some_and(|error| {
                        matches!(error, super::super::engine::Rejection::Unavailable)
                    })
                {
                    health.withdraw();
                    anyhow::bail!("shared owner lease is no longer authoritative");
                }
                tracing::warn!("shared heartbeat acknowledgement unavailable");
            }
            Err(_) => {
                health.withdraw();
                anyhow::bail!("shared owner lease expired without confirmation");
            }
        }
        // A separate freshness tick clears readiness even while publication is
        // waiting on a slow generation; no main-pool acquisition is needed here.
        let next = Instant::now() + interval;
        loop {
            health.refresh();
            let deadline = health
                .lease_deadline()
                .context("shared lease evidence missing")?;
            anyhow::ensure!(Instant::now() < deadline, "shared owner lease expired");
            if Instant::now() >= next {
                break;
            }
            tokio::select! { biased; () = stop.cancelled() => return Ok(()),
            () = tokio::time::sleep_until(next.min(deadline).min(health.next_check())) => {} }
        }
    }
}

async fn custody_loop(
    generations: Arc<Generations>,
    batch: u64,
    interval: Duration,
    stop: CancellationToken,
) -> Result<()> {
    loop {
        tokio::select! { biased; () = stop.cancelled() => return Ok(()), () = tokio::time::sleep(interval) => {} }
        // A false result may mean a busy entry, not an empty queue. Visit a full
        // bounded batch so a live request cannot starve an older completed one.
        let count = generations
            .serving()
            .custody
            .pending()
            .min(usize::try_from(batch)?);
        for _ in 0..count {
            let result = tokio::select! { biased;
                () = stop.cancelled() => return Ok(()),
                result = generations.serving().custody.reconcile_one() => result,
            };
            if result.is_err() {
                tracing::warn!("shared custody reconciliation deferred");
                break;
            }
        }
    }
}

async fn recovery_loop(
    engine: Engine,
    batch: u64,
    interval: Duration,
    stop: CancellationToken,
) -> Result<()> {
    loop {
        tokio::select! { biased; () = stop.cancelled() => return Ok(()), () = tokio::time::sleep(interval) => {} }
        let result = tokio::select! { biased; () = stop.cancelled() => return Ok(()), result = engine.reconcile(batch) => result };
        if result.is_err() {
            tracing::warn!("shared recovery pass deferred");
        }
    }
}
