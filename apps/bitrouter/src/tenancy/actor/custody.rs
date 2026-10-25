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
    CredentialRead,
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
        stop: tokio_util::sync::CancellationToken,
    ) {
        // The owning frame is outside every fallible transaction continuation.
        let _owner = &self.owner;
        let result = AssertUnwindSafe(self.run_loop(&mut ordinary, &mut safety, stop))
            .catch_unwind()
            .await;
        if result.is_err() {
            tracing::error!("tenant authority worker unwound; owner retained");
        }
        if result.is_err() || self.proof.ensure_clean().is_err() {
            self.proof.quarantine();
            self.quarantine(&mut ordinary, &mut safety).await;
        }
    }

    async fn run_loop(
        &self,
        ordinary: &mut mpsc::Receiver<Job>,
        safety: &mut mpsc::Receiver<Job>,
        stop: tokio_util::sync::CancellationToken,
    ) {
        let mut draining = false;
        let mut ordinary_open = true;
        let mut safety_open = true;
        loop {
            let job = tokio::select! {
                biased;
                () = stop.cancelled(), if !draining && (ordinary_open || safety_open) => {
                    ordinary.close();
                    safety.close();
                    draining = true;
                    continue;
                }
                job = safety.recv(), if safety_open => match job {
                    Some(job) => job,
                    None => { safety_open = false; continue; }
                },
                job = ordinary.recv(), if ordinary_open => match job {
                    Some(job) => job,
                    None => { ordinary_open = false; continue; }
                },
                else => break,
            };
            let control = Control::new(self.proof.clone());
            match job {
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
                        self.quarantine(ordinary, safety).await;
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
                        self.quarantine(ordinary, safety).await;
                    }
                    // An abandoned HTTP waiter cannot release the DB-read capacity.
                    drop((caller, id, permit));
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
                        self.quarantine(ordinary, safety).await;
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
                        self.quarantine(ordinary, safety).await;
                    }
                    drop((execution, subject, observation));
                }
            }
        }
    }

    /// Concrete authority consumers share supervision, never a task registry.
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
                        Policy::Management => true,
                        Policy::CredentialRead => false,
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
    ) {
        self.proof.quarantine();
        ordinary.close();
        safety.close();
        while let Ok(job) = safety.try_recv() {
            job.reject();
        }
        while let Ok(job) = ordinary.try_recv() {
            job.reject();
        }
        // SeaORM exposes no acknowledgement for a failed consuming transaction's
        // Drop rollback. Keep this same actor and its lease until process exit.
        // https://docs.rs/sea-orm/1.1.20/src/sea_orm/database/transaction.rs.html
        pending::<()>().await;
    }
}

impl Job {
    fn reject(self) {
        match self {
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
