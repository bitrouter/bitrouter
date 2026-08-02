//! Bounded, idempotent publication of durable trajectory evaluation outbox rows.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use bitrouter_sdk::config::EvalConfig;

use crate::eval::{
    EvalService, admission::SubmissionPrincipal, store::EvalStore, types::AdmissionStatus,
};

use super::{
    evaluation::TRAJECTORY_EVAL_TOPIC,
    store::{MAX_OUTBOX_BATCH_SIZE, OutboxBatchItem, TrajectoryStore},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PublishSummary {
    pub attempted: u64,
    pub delivered: u64,
    pub failed: u64,
}

impl PublishSummary {
    fn merge(&mut self, other: Self) {
        self.attempted = self.attempted.saturating_add(other.attempted);
        self.delivered = self.delivered.saturating_add(other.delivered);
        self.failed = self.failed.saturating_add(other.failed);
    }
}

#[derive(Clone)]
pub struct TrajectoryOutboxPublisher {
    trajectory: TrajectoryStore,
    eval: EvalService,
    batch_size: usize,
    worker: Arc<PublisherWorker>,
}

#[derive(Default)]
struct PublisherWorker {
    active: AtomicBool,
    dirty: AtomicBool,
    #[cfg(test)]
    concurrent_publications: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    max_concurrent_publications: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    spawned_workers: std::sync::atomic::AtomicUsize,
}

impl TrajectoryOutboxPublisher {
    pub(crate) fn new(
        trajectory: TrajectoryStore,
        eval_store: EvalStore,
        eval_config: EvalConfig,
        batch_size: usize,
    ) -> Result<Self> {
        if batch_size == 0 || batch_size > MAX_OUTBOX_BATCH_SIZE {
            anyhow::bail!(
                "trajectory outbox batch size must be between 1 and {MAX_OUTBOX_BATCH_SIZE}"
            )
        }
        Ok(Self {
            trajectory,
            eval: EvalService::new(eval_store, eval_config),
            batch_size,
            worker: Arc::new(PublisherWorker::default()),
        })
    }

    pub(crate) fn kick(&self) {
        self.worker.dirty.store(true, Ordering::Release);
        self.start_worker_if_idle();
    }

    #[cfg(test)]
    pub(crate) fn configured_batch_size(&self) -> usize {
        self.batch_size
    }

    fn start_worker_if_idle(&self) {
        if self
            .worker
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        #[cfg(test)]
        self.worker.spawned_workers.fetch_add(1, Ordering::AcqRel);
        let publisher = self.clone();
        tokio::spawn(async move { publisher.run_worker().await });
    }

    async fn run_worker(self) {
        loop {
            self.worker.dirty.store(false, Ordering::Release);
            #[cfg(test)]
            let concurrent = self
                .worker
                .concurrent_publications
                .fetch_add(1, Ordering::AcqRel)
                + 1;
            #[cfg(test)]
            self.worker
                .max_concurrent_publications
                .fetch_max(concurrent, Ordering::AcqRel);
            let result = self.drain_pending().await;
            #[cfg(test)]
            self.worker
                .concurrent_publications
                .fetch_sub(1, Ordering::AcqRel);
            if result.is_err() {
                tracing::warn!(
                    reason = "drain_failed",
                    "trajectory outbox publication drain failed"
                );
            }
            if self.worker.dirty.swap(false, Ordering::AcqRel) {
                continue;
            }

            self.worker.active.store(false, Ordering::Release);
            if !self.worker.dirty.load(Ordering::Acquire) {
                break;
            }
            if self
                .worker
                .active
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                break;
            }
        }
    }

    pub(crate) async fn publish_batch(&self) -> Result<PublishSummary> {
        let rows = self
            .trajectory
            .pending_outbox_batch(self.batch_size)
            .await?;
        let mut summary = PublishSummary::default();
        for row in rows {
            summary.attempted = summary.attempted.saturating_add(1);
            if self
                .trajectory
                .record_outbox_attempt(&row.owner_user_id, &row.outbox_id)
                .await
                .is_err()
            {
                summary.failed = summary.failed.saturating_add(1);
                tracing::warn!(
                    reason = "attempt_record_failed",
                    "trajectory outbox attempt could not be recorded"
                );
                continue;
            }
            match self.publish_one(&row).await {
                Ok(true) => summary.delivered = summary.delivered.saturating_add(1),
                Ok(false) => summary.failed = summary.failed.saturating_add(1),
                Err(_) => {
                    summary.failed = summary.failed.saturating_add(1);
                    tracing::warn!(
                        reason = "publication_failed",
                        "trajectory outbox item remains pending"
                    );
                }
            }
        }
        Ok(summary)
    }

    pub async fn drain_pending(&self) -> Result<PublishSummary> {
        let pending = self.trajectory.pending_outbox_count().await?;
        let mut summary = PublishSummary::default();
        // Each failed row is moved behind lower-attempt rows. One bounded rotation therefore
        // gives every row present at drain start exactly one chance without amplifying poison.
        let batch_size = u64::try_from(self.batch_size)?;
        let batches = pending.div_ceil(batch_size);
        for _ in 0..batches {
            let batch = self.publish_batch().await?;
            summary.merge(batch);
            if batch.attempted == 0 {
                break;
            }
        }
        Ok(summary)
    }

    pub async fn drain_after_active_worker(&self) -> Result<PublishSummary> {
        while self.worker.active.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        self.drain_pending().await
    }

    #[cfg(test)]
    pub(crate) async fn wait_for_idle(&self) {
        while self.worker.active.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    }

    #[cfg(test)]
    pub(crate) fn worker_stats(&self) -> (usize, usize) {
        (
            self.worker.spawned_workers.load(Ordering::Acquire),
            self.worker
                .max_concurrent_publications
                .load(Ordering::Acquire),
        )
    }

    async fn publish_one(&self, row: &OutboxBatchItem) -> Result<bool> {
        if row.topic != TRAJECTORY_EVAL_TOPIC {
            return Ok(false);
        }
        let Some(envelope) = row
            .payload
            .as_ref()
            .and_then(|payload| payload.evaluation.as_deref())
        else {
            return Ok(false);
        };
        self.eval
            .store()
            .insert_subject_owned(&envelope.subject, &row.owner_user_id)
            .await?;
        let admission = self
            .eval
            .submit(
                envelope.result.clone(),
                SubmissionPrincipal::BuiltinTrajectory {
                    owner_user_id: row.owner_user_id.clone(),
                },
            )
            .await?;
        if admission.status != AdmissionStatus::Admitted {
            return Ok(false);
        }
        let delivered_at = chrono::Utc::now().to_rfc3339();
        self.trajectory
            .mark_outbox_delivered(&row.owner_user_id, &row.outbox_id, &delivered_at)
            .await?;
        Ok(true)
    }
}
