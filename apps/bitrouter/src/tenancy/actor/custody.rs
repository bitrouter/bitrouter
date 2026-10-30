//! The owning actor supervises transactions; HTTP owns only its response waiter.

use std::future::{Future, pending};
use std::panic::AssertUnwindSafe;

use futures::FutureExt;
use tokio::sync::mpsc;

use super::{Control, CredentialRead, ErrorCode, Failure, Instant, Job, Message, Worker};
use crate::tenancy::transaction::{Disposition, Phase};

#[derive(Clone, Copy)]
enum Policy {
    Management,
    #[cfg(all(feature = "core", feature = "harness"))]
    Native,
    CredentialRead,
    Receipt,
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
            let control = Control::new(self.proof.clone());
            let work = async {
                let initial = self.current()?;
                let crypto = crate::tenancy::continuation::initialize(
                    &self.db,
                    &initial,
                    &self.resources,
                    import.as_deref(),
                    &control,
                    &self.oauth,
                )
                .await
                .map_err(|_| self.unavailable(ErrorCode::AuthorityUnavailable))?;
                let inventory =
                    crate::tenancy::receipts::store::inventory(&self.db, &initial.installation)
                        .await
                        .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
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
                self.learning_startup(
                    &control,
                    #[cfg(all(feature = "core", feature = "harness"))]
                    &paid_recovery,
                )
                .await?;
                Ok(crypto)
            };
            let result = self
                .supervise(
                    work,
                    &control,
                    Instant::now() + crate::tenancy::MANAGEMENT_TIMEOUT,
                    Policy::Management,
                    None,
                )
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
        let mut ordinary_open = true;
        let mut safety_open = true;
        let mut completions_open = true;
        loop {
            if !ordinary_open && !safety_open && !completions_open {
                break;
            }
            let escalation = self.admission.next_receipt_escalation();
            let job = tokio::select! {
                biased;
                () = stop.cancelled(), if !draining && (ordinary_open || safety_open || completions_open) => {
                    ordinary.close();
                    safety.close();
                    completions.close();
                    draining = true;
                    continue;
                }
                job = safety.recv(), if safety_open => match job {
                    Some(job) => job,
                    None => { safety_open = false; continue; }
                },
                ()=async {match escalation{Some(deadline)=>tokio::time::sleep_until(deadline).await,None=>pending::<()>().await}}=>{self.admission.escalate_receipts();continue;}
                job = completions.recv(), if completions_open => match job {
                    Some(job) => job,
                    None => { completions_open = false; continue; }
                },
                job = ordinary.recv(), if ordinary_open => match job {
                    Some(job) => job,
                    None => { ordinary_open = false; continue; }
                },
                else => break,
            };
            let control = Control::new(self.proof.clone());
            match job {
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
                            Policy::Receipt,
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
                Job::Credential(CredentialRead {
                    caller,
                    id,
                    deadline,
                    permit,
                    reply,
                }) => {
                    let work = async {
                        self.current()?
                            .principal(&caller, chrono::Utc::now())
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
                    drop((caller, id, permit));
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
        deadline: Instant,
        policy: Policy,
        operation: Option<bitrouter_ai::tenancy::identity::OperationId>,
    ) -> Result<T, Failure> {
        // Reject an already-expired queued job before polling any database
        // future. A selected timer is the watchdog for work admitted here,
        // not the admission check for an elapsed absolute deadline.
        if Instant::now() >= deadline {
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
                () = tokio::time::sleep_until(deadline), if !expired => {
                    expired = true;
                    if control.phase() == Phase::Preparing {
                        // No transaction exists and no detached continuation may commit.
                        return Err(Failure::new(policy.expired(Disposition::NoTransaction, !self.live.0.load(std::sync::atomic::Ordering::Acquire)), operation));
                    }
                    let keep_write = match policy {
                        Policy::Management | Policy::Receipt => true,
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
