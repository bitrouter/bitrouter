use super::*;
use crate::tenancy::{actor, transaction::Disposition};
use crate::{
    acp_trajectory::checkpoint::owned::Summary,
    harness::trajectory::owned::{Append, Appended},
};
use tokio::sync::{OwnedSemaphorePermit, oneshot};
use tokio::time::Instant;

#[derive(Clone)]
pub(in crate::tenancy) enum Request {
    Create(String),
    Append { value: Arc<Append>, cleanup: bool },
    Freeze(String),
    Human(Arc<super::human::Request>),
    Block(Arc<crate::tenancy::execution::block::SelectionRequest>),
}
pub(in crate::tenancy) enum Reply {
    Created,
    Appended(Appended),
    Frozen(Option<Summary>),
    Human(Box<super::human::Reply>, bool),
    Block(Box<crate::tenancy::execution::block::store::Selection>),
}
pub(in crate::tenancy) struct Outcome {
    pub result: Result<Reply, Failure>,
    pub disposition: Disposition,
}
impl Outcome {
    pub(in crate::tenancy) fn rejected() -> Self {
        Self {
            result: Err(ErrorCode::AuthorityUnavailable.into()),
            disposition: Disposition::NoTransaction,
        }
    }
}
pub(in crate::tenancy) struct Job {
    pub binding: Arc<Binding>,
    pub request: Request,
    pub deadline: Instant,
    pub reply: oneshot::Sender<Outcome>,
    pub _read: OwnedSemaphorePermit,
}
impl Request {
    pub(in crate::tenancy) fn cleanup(&self) -> bool {
        matches!(self, Self::Append { cleanup: true, .. })
    }
}

impl Binding {
    #[cfg(all(test, unix))]
    pub(in crate::tenancy) async fn enqueue(
        self: &Arc<Self>,
        request: Request,
    ) -> Result<oneshot::Receiver<Outcome>, Failure> {
        self.enqueue_before(request, Instant::now() + std::time::Duration::from_secs(5))
            .await
    }

    /// The owner stores the returned receiver alongside exact pending input
    /// before awaiting it. H retains an admitted job if that waiter disappears.
    pub(in crate::tenancy) async fn enqueue_before(
        self: &Arc<Self>,
        request: Request,
        deadline: Instant,
    ) -> Result<oneshot::Receiver<Outcome>, Failure> {
        let service = self.service();
        if !self.origin(&service.cleanup) || service.stop.is_cancelled() {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        if !request.cleanup() {
            self.check()?;
        }
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
        {
            let accepting = service
                .accepting
                .lock()
                .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
            if (!request.cleanup() && !*accepting)
                || service.stop.is_cancelled()
                || Instant::now() >= deadline
            {
                return Err(ErrorCode::AuthorityUnavailable.into());
            }
            if !request.cleanup() {
                self.check()?;
            }
            permit.send(actor::Job::Capture(Box::new(Job {
                binding: self.clone(),
                request,
                deadline,
                reply,
                _read: read,
            })));
        }
        Ok(receive)
    }
}
