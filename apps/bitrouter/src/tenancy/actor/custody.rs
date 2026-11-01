//! The owning actor supervises transactions; HTTP owns only its response waiter.

use std::future::{Future, pending, poll_fn};
use std::panic::AssertUnwindSafe;
use std::task::Poll;

use futures::FutureExt;
use tokio::sync::mpsc;

use super::{Control, CredentialRead, ErrorCode, Failure, Instant, Job, Message, Worker};
use crate::tenancy::transaction::{Disposition, Phase};

/// One owning writer serves all queues. Rotate the first poll after each job so
/// sustained safety traffic cannot starve terminal cleanup or ordinary work.
/// Shutdown remains an independent select branch and closes every queue first.
async fn receive_fair<T>(mut queues: [&mut mpsc::Receiver<T>; 3], next: &mut usize) -> Option<T> {
    poll_fn(|cx| {
        let mut closed = 0;
        for offset in 0..queues.len() {
            let index = (*next + offset) % queues.len();
            match queues[index].poll_recv(cx) {
                Poll::Ready(Some(job)) => {
                    *next = (index + 1) % queues.len();
                    return Poll::Ready(Some(job));
                }
                Poll::Ready(None) => closed += 1,
                Poll::Pending => {}
            }
        }
        if closed == queues.len() {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    })
    .await
}

#[derive(Default)]
struct Scheduler {
    next_queue: usize,
    queue_due: bool,
}

#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
enum Turn<T> {
    Stop,
    Escalate,
    Archive,
    Queue(Option<T>),
}

impl Scheduler {
    async fn receive<T>(
        &mut self,
        queues: [&mut mpsc::Receiver<T>; 3],
        stop: Option<&tokio_util::sync::CancellationToken>,
        escalation: Option<Instant>,
        archive: Option<Instant>,
    ) -> Turn<T> {
        let stopped = async {
            match stop {
                Some(stop) => stop.cancelled().await,
                None => pending::<()>().await,
            }
        };
        let queued = receive_fair(queues, &mut self.next_queue);
        let maintenance = async {
            tokio::select! {
                biased;
                () = async { match escalation {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => pending::<()>().await,
                }} => Turn::Escalate,
                () = async { match archive {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => pending::<()>().await,
                }} => Turn::Archive,
            }
        };
        // Maintenance can outlast the other timer's backoff. Give ready queues
        // priority for one turn afterward; empty queues still permit maintenance.
        // Then restore timer priority so sustained traffic cannot starve it.
        let turn = if self.queue_due {
            tokio::select! {
                biased;
                () = stopped => Turn::Stop,
                job = queued => Turn::Queue(job),
                turn = maintenance => turn,
            }
        } else {
            tokio::select! {
                biased;
                () = stopped => Turn::Stop,
                turn = maintenance => turn,
                job = queued => Turn::Queue(job),
            }
        };
        // Shutdown closes the queues before the next receive. Preserve any
        // owed queue turn while switching to that drain.
        if !matches!(&turn, Turn::Stop) {
            self.queue_due = matches!(&turn, Turn::Escalate | Turn::Archive);
        }
        turn
    }
}

#[cfg(test)]
mod fairness_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn slow_maintenance_and_ready_queues_each_make_progress() -> anyhow::Result<()> {
        let (safety, mut a) = mpsc::channel(1);
        let (completion, mut b) = mpsc::channel(1);
        let (ordinary, mut c) = mpsc::channel(1);
        for (queue, value) in [&safety, &completion, &ordinary].into_iter().zip(0..3) {
            queue.send(value).await?;
        }
        let stop = tokio_util::sync::CancellationToken::new();
        let mut scheduler = Scheduler::default();
        let mut escalation = Instant::now();
        let mut archive = Instant::now();
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        // Both maintenance backlogs remain nonempty. Each turn takes longer
        // than either backoff, but less than its five-second operation budget.
        // Previously the two timers alternated without polling any queue.
        for round in 0..12 {
            let turn = scheduler
                .receive(
                    [&mut a, &mut b, &mut c],
                    Some(&stop),
                    Some(escalation),
                    Some(archive),
                )
                .await;
            assert_eq!(
                turn,
                if round % 2 == 0 {
                    Turn::Escalate
                } else {
                    Turn::Archive
                }
            );
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if round % 2 == 0 {
                escalation = Instant::now() + std::time::Duration::from_secs(1);
            } else {
                archive = Instant::now() + std::time::Duration::from_millis(1);
            }
            let expected = round % 3;
            assert_eq!(
                scheduler
                    .receive(
                        [&mut a, &mut b, &mut c],
                        Some(&stop),
                        Some(escalation),
                        Some(archive),
                    )
                    .await,
                Turn::Queue(Some(expected)),
            );
            [&safety, &completion, &ordinary][expected]
                .send(expected)
                .await?;
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn empty_queues_do_not_delay_due_maintenance() -> anyhow::Result<()> {
        let (_safety, mut a) = mpsc::channel::<()>(1);
        let (_completion, mut b) = mpsc::channel(1);
        let (_ordinary, mut c) = mpsc::channel(1);
        let mut scheduler = Scheduler::default();
        let due = Instant::now();
        assert_eq!(
            scheduler
                .receive([&mut a, &mut b, &mut c], None, Some(due), Some(due))
                .await,
            Turn::Escalate,
        );
        let ready_at = Instant::now();
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                scheduler.receive([&mut a, &mut b, &mut c], None, None, Some(due)),
            )
            .await?,
            Turn::Archive,
        );
        assert_eq!(Instant::now(), ready_at);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_preempts_timers_and_queue_opportunity_then_drains() -> anyhow::Result<()> {
        for after_maintenance in [false, true] {
            let (safety, mut a) = mpsc::channel(1);
            let (completion, mut b) = mpsc::channel(1);
            let (ordinary, mut c) = mpsc::channel(1);
            for (queue, value) in [&safety, &completion, &ordinary].into_iter().zip(0..3) {
                queue.send(value).await?;
            }
            let mut scheduler = Scheduler::default();
            let stop = tokio_util::sync::CancellationToken::new();
            let due = Instant::now();
            tokio::time::advance(std::time::Duration::from_secs(1)).await;
            if after_maintenance {
                assert_eq!(
                    scheduler
                        .receive([&mut a, &mut b, &mut c], Some(&stop), Some(due), Some(due))
                        .await,
                    Turn::Escalate,
                );
            }
            stop.cancel();
            assert_eq!(
                scheduler
                    .receive([&mut a, &mut b, &mut c], Some(&stop), Some(due), Some(due))
                    .await,
                Turn::Stop,
            );
            // Match the actor's shutdown transition: close producers, disable
            // the cancellation branch, and drain already accepted jobs.
            a.close();
            b.close();
            c.close();
            for expected in 0..3 {
                assert_eq!(
                    scheduler
                        .receive([&mut a, &mut b, &mut c], None, None, None)
                        .await,
                    Turn::Queue(Some(expected)),
                );
            }
            assert_eq!(
                scheduler
                    .receive([&mut a, &mut b, &mut c], None, None, None)
                    .await,
                Turn::Queue(None),
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn continuously_ready_queues_each_make_progress() -> anyhow::Result<()> {
        let (safety, mut a) = mpsc::channel(1);
        let (completion, mut b) = mpsc::channel(1);
        let (ordinary, mut c) = mpsc::channel(1);
        let mut next = 0;
        safety.send(0).await?;
        completion.send(1).await?;
        ordinary.send(2).await?;
        for round in 0..12 {
            let expected = round % 3;
            assert_eq!(
                receive_fair([&mut a, &mut b, &mut c], &mut next).await,
                Some(expected),
            );
            [&safety, &completion, &ordinary][expected]
                .send(expected)
                .await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn closed_queues_drain_and_pending_queue_still_wakes() -> anyhow::Result<()> {
        let (safety, mut a) = mpsc::channel(1);
        let (completion, mut b) = mpsc::channel(1);
        let (ordinary, mut c) = mpsc::channel(1);
        safety.send(0).await?;
        drop(safety);
        drop(completion);
        let mut next = 0;
        assert_eq!(
            receive_fair([&mut a, &mut b, &mut c], &mut next).await,
            Some(0)
        );
        let send = async move {
            tokio::task::yield_now().await;
            ordinary.send(2).await
        };
        let (received, sent) =
            tokio::join!(receive_fair([&mut a, &mut b, &mut c], &mut next), send);
        sent?;
        assert_eq!(received, Some(2));
        assert_eq!(
            receive_fair([&mut a, &mut b, &mut c], &mut next).await,
            None
        );
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Policy {
    Management,
    #[cfg(all(feature = "core", feature = "harness"))]
    Native,
    CredentialRead,
    Receipt,
    TerminalReceipt,
    #[cfg(feature = "core")]
    OwnedEval,
}
impl Policy {
    fn expired(self, disposition: Disposition, fenced: bool) -> ErrorCode {
        #[cfg(feature = "core")]
        if matches!(self, Self::OwnedEval)
            && !fenced
            && matches!(
                disposition,
                Disposition::NoTransaction | Disposition::RolledBack
            )
        {
            return ErrorCode::CapacityExceeded;
        }
        let _ = (self, disposition, fenced);
        ErrorCode::AuthorityUnavailable
    }
}

impl Worker {
    pub async fn run(
        self,
        mut ordinary: mpsc::Receiver<Job>,
        mut safety: mpsc::Receiver<Job>,
        mut completions: mpsc::Receiver<Job>,
        stop: tokio_util::sync::CancellationToken,
        import: Option<Vec<u8>>,
        initialized: tokio::sync::oneshot::Sender<Result<super::Initialized, Failure>>,
    ) {
        // The owning frame is outside every fallible transaction continuation.
        let _owner = &self.owner;
        #[cfg(all(feature = "core", feature = "harness"))]
        let paid_recovery = crate::tenancy::learning::store::recovery::Bootstrap::default();
        let result = AssertUnwindSafe(async {
            let started = std::time::Instant::now();
            let mut directory_snapshot = None;
            tracing::info!(
                phase = "authority",
                outcome = "started",
                "tenant startup validation"
            );
            let control = self.control();
            let work = async {
                let initial = self.current()?;
                // Validate the entire persisted directory under the existing
                // startup owner, retaining bounded pages rather than all rows.
                let tx = control
                    .begin_repeatable_read(&self.db)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                let validated = async {
                    let (loaded, totals) = crate::tenancy::store::startup::load_header_observed_in(
                        &tx,
                        crate::tenancy::store::startup::Budget {
                            rows: 32,
                            bytes: self
                                .authority
                                .load_budget()
                                .checked_mul(3)
                                .ok_or(ErrorCode::CapacityExceeded)?,
                        },
                    )
                    .await
                    .map_err(|error| self.directory_failure(error))?
                    .ok_or(ErrorCode::AuthorityUnavailable)?;
                    if loaded.header.installation != initial.installation
                        || loaded.header.revision != initial.revision
                    {
                        return Err(ErrorCode::AuthorityUnavailable.into());
                    }
                    directory_snapshot = Some((
                        loaded.header.revision.get(),
                        loaded.header.operation_ids,
                        loaded.header.safety_operation_ids,
                        totals,
                    ));
                    let migrate = loaded.format == crate::tenancy::store::startup::Format::V1
                        && !loaded.header.saturated;
                    if migrate {
                        control.reserve_write(4096, true)?;
                        crate::tenancy::store::startup::upgrade_format_in(&tx, &loaded)
                            .await
                            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                    }
                    Ok::<_, Failure>(migrate)
                }
                .await;
                match validated {
                    Ok(true) => control.commit(tx).await,
                    result => {
                        control
                            .rollback(tx)
                            .await
                            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                        result?;
                        Ok(())
                    }
                }
                .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                tracing::info!(
                    phase = "continuation",
                    outcome = "started",
                    "tenant startup validation"
                );
                let crypto = crate::tenancy::continuation::initialize(
                    &self.db,
                    &initial,
                    &self.resources,
                    import.as_deref(),
                    &control,
                )
                .await
                .map_err(|_| self.unavailable(ErrorCode::AuthorityUnavailable))?;
                let tx = control
                    .begin_repeatable_read(&self.db)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                tracing::info!(
                    phase = "receipts",
                    outcome = "started",
                    "tenant startup validation"
                );
                let inventory =
                    crate::tenancy::receipts::store::inventory(&tx, &initial.installation).await;
                control
                    .rollback(tx)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                let inventory =
                    inventory.map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                let volume = self.owner.as_ref().and_then(|owner| owner.path.parent());
                let free = crate::tenancy::receipts::capacity::free_space(volume)
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                #[cfg(all(feature = "core", feature = "harness"))]
                paid_recovery
                    .inventory(&inventory)
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                self.admission
                    .initialize_receipts(inventory, free)
                    .map_err(|code| self.unavailable(code))?;
                tracing::info!(
                    phase = "learning",
                    outcome = "started",
                    "tenant startup validation"
                );
                self.learning_startup(
                    &control,
                    #[cfg(all(feature = "core", feature = "harness"))]
                    &paid_recovery,
                )
                .await?;
                tracing::info!(
                    phase = "obligations",
                    outcome = "started",
                    "tenant startup validation"
                );
                self.initialize_obligation_schedule(&control).await?;
                Ok(crypto)
            };
            let result = self
                .supervise(work, &control, None, Policy::Management, None)
                .await;
            #[cfg(all(feature = "core", feature = "harness"))]
            let result = match paid_recovery.settle(self.proof.disposition()) {
                Ok(()) => result,
                Err(_) => {
                    self.proof.quarantine();
                    Err(self.unavailable(ErrorCode::StorageUnavailable))
                }
            };
            // The privileged manifest contains plaintext definitions. It is
            // bootstrap work, not actor state retained for the serving lifetime.
            drop(import);
            let result = result.and_then(|crypto| {
                // Commit has been acknowledged. Install only the narrow HTTP
                // capability before publishing readiness; markers have no sender.
                self.http_continuation
                    .initialize(crypto.as_ref().map(|key| key.http()))?;
                Ok(crypto)
            });
            let result = result.and_then(|crypto| {
                if let Some(startup) = &self.startup {
                    startup
                        .publish(&self.proof)
                        .map_err(|_| self.unavailable(ErrorCode::AuthorityUnavailable))?;
                }
                // Bootstrap/full-H settlement and receipt accounting are complete.
                // No maintenance or request can contend with this initial copy.
                // The ACK carries numeric data, never an Admission guard or owner.
                #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
                let capacity = if self.observe_capacity {
                    Some(
                        self.admission
                            .observed_capacity()
                            .map_err(|_| self.unavailable(ErrorCode::AuthorityUnavailable))?,
                    )
                } else {
                    None
                };
                Ok(super::Initialized {
                    crypto,
                    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
                    capacity,
                })
            });
            if result.is_ok() {
                // All startup phases and their custody settlement succeeded.
                // Counts describe the earlier validated authority snapshot only.
                if let Some((authority_revision, operations, safety_operations, totals)) =
                    directory_snapshot
                {
                    tracing::info!(
                        outcome = "complete",
                        as_of = "validated_authority_snapshot",
                        authority_revision,
                        operations,
                        safety_operations,
                        tenants = totals.tenants,
                        principals = totals.principals,
                        bindings = totals.bindings,
                        credentials = totals.credentials,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "tenant startup validation completed"
                    );
                }
            } else if let Err(error) = &result {
                tracing::warn!(outcome = "failed", code = ?error.0.code,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "tenant startup validation did not complete");
            }
            let ready = result.is_ok();
            // Cancellation drops only the waiter. This actor still owns BEGIN,
            // commit/rollback and its lease until supervision settles.
            let delivered = initialized.send(result).is_ok();
            if ready && delivered {
                self.run_loop(&mut ordinary, &mut safety, &mut completions, stop)
                    .await;
            }
        })
        .catch_unwind()
        .await;
        if result.is_err() {
            tracing::error!("tenant authority worker unwound; owner retained");
        }
        if result.is_err() || self.proof.ensure_clean().is_err() {
            self.proof.quarantine();
            self.quarantine(&mut ordinary, &mut safety, &mut completions)
                .await;
        }
    }

    async fn run_loop(
        &self,
        ordinary: &mut mpsc::Receiver<Job>,
        safety: &mut mpsc::Receiver<Job>,
        completions: &mut mpsc::Receiver<Job>,
        stop: tokio_util::sync::CancellationToken,
    ) {
        let mut draining = false;
        let mut scheduler = Scheduler::default();
        let mut maintenance_after = Instant::now();
        let mut archive_after = Instant::now();
        let mut archive_cursor = None;
        loop {
            let escalation = match self.obligation_deadline() {
                Ok(due) => due.map(|due| due.max(maintenance_after)),
                Err(_) => {
                    self.quarantine(ordinary, safety, completions).await;
                    None
                }
            };
            let archive = match self.admission.archive_needed() {
                Ok(true) if !draining => Some(archive_after),
                Ok(_) => None,
                Err(_) => {
                    self.quarantine(ordinary, safety, completions).await;
                    None
                }
            };
            let job = match scheduler
                .receive(
                    [safety, completions, ordinary],
                    (!draining).then_some(&stop),
                    escalation,
                    archive,
                )
                .await
            {
                Turn::Stop => {
                    ordinary.close();
                    safety.close();
                    completions.close();
                    draining = true;
                    continue;
                }
                Turn::Escalate => {
                    if let Some(due) = escalation {
                        tracing::debug!(
                            component = "obligations",
                            due_lateness_ms =
                                Instant::now().saturating_duration_since(due).as_millis() as u64,
                            "tenant maintenance turn"
                        );
                    }
                    let control = self.control();
                    let result = self
                        .supervise(
                            self.escalate_obligations(chrono::Utc::now(), &control),
                            &control,
                            Instant::now() + std::time::Duration::from_secs(5),
                            Policy::CredentialRead,
                            None,
                        )
                        .await;
                    maintenance_after = Instant::now() + std::time::Duration::from_secs(1);
                    if self.proof.disposition() == Disposition::Ambiguous {
                        self.quarantine(ordinary, safety, completions).await;
                    }
                    if let Err(error) = result {
                        tracing::warn!(code = ?error.0.code, "obligation maintenance deferred");
                    }
                    continue;
                }
                Turn::Archive => {
                    if let Some(due) = archive {
                        tracing::debug!(
                            component = "archive",
                            due_lateness_ms =
                                Instant::now().saturating_duration_since(due).as_millis() as u64,
                            "tenant maintenance turn"
                        );
                    }
                    let control = self.control();
                    let result = self
                        .supervise(
                            self.archive_history(archive_cursor.as_ref(), &control),
                            &control,
                            Instant::now() + std::time::Duration::from_secs(5),
                            Policy::CredentialRead,
                            None,
                        )
                        .await;
                    if self.proof.disposition() == Disposition::Ambiguous {
                        self.quarantine(ordinary, safety, completions).await;
                    }
                    let delay = match result {
                        Ok(progress) => {
                            tracing::debug!(
                                component = "archive",
                                archived = progress.archived,
                                pass_complete = progress.done,
                                "tenant maintenance progress"
                            );
                            archive_cursor = if progress.done { None } else { progress.after };
                            if progress.done && !progress.archived {
                                // A complete pass found no more eligible history.
                                // Keep old unknown/live owners intact; don't spin.
                                std::time::Duration::from_secs(30)
                            } else {
                                // One bounded maintenance turn, then let queues
                                // progress before inspecting another key page.
                                std::time::Duration::from_millis(1)
                            }
                        }
                        Err(error) => {
                            tracing::warn!(code = ?error.0.code, "receipt archive maintenance deferred");
                            std::time::Duration::from_secs(5)
                        }
                    };
                    archive_after = Instant::now() + delay;
                    continue;
                }
                Turn::Queue(Some(job)) => job,
                Turn::Queue(None) => break,
            };
            let control = self.control();
            match job {
                #[cfg(test)]
                Job::Archive { deadline, reply } => {
                    let result = self
                        .supervise(
                            self.archive_history(None, &control),
                            &control,
                            deadline.unwrap_or_else(|| {
                                Instant::now() + std::time::Duration::from_secs(5)
                            }),
                            Policy::CredentialRead,
                            None,
                        )
                        .await
                        .map(|progress| progress.archived);
                    if reply.send(result).is_err() {
                        tracing::debug!("archive test waiter disconnected");
                    }
                    if self.proof.disposition() == Disposition::Ambiguous {
                        self.quarantine(ordinary, safety, completions).await;
                    }
                }
                #[cfg(test)]
                Job::Escalate {
                    now,
                    deadline,
                    reply,
                } => {
                    let result = self
                        .supervise(
                            self.escalate_obligations(now, &control),
                            &control,
                            deadline.unwrap_or_else(|| {
                                Instant::now() + std::time::Duration::from_secs(5)
                            }),
                            Policy::CredentialRead,
                            None,
                        )
                        .await;
                    if reply.send(result).is_err() {
                        tracing::debug!("escalation test waiter disconnected");
                    }
                    if self.proof.disposition() == Disposition::Ambiguous {
                        self.quarantine(ordinary, safety, completions).await;
                    }
                }
                Job::UnknownCount(job) => {
                    let result = self
                        .supervise(
                            self.read_unknown_count(&job, &control),
                            &control,
                            job.deadline,
                            Policy::CredentialRead,
                            None,
                        )
                        .await;
                    let super::UnknownCount { reply, permit, .. } = job;
                    if reply.send(result).is_err() {
                        tracing::debug!("cold tenant count waiter disconnected");
                    }
                    if self.proof.disposition() == Disposition::Ambiguous {
                        self.quarantine(ordinary, safety, completions).await;
                    }
                    drop(permit);
                }
                Job::OAuth(job) => {
                    let result = self
                        .supervise(
                            self.execute_oauth(&job, &control),
                            &control,
                            job.exchange
                                .owner
                                .cleanup_deadline()
                                .min(Instant::now() + std::time::Duration::from_secs(30)),
                            Policy::Receipt,
                            None,
                        )
                        .await;
                    if self.proof.disposition() == Disposition::Ambiguous {
                        job.exchange.owner.fail(ErrorCode::StorageUnavailable);
                        job.progress.finish(Err(ErrorCode::StorageUnavailable));
                        // Keep the exact pending replacement/exchange outside caught work.
                        self.quarantine(ordinary, safety, completions).await;
                    }
                    if result.is_err()
                        && job.operation == crate::tenancy::oauth::exchange::Operation::Release
                    {
                        job.exchange.owner.fail(ErrorCode::StorageUnavailable);
                    }
                    // A resumed waiter can immediately submit Commit/Release.
                    // Clear the exchange's in-flight marker before publishing
                    // completion, otherwise another runtime thread races it.
                    job.exchange.completed();
                    job.progress
                        .finish(result.map_err(|failure| failure.0.code));
                }
                Job::ReceiptRead(read) => {
                    let result = self
                        .supervise(
                            self.read_receipt(&read, &control),
                            &control,
                            read.deadline,
                            Policy::CredentialRead,
                            None,
                        )
                        .await;
                    let crate::tenancy::receipts::read::Read {
                        reply,
                        _permit,
                        caller,
                        request,
                        ..
                    } = read;
                    let _ = reply.send(result);
                    if self.proof.disposition() == Disposition::Ambiguous {
                        self.quarantine(ordinary, safety, completions).await;
                    }
                    drop((_permit, caller, request));
                }
                Job::Receipt(owner) => {
                    owner.observation.dequeue();
                    let result = self
                        .supervise(
                            self.execute_receipt(&owner, &control),
                            &control,
                            owner
                                .cleanup_deadline()
                                .min(Instant::now() + std::time::Duration::from_secs(30)),
                            Policy::TerminalReceipt,
                            None,
                        )
                        .await;
                    owner.observation.returned(result.is_err());
                    if result.is_err() && !owner.durable() {
                        owner.storage_failed(matches!(
                            self.proof.disposition(),
                            Disposition::NoTransaction | Disposition::RolledBack
                        ));
                    }
                    if self.proof.disposition() == Disposition::Ambiguous {
                        self.quarantine(ordinary, safety, completions).await;
                    }
                    // The registry retains the immutable candidate and its reservation
                    // after a confirmed rollback; an ambiguous actor never releases either.
                    drop(owner);
                }
                #[cfg(all(feature = "core", feature = "harness"))]
                Job::Learner(message) => {
                    let result = self
                        .supervise(
                            self.execute_learner(&message, &control),
                            &control,
                            message.deadline,
                            Policy::OwnedEval,
                            None,
                        )
                        .await;
                    // Keep the original work/read/learning owners outside the
                    // supervised continuation until consuming COMMIT is known.
                    if self.proof.disposition() == Disposition::Ambiguous {
                        self.quarantine(ordinary, safety, completions).await;
                    }
                    let crate::tenancy::learner::queue::Job {
                        work, read, reply, ..
                    } = *message;
                    if reply.send(result).is_err() {
                        tracing::debug!("owned learner waiter disconnected");
                    }
                    drop((work, read));
                }
                #[cfg(all(feature = "core", feature = "harness"))]
                Job::ExperimentRead(message) => {
                    let result = self
                        .supervise(
                            self.read_experiment(&message, &control),
                            &control,
                            message.deadline,
                            Policy::OwnedEval,
                            None,
                        )
                        .await;
                    let crate::tenancy::experiment::queue::ReadJob {
                        binding,
                        profile,
                        root,
                        reply,
                        permit,
                        command,
                        ..
                    } = *message;
                    if reply.send(result).is_err() {
                        tracing::debug!("native cohort completed without its response receiver");
                    }
                    if self.proof.disposition() == Disposition::Ambiguous {
                        self.quarantine(ordinary, safety, completions).await;
                    }
                    drop((binding, profile, root, permit, command));
                }
                #[cfg(all(feature = "core", feature = "harness"))]
                Job::Experiment(message) => {
                    let result = self
                        .supervise(
                            self.execute_experiment(&message, &control),
                            &control,
                            message.deadline,
                            Policy::OwnedEval,
                            None,
                        )
                        .await;
                    let crate::tenancy::experiment::queue::Job {
                        execution,
                        origin,
                        profile,
                        permit,
                        reply,
                        ..
                    } = *message;
                    if reply.send(result).is_err() {
                        tracing::debug!(
                            "native experiment enrollment completed without a response receiver"
                        );
                    }
                    if self.proof.disposition() == Disposition::Ambiguous {
                        self.quarantine(ordinary, safety, completions).await;
                    }
                    drop((execution, origin, profile, permit));
                }
                #[cfg(all(feature = "core", feature = "harness"))]
                Job::Learning(message) => {
                    let result = self
                        .supervise(
                            self.execute_learning(&message, &control),
                            &control,
                            message.deadline,
                            Policy::OwnedEval,
                            None,
                        )
                        .await;
                    message.work.settled(self.proof.disposition());
                    let crate::tenancy::learning::queue::Job {
                        work,
                        reply,
                        _read: read,
                        ..
                    } = *message;
                    let _ = reply.send(result);
                    if self.proof.disposition() == Disposition::Ambiguous {
                        self.quarantine(ordinary, safety, completions).await;
                    }
                    drop((work, read));
                }
                #[cfg(all(feature = "core", feature = "harness"))]
                Job::Capture(message) => {
                    let result = self
                        .supervise(
                            self.execute_capture(&message, &control),
                            &control,
                            message.deadline,
                            Policy::OwnedEval,
                            None,
                        )
                        .await;
                    let crate::tenancy::capture::queue::Job {
                        binding,
                        request,
                        reply,
                        _read: read,
                        ..
                    } = *message;
                    if reply
                        .send(crate::tenancy::capture::queue::Outcome {
                            result,
                            disposition: self.proof.disposition(),
                        })
                        .is_err()
                    {
                        tracing::debug!(
                            "capture operation completed without its response receiver"
                        );
                    }
                    if self.proof.disposition() == Disposition::Ambiguous {
                        self.quarantine(ordinary, safety, completions).await;
                    }
                    drop((binding, request, read));
                }
                #[cfg(all(feature = "core", feature = "harness"))]
                Job::Native(message) => {
                    let result = self
                        .supervise(
                            self.execute_native_journal(&message, &control),
                            &control,
                            message.deadline,
                            Policy::Native,
                            None,
                        )
                        .await;
                    let crate::tenancy::session::Job {
                        request,
                        reply,
                        _read: read,
                        ..
                    } = *message;
                    if reply
                        .send(crate::tenancy::session::Outcome {
                            result,
                            disposition: self.proof.disposition(),
                        })
                        .is_err()
                    {
                        tracing::debug!("native journal completed without a response receiver");
                    }
                    if self.proof.disposition() == Disposition::Ambiguous {
                        self.quarantine(ordinary, safety, completions).await;
                    }
                    drop((request, read));
                }
                Job::Command(message) => {
                    let Message {
                        caller,
                        command,
                        reply,
                    } = *message;
                    let operation = Some(command.operation_id().clone());
                    let result = self
                        .supervise(
                            self.execute(&caller, &command, &control),
                            &control,
                            Instant::now() + crate::tenancy::MANAGEMENT_TIMEOUT,
                            Policy::Management,
                            operation,
                        )
                        .await;
                    if reply.send(result).is_err() {
                        tracing::debug!("tenancy command completed without a response receiver");
                    }
                    if self.proof.disposition() == Disposition::Ambiguous {
                        self.quarantine(ordinary, safety, completions).await;
                    }
                    // Keep active command custody outside the caught work future.
                    drop((caller, command));
                }
                Job::AuthorityRead(message) => {
                    let result = self
                        .supervise(
                            self.read_authority(&message, &control),
                            &control,
                            message.deadline,
                            Policy::CredentialRead,
                            None,
                        )
                        .await;
                    let super::AuthorityRead {
                        subject,
                        permit,
                        reply,
                        ..
                    } = message;
                    let _ = reply.send(result);
                    if self.proof.disposition() == Disposition::Ambiguous {
                        self.quarantine(ordinary, safety, completions).await;
                    }
                    drop((subject, permit));
                }
                Job::Credential(CredentialRead {
                    caller,
                    authority,
                    id,
                    deadline,
                    permit,
                    reply,
                }) => {
                    let work = async {
                        self.authority
                            .read()
                            .map_err(Failure::from)?
                            .check(&authority, |view| {
                                view.facts()
                                    .principal(&caller, chrono::Utc::now())
                                    .map(|_| ())
                            })
                            .map_err(Failure::from)?;
                        crate::tenancy::store::credential_view(&self.db, id.as_str(), &control)
                            .await
                            .map_err(|error| {
                                if error.is::<crate::tenancy::transaction::ReadExpired>() {
                                    ErrorCode::AuthorityUnavailable.into()
                                } else {
                                    self.unavailable(ErrorCode::StorageUnavailable)
                                }
                            })
                    };
                    let result = self
                        .supervise(work, &control, deadline, Policy::CredentialRead, None)
                        .await;
                    if reply.send(result).is_err() {
                        tracing::debug!(
                            "tenancy credential read completed without a response receiver"
                        );
                    }
                    if self.proof.disposition() == Disposition::Ambiguous {
                        self.quarantine(ordinary, safety, completions).await;
                    }
                    // An abandoned HTTP waiter cannot release the DB-read capacity.
                    drop((caller, authority, id, permit));
                }
                Job::Continuation(message) => {
                    let policy = if message.work.read() {
                        Policy::CredentialRead
                    } else {
                        Policy::Management
                    };
                    let result = self
                        .supervise(
                            self.execute_continuation(&message, &control),
                            &control,
                            message.deadline,
                            policy,
                            None,
                        )
                        .await;
                    if result.is_err()
                        && matches!(
                            self.proof.disposition(),
                            Disposition::NoTransaction | Disposition::RolledBack
                        )
                        && let Some(ticket) = message.work.preparation()
                    {
                        self.http_continuation.release(ticket);
                    }
                    let crate::tenancy::continuation::http::Job {
                        origin,
                        deadline: _,
                        work,
                        reply,
                    } = *message;
                    if reply.send(result).is_err()
                        && let Some(ticket) = work.ticket()
                    {
                        self.http_continuation.abandon(ticket);
                    }
                    if self.proof.disposition() == Disposition::Ambiguous {
                        // Keep ticket, receipt marker, lookup permit, and proof
                        // outside the caught future for the whole quarantine.
                        self.quarantine(ordinary, safety, completions).await;
                    }
                    drop((origin, work));
                }
                #[cfg(feature = "core")]
                Job::Eval(message) => {
                    let result = self
                        .supervise(
                            self.execute_eval(&message, &control),
                            &control,
                            message.deadline,
                            Policy::OwnedEval,
                            None,
                        )
                        .await;
                    if self.proof.disposition() == Disposition::Ambiguous {
                        let crate::tenancy::eval::Job {
                            reply,
                            identity,
                            request,
                            lease,
                            ..
                        } = *message;
                        if reply
                            .send(Err(ErrorCode::AuthorityUnavailable.into()))
                            .is_err()
                        {
                            tracing::debug!("ambiguous evaluation had no response receiver");
                        }
                        // A failure response must not release capacity while cleanup
                        // is unconfirmed. Keep all active custody outside unwind.
                        self.quarantine(ordinary, safety, completions).await;
                        drop((identity, request, lease, result));
                    } else {
                        let crate::tenancy::eval::Job { reply, lease, .. } = *message;
                        if reply
                            .send(result.map(|body| crate::tenancy::eval::Reply { body, lease }))
                            .is_err()
                        {
                            tracing::debug!("evaluation completed without a response receiver");
                        }
                    }
                }
                #[cfg(feature = "core")]
                Job::TrajectoryBegin(message) => {
                    let result = self
                        .supervise(
                            self.execute_trajectory_begin(&message, &control),
                            &control,
                            message.deadline,
                            Policy::OwnedEval,
                            None,
                        )
                        .await;
                    if result.is_err() {
                        if matches!(
                            self.proof.disposition(),
                            Disposition::NoTransaction | Disposition::RolledBack
                        ) {
                            message.observation.no_start(self.proof.disposition());
                        } else {
                            message.observation.fail();
                        }
                    }
                    let crate::tenancy::trajectory::Begin {
                        reply,
                        execution,
                        observation,
                        input,
                        evidence,
                        ..
                    } = *message;
                    if reply.send(result).is_err() {
                        tracing::debug!("trajectory begin completed without a response receiver");
                    }
                    if self.proof.disposition() == Disposition::Ambiguous {
                        self.quarantine(ordinary, safety, completions).await;
                    }
                    drop((execution, observation, input, evidence));
                }
                #[cfg(feature = "core")]
                Job::TrajectoryTerminal(message) => {
                    let result = self
                        .supervise(
                            self.execute_trajectory_terminal(&message, &control),
                            &control,
                            message.deadline,
                            Policy::OwnedEval,
                            None,
                        )
                        .await;
                    message.observation.transaction(self.proof.disposition());
                    if result.is_err() {
                        message.observation.fail();
                    }
                    let crate::tenancy::trajectory::Terminal {
                        reply,
                        execution,
                        observation,
                        start,
                        facts,
                        ..
                    } = *message;
                    if reply.send(result).is_err() {
                        tracing::debug!(
                            "trajectory terminal completed without a response receiver"
                        );
                    }
                    if self.proof.disposition() == Disposition::Ambiguous {
                        self.quarantine(ordinary, safety, completions).await;
                    }
                    drop((execution, observation, start, facts));
                }
                #[cfg(feature = "core")]
                Job::Settlement(message) => {
                    let result = self
                        .supervise(
                            self.execute_settlement(&message, &control),
                            &control,
                            message.deadline,
                            Policy::OwnedEval,
                            None,
                        )
                        .await;
                    message.observation.transaction(self.proof.disposition());
                    if result.is_err() {
                        message.observation.fail();
                    }
                    let crate::tenancy::eval::Settlement {
                        reply,
                        execution,
                        subject,
                        observation,
                        ..
                    } = *message;
                    if reply.send(result).is_err() {
                        tracing::debug!(
                            "evaluation observation completed without a response receiver"
                        );
                    }
                    if self.proof.disposition() == Disposition::Ambiguous {
                        self.quarantine(ordinary, safety, completions).await;
                    }
                    drop((execution, subject, observation));
                }
            }
        }
    }

    /// Bootstrap and serving consumers share the existing transaction supervision.
    async fn supervise<T>(
        &self,
        work: impl Future<Output = Result<T, Failure>>,
        control: &Control,
        deadline: impl Into<Option<Instant>>,
        policy: Policy,
        operation: Option<bitrouter_ai::tenancy::identity::OperationId>,
    ) -> Result<T, Failure> {
        // Reject an already-expired queued job before polling any database
        // future. A selected timer is the watchdog for work admitted here,
        // not the admission check for an elapsed absolute deadline.
        let deadline = deadline.into();
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(Failure::new(
                policy.expired(
                    Disposition::NoTransaction,
                    !self.live.0.load(std::sync::atomic::Ordering::Acquire),
                ),
                operation,
            ));
        }
        let work = AssertUnwindSafe(work).catch_unwind();
        tokio::pin!(work);
        let mut progress = control.progress();
        let mut expired = false;
        let mut cleanup_fenced = false;
        let mut observing = true;
        loop {
            let cleanup = if cleanup_fenced {
                None
            } else {
                progress.borrow().cleanup_deadline
            };
            tokio::select! {
                biased;
                () = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => pending::<()>().await,
                    }
                }, if !expired => {
                    expired = true;
                    if control.phase() == Phase::Preparing {
                        // No transaction exists and no detached continuation may commit.
                        return Err(Failure::new(policy.expired(Disposition::NoTransaction, !self.live.0.load(std::sync::atomic::Ordering::Acquire)), operation));
                    }
                    let keep_write = match policy {
                        Policy::Management => !control.preparing_read(),
                        Policy::Receipt | Policy::TerminalReceipt => true,
                        Policy::CredentialRead => false,
                        #[cfg(all(feature = "core", feature = "harness"))]
                        Policy::Native => matches!(control.phase(), Phase::Committing)
                            || self.proof.disposition() == Disposition::Committed,
                        #[cfg(feature = "core")]
                        Policy::OwnedEval => matches!(control.phase(), Phase::Committing)
                            || self.proof.disposition() == Disposition::Committed,
                    };
                    if keep_write {
                        // Preserve the existing late durable management operation.
                        self.proof.fenced();
                    } else {
                        // Do not drop BEGIN/SELECT. Finish the await, then roll back.
                        control.stop_read();
                    }
                }
                () = async {
                    match cleanup {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => pending::<()>().await,
                    }
                } => {
                    cleanup_fenced = true;
                    self.proof.fenced();
                }
                response = &mut work => {
                    let response = match response {
                        Ok(response) => response,
                        Err(_) => {
                            tracing::error!("tenant authority work unwound; owner retained");
                            self.proof.quarantine();
                            Err(Failure::new(ErrorCode::AuthorityUnavailable, operation.clone()))
                        }
                    };
                    let receipt_rollback = matches!(policy, Policy::TerminalReceipt)
                        && matches!(self.proof.disposition(), Disposition::NoTransaction | Disposition::RolledBack);
                    if response.as_ref().is_err_and(|error| error.0.code == ErrorCode::StorageUnavailable)
                        && !receipt_rollback
                    {
                        control.storage_failed();
                    }
                    control.conclude();
                    let response = if self.proof.disposition() == Disposition::Ambiguous {
                        self.proof.quarantine();
                        match response {
                            Ok(_) => Err(Failure::new(ErrorCode::AuthorityUnavailable, operation.clone())),
                            Err(error) => Err(error),
                        }
                    } else { response };
                    return if expired {
                        Err(Failure::new(policy.expired(self.proof.disposition(), cleanup_fenced || !self.live.0.load(std::sync::atomic::Ordering::Acquire)), operation))
                    } else {
                        response
                    };
                }
                changed = progress.changed(), if observing => {
                    if changed.is_err() {
                        observing = false;
                        tracing::error!("tenant transaction phase watch closed; owner retained");
                        self.proof.quarantine();
                        // Retain the work even if its phase channel was lost.
                    }
                }
            }
        }
    }

    async fn quarantine(
        &self,
        ordinary: &mut mpsc::Receiver<Job>,
        safety: &mut mpsc::Receiver<Job>,
        completions: &mut mpsc::Receiver<Job>,
    ) {
        // Observer failures merely stop admission. Actual ambiguity must also
        // fence the shared ledger, independently of any individual owner.
        self.admission.fence_receipts();
        self.proof.quarantine();
        ordinary.close();
        safety.close();
        completions.close();
        while let Ok(job) = completions.try_recv() {
            job.reject(&self.http_continuation);
        }
        while let Ok(job) = safety.try_recv() {
            job.reject(&self.http_continuation);
        }
        while let Ok(job) = ordinary.try_recv() {
            job.reject(&self.http_continuation);
        }
        // SeaORM exposes no acknowledgement for a failed consuming transaction's
        // Drop rollback. Keep this same actor and its lease until process exit.
        // https://docs.rs/sea-orm/1.1.20/src/sea_orm/database/transaction.rs.html
        pending::<()>().await;
    }
}

impl Job {
    fn reject(self, http: &crate::continuation::tenant::state::State) {
        match self {
            #[cfg(test)]
            Job::Archive { reply, .. } => {
                if reply
                    .send(Err(ErrorCode::AuthorityUnavailable.into()))
                    .is_err()
                {
                    tracing::debug!("rejected archive test waiter disconnected");
                }
            }
            #[cfg(test)]
            Job::Escalate { reply, .. } => {
                if reply
                    .send(Err(ErrorCode::AuthorityUnavailable.into()))
                    .is_err()
                {
                    tracing::debug!("rejected escalation test waiter disconnected");
                }
            }
            Job::UnknownCount(job) => {
                if job
                    .reply
                    .send(Err(ErrorCode::AuthorityUnavailable.into()))
                    .is_err()
                {
                    tracing::debug!("rejected cold tenant count waiter disconnected");
                }
            }
            Job::OAuth(job) => {
                job.exchange.owner.fail(ErrorCode::StorageUnavailable);
                job.progress.finish(Err(ErrorCode::StorageUnavailable));
            }
            Self::Receipt(owner) => owner.fail(ErrorCode::StorageUnavailable),
            Self::ReceiptRead(read) => {
                let _ = read.reply.send(Err(ErrorCode::AuthorityUnavailable.into()));
            }
            #[cfg(all(feature = "core", feature = "harness"))]
            Self::Learner(message) => {
                if message
                    .reply
                    .send(Err(ErrorCode::AuthorityUnavailable.into()))
                    .is_err()
                {
                    tracing::debug!("rejected learner waiter disconnected");
                }
            }
            #[cfg(all(feature = "core", feature = "harness"))]
            Self::ExperimentRead(message) => {
                if message
                    .reply
                    .send(Err(ErrorCode::AuthorityUnavailable.into()))
                    .is_err()
                {
                    tracing::debug!("rejected native cohort has no response receiver");
                }
            }
            #[cfg(all(feature = "core", feature = "harness"))]
            Self::Experiment(message) => {
                if message
                    .reply
                    .send(Err(ErrorCode::AuthorityUnavailable.into()))
                    .is_err()
                {
                    tracing::debug!("rejected native experiment had no response receiver");
                }
            }
            #[cfg(all(feature = "core", feature = "harness"))]
            Self::Learning(message) => {
                // An already quarantined actor cannot attest the pending H phase.
                message.work.settled(Disposition::Ambiguous);
                let _ = message
                    .reply
                    .send(Err(ErrorCode::AuthorityUnavailable.into()));
            }
            #[cfg(all(feature = "core", feature = "harness"))]
            Self::Capture(message) => {
                if message
                    .reply
                    .send(crate::tenancy::capture::queue::Outcome::rejected())
                    .is_err()
                {
                    tracing::debug!("rejected capture had no response receiver");
                }
            }
            #[cfg(all(feature = "core", feature = "harness"))]
            Self::Native(message) => {
                if message
                    .reply
                    .send(crate::tenancy::session::Outcome::rejected())
                    .is_err()
                {
                    tracing::debug!("rejected native journal had no response receiver");
                }
            }
            Self::Command(message) => {
                let error = Failure::new(
                    ErrorCode::AuthorityUnavailable,
                    Some(message.command.operation_id().clone()),
                );
                if message.reply.send(Err(error)).is_err() {
                    tracing::debug!("rejected tenancy command had no response receiver");
                }
            }
            #[cfg(feature = "core")]
            Self::Eval(message) => {
                if message
                    .reply
                    .send(Err(ErrorCode::AuthorityUnavailable.into()))
                    .is_err()
                {
                    tracing::debug!("rejected evaluation had no response receiver");
                }
            }
            #[cfg(feature = "core")]
            Self::Settlement(message) => {
                message.observation.fail();
                if message
                    .reply
                    .send(Err(ErrorCode::AuthorityUnavailable.into()))
                    .is_err()
                {
                    tracing::debug!("rejected observation had no response receiver");
                }
            }
            #[cfg(feature = "core")]
            Self::TrajectoryBegin(message) => {
                message.observation.no_start(Disposition::NoTransaction);
                if message
                    .reply
                    .send(Err(ErrorCode::AuthorityUnavailable.into()))
                    .is_err()
                {
                    tracing::debug!("rejected trajectory begin had no response receiver");
                }
            }
            #[cfg(feature = "core")]
            Self::TrajectoryTerminal(message) => {
                message.observation.fail();
                if message
                    .reply
                    .send(Err(ErrorCode::AuthorityUnavailable.into()))
                    .is_err()
                {
                    tracing::debug!("rejected trajectory terminal had no response receiver");
                }
            }
            Self::Continuation(message) => {
                // This queued job never entered SQL; it cannot own a mutation.
                if let Some(ticket) = message.work.preparation() {
                    http.release(ticket);
                }
                if message
                    .reply
                    .send(Err(ErrorCode::AuthorityUnavailable.into()))
                    .is_err()
                {
                    tracing::debug!("rejected HTTP continuation had no response receiver");
                }
            }
            Self::AuthorityRead(message) => {
                let _ = message
                    .reply
                    .send(Err(ErrorCode::AuthorityUnavailable.into()));
            }
            Self::Credential(message) => {
                if message
                    .reply
                    .send(Err(ErrorCode::AuthorityUnavailable.into()))
                    .is_err()
                {
                    tracing::debug!("rejected tenancy credential read had no response receiver");
                }
            }
        }
    }
}
