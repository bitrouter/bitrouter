use super::{owner::Owner, store::Prepared};
use crate::acp_trajectory::checkpoint::owned::Summary;
use crate::tenancy::{actor, capture::Binding, error::Failure, transaction::Disposition};
use bitrouter_ai::tenancy::operation::ErrorCode;
use std::sync::Arc;
use tokio::{
    sync::{OwnedSemaphorePermit, oneshot},
    time::Instant,
};

pub(in crate::tenancy) enum Work {
    NativePrepare(Box<super::native::Prepare>),
    NativeStopped(Arc<crate::harness::resources::managed::session::owned::CleanupProof>),
    Prepare {
        binding: Arc<Binding>,
        native: String,
        summary: Summary,
    },
    Accept(Arc<Owner>),
    Finish(Arc<Owner>),
    Observe(Arc<Owner>),
    CaptureStopped(Arc<crate::harness::acp::controlled::CleanupProof>),
    StopSet(Arc<crate::tenancy::execution::lifecycle::paid::StopSet>),
}
pub(in crate::tenancy) enum Reply {
    Prepared(Arc<Prepared>),
    Accepted(bool),
    Finished,
    Observed,
    CaptureStopped,
    NativeStopped,
    Stopped,
}
pub(in crate::tenancy) struct Job {
    pub work: Work,
    pub deadline: Instant,
    pub reply: oneshot::Sender<Result<Reply, Failure>>,
    pub _read: Option<OwnedSemaphorePermit>,
}
impl Work {
    pub(in crate::tenancy) fn binding(&self) -> super::source::Binding {
        match self {
            Self::Prepare { binding, .. } => super::source::Binding::Capture(binding.clone()),
            Self::NativePrepare(request) => super::source::Binding::Native(request.binding.clone()),
            Self::NativeStopped(proof) => super::source::Binding::Native(proof.binding().clone()),
            Self::CaptureStopped(proof) => super::source::Binding::Capture(proof.binding().clone()),
            Self::StopSet(proof) => proof.binding().clone(),
            Self::Accept(owner) | Self::Finish(owner) | Self::Observe(owner) => {
                owner.binding.clone()
            }
        }
    }
    pub(in crate::tenancy) fn cleanup(&self) -> bool {
        matches!(
            self,
            Self::Finish(_)
                | Self::Observe(_)
                | Self::CaptureStopped(_)
                | Self::NativeStopped(_)
                | Self::StopSet(_)
        )
    }
    pub(in crate::tenancy) fn settled(&self, disposition: Disposition) {
        match self {
            Self::Accept(owner) => owner.settled(disposition),
            Self::Finish(owner) => owner.finish_settled(disposition),
            Self::Observe(owner) => owner.observe_settled(disposition),
            Self::Prepare { .. }
            | Self::NativePrepare(_)
            | Self::CaptureStopped(_)
            | Self::NativeStopped(_)
            | Self::StopSet(_) => {}
        }
    }
}
async fn enqueue(
    work: Work,
    deadline: Instant,
) -> Result<oneshot::Receiver<Result<Reply, Failure>>, Failure> {
    let service = work.binding().service().clone();
    if !work.binding().origin(&service.cleanup) || service.stop.is_cancelled() {
        return Err(ErrorCode::AuthorityUnavailable.into());
    }
    let cleanup = work.cleanup();
    let read = if cleanup {
        None
    } else {
        Some(
            service
                .reads
                .clone()
                .try_acquire_owned()
                .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?,
        )
    };
    let sender = if cleanup {
        service.completions.clone()
    } else {
        service.commands.clone()
    };
    let permit = tokio::time::timeout_at(deadline, sender.reserve())
        .await
        .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?
        .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
    let (reply, receive) = oneshot::channel();
    {
        let accepting = service
            .accepting
            .lock()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        if (!cleanup && !*accepting) || Instant::now() >= deadline || service.stop.is_cancelled() {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        if !cleanup {
            work.binding().assessment_check().map_err(Failure::from)?;
        }
        permit.send(actor::Job::Learning(Box::new(Job {
            work,
            deadline,
            reply,
            _read: read,
        })));
    }
    Ok(receive)
}
pub(super) async fn prepare(
    binding: Arc<Binding>,
    native: &str,
    summary: &Summary,
) -> Result<Arc<Prepared>, Failure> {
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    let receive = enqueue(
        Work::Prepare {
            binding,
            native: native.to_owned(),
            summary: summary.clone(),
        },
        deadline,
    )
    .await?;
    match tokio::time::timeout_at(deadline, receive).await {
        Ok(Ok(Ok(Reply::Prepared(value)))) => Ok(value),
        Ok(Ok(Err(error))) => Err(error),
        _ => Err(ErrorCode::AuthorityUnavailable.into()),
    }
}
pub(super) async fn accept(owner: Arc<Owner>) -> Result<bool, Failure> {
    // Cancellation before enqueue must not leave a receipt waiting for an actor
    // that never received this job. Once sent, only actor H evidence settles it.
    struct EnqueueGuard(Option<Arc<Owner>>);
    impl Drop for EnqueueGuard {
        fn drop(&mut self) {
            if let Some(owner) = self.0.take() {
                owner.settled(Disposition::NoTransaction);
            }
        }
    }
    let mut guard = EnqueueGuard(Some(owner.clone()));
    let deadline = owner
        .deadline
        .min(Instant::now() + std::time::Duration::from_secs(5));
    let receive = match enqueue(Work::Accept(owner.clone()), deadline).await {
        Ok(receive) => receive,
        Err(error) => {
            owner.settled(Disposition::NoTransaction);
            return Err(error);
        }
    };
    guard.0 = None;
    match tokio::time::timeout_at(deadline, receive).await {
        Ok(Ok(Ok(Reply::Accepted(value)))) => Ok(value),
        Ok(Ok(Err(error))) => Err(error),
        _ => Err(ErrorCode::AuthorityUnavailable.into()),
    }
}
pub(super) async fn finish(owner: Arc<Owner>) -> Result<(), ErrorCode> {
    #[cfg(all(test, unix))]
    {
        let gate = owner
            .binding
            .service()
            .paid_finish_gate
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?
            .take();
        if let Some(gate) = gate {
            gate.execution
                .set(owner.execution.clone())
                .map_err(|_| ErrorCode::AuthorityUnavailable)?;
            gate.entered.notify_one();
            gate.release.cancelled().await;
        }
    }
    owner.begin_finish()?;
    struct EnqueueGuard(Option<Arc<Owner>>);
    impl Drop for EnqueueGuard {
        fn drop(&mut self) {
            if let Some(owner) = self.0.take() {
                owner.finish_settled(Disposition::NoTransaction);
            }
        }
    }
    let mut guard = EnqueueGuard(Some(owner.clone()));
    let deadline = owner.cleanup_deadline()?;
    let receive = enqueue(Work::Finish(owner), deadline)
        .await
        .map_err(|error| error.0.code)?;
    guard.0 = None;
    match tokio::time::timeout_at(deadline, receive).await {
        Ok(Ok(Ok(Reply::Finished))) => Ok(()),
        Ok(Ok(Err(error))) => Err(error.0.code),
        _ => Err(ErrorCode::AuthorityUnavailable),
    }
}

/// One actual parsed-result observation; actor ACK, not waiter lifetime, owns it.
pub(super) async fn observe(
    owner: Arc<Owner>,
    output: crate::core::evolution::judge::JudgeOutput,
) -> Result<(), ErrorCode> {
    owner.prepare_observation(output)?;
    if !owner.begin_observe()? {
        return Ok(());
    }
    struct Guard(Option<Arc<Owner>>);
    impl Drop for Guard {
        fn drop(&mut self) {
            if let Some(owner) = self.0.take() {
                owner.observe_settled(Disposition::NoTransaction);
            }
        }
    }
    let mut guard = Guard(Some(owner.clone()));
    let deadline = owner.cleanup_deadline()?;
    let receive = enqueue(Work::Observe(owner), deadline)
        .await
        .map_err(|error| error.0.code)?;
    guard.0 = None;
    match tokio::time::timeout_at(deadline, receive).await {
        Ok(Ok(Ok(Reply::Observed))) => Ok(()),
        Ok(Ok(Err(error))) => Err(error.0.code),
        _ => Err(ErrorCode::AuthorityUnavailable),
    }
}

pub(in crate::tenancy) async fn capture_stopped(
    proof: Arc<crate::harness::acp::controlled::CleanupProof>,
) -> Result<(), ErrorCode> {
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    let receive = enqueue(Work::CaptureStopped(proof), deadline)
        .await
        .map_err(|error| error.0.code)?;
    match tokio::time::timeout_at(
        deadline + crate::tenancy::transaction::CLEANUP_TIMEOUT,
        receive,
    )
    .await
    {
        Ok(Ok(Ok(Reply::CaptureStopped))) => Ok(()),
        Ok(Ok(Err(error))) => Err(error.0.code),
        _ => Err(ErrorCode::AuthorityUnavailable),
    }
}

pub(in crate::tenancy) async fn stop_set(
    proof: Arc<crate::tenancy::execution::lifecycle::paid::StopSet>,
    deadline: Instant,
) -> Result<(), ErrorCode> {
    let receive = enqueue(Work::StopSet(proof), deadline)
        .await
        .map_err(|error| error.0.code)?;
    match tokio::time::timeout_at(deadline, receive).await {
        Ok(Ok(Ok(Reply::Stopped))) => Ok(()),
        Ok(Ok(Err(error))) => Err(error.0.code),
        _ => Err(ErrorCode::AuthorityUnavailable),
    }
}

pub(super) async fn prepare_native(
    request: super::native::Prepare,
) -> Result<Arc<Prepared>, Failure> {
    let deadline = request.deadline;
    let receive = enqueue(Work::NativePrepare(Box::new(request)), deadline).await?;
    match tokio::time::timeout_at(deadline, receive).await {
        Ok(Ok(Ok(Reply::Prepared(value)))) => Ok(value),
        Ok(Ok(Err(error))) => Err(error),
        _ => Err(ErrorCode::AuthorityUnavailable.into()),
    }
}

pub(in crate::tenancy) async fn native_stopped(
    proof: Arc<crate::harness::resources::managed::session::owned::CleanupProof>,
) -> Result<(), ErrorCode> {
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    let receive = enqueue(Work::NativeStopped(proof), deadline)
        .await
        .map_err(|e| e.0.code)?;
    match tokio::time::timeout_at(
        deadline + crate::tenancy::transaction::CLEANUP_TIMEOUT,
        receive,
    )
    .await
    {
        Ok(Ok(Ok(Reply::NativeStopped))) => Ok(()),
        Ok(Ok(Err(error))) => Err(error.0.code),
        _ => Err(ErrorCode::AuthorityUnavailable),
    }
}
