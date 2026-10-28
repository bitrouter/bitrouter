//! Native journal jobs on the existing authority writer.
pub(crate) mod assessment;
pub(crate) mod recovery;
use bitrouter_ai::tenancy::operation::ErrorCode;
use std::{sync::Arc, time::Duration};
use tokio::sync::{OwnedSemaphorePermit, oneshot};

use super::{
    Service, actor,
    error::Failure,
    execution::session::{Authority, Binding},
};
use crate::managed_store::owned::Loaded;

pub(super) const WORK: Duration = Duration::from_secs(5);

pub(super) enum Request {
    Load(Arc<Authority>),
    Assessment(Box<assessment::Request>),
    Reconcile(Arc<Binding>, Option<Arc<recovery::CheckedRecovery>>),
    Save {
        binding: Arc<Binding>,
        expected: u64,
        bytes: Vec<u8>,
        cleanup: bool,
        metadata: Option<recovery::Metadata>,
        recovery: Option<Arc<recovery::CheckedRecovery>>,
    },
}
pub(super) enum Reply {
    Loaded(Option<Box<Loaded>>),
    Saved,
    Assessment {
        value: Box<assessment::Reply>,
        wrote: bool,
    },
}
pub(super) struct Outcome {
    pub result: Result<Reply, Failure>,
    pub disposition: super::transaction::Disposition,
}
impl Outcome {
    pub(super) fn rejected() -> Self {
        Self {
            result: Err(ErrorCode::AuthorityUnavailable.into()),
            disposition: super::transaction::Disposition::NoTransaction,
        }
    }
}

pub(crate) struct NativeFailure {
    failure: Failure,
    disposition: super::transaction::Disposition,
}
impl From<Failure> for NativeFailure {
    fn from(failure: Failure) -> Self {
        Self {
            failure,
            disposition: super::transaction::Disposition::NoTransaction,
        }
    }
}
impl From<ErrorCode> for NativeFailure {
    fn from(code: ErrorCode) -> Self {
        Self::from(Failure::from(code))
    }
}
impl NativeFailure {
    fn unknown() -> Self {
        Self {
            failure: ErrorCode::AuthorityUnavailable.into(),
            disposition: super::transaction::Disposition::Ambiguous,
        }
    }
    pub(crate) fn core(self) -> bitrouter_ai::core::protocol::CoreError {
        use super::transaction::Disposition;
        use bitrouter_ai::core::protocol::CommitStatus;
        let mut error = super::execution::session::core_error(self.failure);
        error.commit_status = match self.disposition {
            Disposition::NoTransaction | Disposition::RolledBack => CommitStatus::NotCommitted,
            Disposition::Committed => CommitStatus::Committed,
            Disposition::Pending | Disposition::Ambiguous => CommitStatus::Unknown,
        };
        error
    }
}

pub(super) struct Job {
    pub request: Request,
    pub deadline: tokio::time::Instant,
    pub reply: oneshot::Sender<Outcome>,
    pub _read: OwnedSemaphorePermit,
}

#[derive(Debug, thiserror::Error)]
#[error("native journal operation rejected")]
pub(crate) struct Rejected(pub ErrorCode);

impl Service {
    pub(crate) async fn load_native_journal(
        &self,
        authority: Arc<Authority>,
    ) -> Result<Option<Loaded>, NativeFailure> {
        match self.native_journal(Request::Load(authority)).await? {
            Reply::Loaded(value) => Ok(value.map(|loaded| *loaded)),
            Reply::Saved | Reply::Assessment { .. } => Err(self.unavailable().into()),
        }
    }
    pub(crate) async fn reconcile_native_journal(
        &self,
        binding: Arc<Binding>,
        recovery: Option<Arc<recovery::CheckedRecovery>>,
    ) -> Result<Option<Loaded>, NativeFailure> {
        match self
            .native_journal(Request::Reconcile(binding, recovery))
            .await?
        {
            Reply::Loaded(value) => Ok(value.map(|loaded| *loaded)),
            Reply::Saved | Reply::Assessment { .. } => Err(self.unavailable().into()),
        }
    }
    pub(crate) async fn save_native_journal(
        &self,
        binding: Arc<Binding>,
        expected: u64,
        bytes: Vec<u8>,
        cleanup: bool,
        metadata: Option<recovery::Metadata>,
        recovery: Option<Arc<recovery::CheckedRecovery>>,
    ) -> Result<(), NativeFailure> {
        match self
            .native_journal(Request::Save {
                binding,
                expected,
                bytes,
                cleanup,
                metadata,
                recovery,
            })
            .await?
        {
            Reply::Saved => Ok(()),
            Reply::Loaded(_) | Reply::Assessment { .. } => Err(self.unavailable().into()),
        }
    }
    pub(crate) async fn native_assessment(
        &self,
        request: assessment::Request,
    ) -> Result<assessment::Reply, NativeFailure> {
        match self
            .native_journal(Request::Assessment(Box::new(request)))
            .await?
        {
            Reply::Assessment { value, .. } => Ok(*value),
            _ => Err(self.unavailable().into()),
        }
    }
    async fn native_journal(&self, request: Request) -> Result<Reply, NativeFailure> {
        let cleanup = matches!(
            &request,
            Request::Save { cleanup: true, .. } | Request::Reconcile(_, _)
        );
        let authority = match &request {
            Request::Load(authority) => authority.as_ref(),
            Request::Assessment(request) => request.binding.authority().as_ref(),
            Request::Save { binding, .. } | Request::Reconcile(binding, _) => {
                binding.authority().as_ref()
            }
        };
        if !authority.origin(&self.cleanup) || self.stop.is_cancelled() {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        if !cleanup {
            authority.check()?;
        }
        let deadline = match &request {
            Request::Assessment(request) => {
                request.check()?;
                request.deadline
            }
            _ => tokio::time::Instant::now() + WORK,
        };
        let read = self
            .reads
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
        let permit = tokio::time::timeout_at(deadline, self.commands.reserve())
            .await
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        let (reply, receive) = oneshot::channel();
        {
            let accepting = self
                .accepting
                .lock()
                .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
            if (!cleanup && !*accepting) || self.stop.is_cancelled() {
                return Err(ErrorCode::AuthorityUnavailable.into());
            }
            if !cleanup {
                authority.check()?;
            }
            if let Request::Assessment(request) = &request {
                request.check()?;
            }
            permit.send(actor::Job::Native(Box::new(Job {
                request,
                deadline,
                reply,
                _read: read,
            })));
        }
        let outcome = tokio::time::timeout_at(deadline, receive)
            .await
            .map_err(|_| NativeFailure::unknown())?
            .map_err(|_| NativeFailure::unknown())?;
        outcome.result.map_err(|failure| NativeFailure {
            failure,
            disposition: outcome.disposition,
        })
    }
}
