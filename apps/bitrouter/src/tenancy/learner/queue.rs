use super::{RegistrationInput, source, store};
use crate::tenancy::{Service, actor, error::Failure, model::Caller};
use bitrouter_ai::tenancy::operation::{ErrorCode, OperationResult};
use std::sync::Arc;
use tokio::{
    sync::{OwnedSemaphorePermit, oneshot},
    time::Instant,
};

pub(in crate::tenancy) enum Work {
    Register {
        caller: Caller,
        input: Box<RegistrationInput>,
    },
    Scan(store::Cursor),
    Acquire {
        registration: Arc<super::Registration>,
        stopped: Arc<crate::tenancy::hosts::StoppedRead>,
        deadline: Instant,
    },
    Publish {
        acquired: Arc<source::Acquired>,
        calculated: Box<source::Calculated>,
        shutdown: tokio_util::sync::CancellationToken,
    },
}
impl Work {
    pub(in crate::tenancy) fn check_cancelled(&self) -> Result<(), ErrorCode> {
        if matches!(self, Self::Publish { shutdown, .. } if shutdown.is_cancelled()) {
            return Err(ErrorCode::CapacityExceeded);
        }
        Ok(())
    }
}
pub(in crate::tenancy) enum Reply {
    Registered(OperationResult),
    Scanned(store::Page),
    Acquired(Arc<source::Acquired>),
    Published(OperationResult),
    Unchanged,
}
pub(in crate::tenancy) struct Job {
    pub work: Work,
    pub deadline: Instant,
    pub reply: oneshot::Sender<Result<Reply, Failure>>,
    pub read: OwnedSemaphorePermit,
    #[cfg(all(test, unix))]
    pub test: Arc<super::testing::Hooks>,
    pub lease: std::sync::OnceLock<Arc<crate::tenancy::admission::LearningLease>>,
}

pub(in crate::tenancy) async fn request(
    service: &Service,
    work: Work,
    deadline: Instant,
) -> Result<Reply, Failure> {
    work.check_cancelled().map_err(Failure::from)?;
    let read = service
        .reads
        .clone()
        .try_acquire_owned()
        .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
    let permit = tokio::time::timeout_at(deadline, service.commands.reserve())
        .await
        .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?
        .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
    let (reply, receive) = oneshot::channel();
    #[cfg(all(test, unix))]
    let publication = matches!(&work, Work::Publish { .. });
    {
        let accepting = service
            .accepting
            .lock()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        if !*accepting || service.stop.is_cancelled() || Instant::now() >= deadline {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        work.check_cancelled().map_err(Failure::from)?;
        permit.send(actor::Job::Learner(Box::new(Job {
            work,
            deadline,
            reply,
            read,
            lease: Default::default(),
            #[cfg(all(test, unix))]
            test: service.learner_test.clone(),
        })));
    }
    #[cfg(all(test, unix))]
    if publication {
        service.learner_test.queued.notify_one();
    }
    tokio::time::timeout_at(deadline, receive)
        .await
        .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?
        .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?
}

pub(in crate::tenancy) async fn register(
    service: &Service,
    caller: Caller,
    input: RegistrationInput,
) -> Result<OperationResult, Failure> {
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    match request(
        service,
        Work::Register {
            caller,
            input: Box::new(input),
        },
        deadline,
    )
    .await?
    {
        Reply::Registered(result) => Ok(result),
        _ => Err(ErrorCode::AuthorityUnavailable.into()),
    }
}
