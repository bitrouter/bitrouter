//! Bounded single-writer actor; admitted commands outlive HTTP cancellation.

mod archive;
mod authority_read;
mod command;
mod obligations;

#[cfg(all(feature = "core", feature = "harness"))]
mod capture;
mod continuation;
mod custody;
#[cfg(feature = "core")]
mod eval;
#[cfg(all(feature = "core", feature = "harness"))]
mod experiment;
#[cfg(all(feature = "core", feature = "harness"))]
mod learner;
#[cfg(all(feature = "core", feature = "harness"))]
mod learning;
mod learning_startup;
mod oauth;
mod receipts;
#[cfg(all(feature = "core", feature = "harness"))]
mod session;
#[cfg(feature = "core")]
mod trajectory;

#[cfg(test)]
mod tests;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[cfg(feature = "core")]
use super::transaction::Disposition;
use bitrouter_ai::tenancy::operation::ErrorCode;
#[cfg(all(feature = "core", feature = "harness"))]
use bitrouter_ai::tenancy::operation::{OperationResult, OperationStatus};
#[cfg(feature = "core")]
use chrono::Utc;
use sea_orm::DatabaseConnection;
use tokio::sync::{OwnedSemaphorePermit, oneshot};
use tokio::time::Instant;

use super::{
    command::Command,
    error::Failure,
    model::{Caller, Snapshot},
    store::{self, StoredResponse},
    transaction::{Control, Proof},
};

pub(super) struct Message {
    pub caller: Caller,
    pub command: Command,
    pub reply: oneshot::Sender<Result<StoredResponse, Failure>>,
}

pub(super) enum Job {
    #[cfg(all(feature = "core", feature = "harness"))]
    Learner(Box<super::learner::queue::Job>),
    #[cfg(all(feature = "core", feature = "harness"))]
    Experiment(Box<super::experiment::queue::Job>),
    #[cfg(all(feature = "core", feature = "harness"))]
    ExperimentRead(Box<crate::tenancy::experiment::queue::ReadJob>),
    #[cfg(all(feature = "core", feature = "harness"))]
    Learning(Box<super::learning::queue::Job>),
    #[cfg(all(feature = "core", feature = "harness"))]
    Capture(Box<super::capture::queue::Job>),
    #[cfg(all(feature = "core", feature = "harness"))]
    Native(Box<super::session::Job>),
    OAuth(Box<super::oauth::exchange::Job>),
    Command(Box<Message>),
    Credential(CredentialRead),
    AuthorityRead(AuthorityRead),
    UnknownCount(UnknownCount),
    #[cfg(test)]
    Escalate {
        now: chrono::DateTime<chrono::Utc>,
        deadline: Option<Instant>,
        reply: oneshot::Sender<Result<usize, Failure>>,
    },
    #[cfg(test)]
    Archive {
        deadline: Option<Instant>,
        reply: oneshot::Sender<Result<bool, Failure>>,
    },
    Receipt(Arc<super::receipts::owner::Owner>),
    ReceiptRead(super::receipts::read::Read),
    Continuation(Box<super::continuation::http::Job>),
    #[cfg(feature = "core")]
    Eval(Box<super::eval::Job>),
    #[cfg(feature = "core")]
    Settlement(Box<super::eval::Settlement>),
    #[cfg(feature = "core")]
    TrajectoryBegin(Box<super::trajectory::Begin>),
    #[cfg(feature = "core")]
    TrajectoryTerminal(Box<super::trajectory::Terminal>),
}

impl From<Message> for Job {
    fn from(message: Message) -> Self {
        Self::Command(Box::new(message))
    }
}

pub(super) struct CredentialRead {
    pub caller: Caller,
    pub authority: super::authority::live::Lease,
    pub id: bitrouter_ai::tenancy::identity::CredentialId,
    pub deadline: Instant,
    pub permit: OwnedSemaphorePermit,
    pub reply: oneshot::Sender<
        Result<
            (
                bitrouter_ai::tenancy::identity::AuthorityRevision,
                Option<super::model::Credential>,
            ),
            Failure,
        >,
    >,
}

pub(super) enum AuthoritySubject {
    Token {
        hash: String,
        reuse: bool,
    },
    Caller(Caller),
    Tenant {
        caller: Caller,
        tenant: bitrouter_ai::tenancy::identity::TenantId,
        active: bool,
        resource: Option<bitrouter_ai::tenancy::configuration::ResourceRef>,
    },
    Extend {
        caller: Caller,
        lease: super::authority::live::Lease,
        dependencies: Vec<bitrouter_ai::tenancy::configuration::ResourceRef>,
        intent: super::authority::live::DependencyIntent,
    },
}

pub(super) struct AuthorityRead {
    pub subject: AuthoritySubject,
    pub deadline: Instant,
    pub permit: OwnedSemaphorePermit,
    pub reply: oneshot::Sender<Result<super::authority::live::Authenticated, Failure>>,
}

pub(super) struct UnknownCount {
    pub tenant: bitrouter_ai::tenancy::identity::TenantId,
    pub deadline: Instant,
    pub permit: OwnedSemaphorePermit,
    pub reply: oneshot::Sender<Result<super::admission::unknown::CountPin, Failure>>,
}

/// Any exit (including task cancellation) invalidates the last published view.
pub(super) struct Liveness(pub Arc<AtomicBool>);
impl Drop for Liveness {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Acknowledged bootstrap result, captured before actor maintenance starts.
pub(super) struct Initialized {
    pub crypto: Option<super::continuation::PinnedCrypto>,
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub capacity: Option<crate::telemetry::receipt_latency::capacity::Snapshot>,
}

pub(super) struct Worker {
    pub startup: Option<Arc<super::origin::StartupOwner>>,
    pub db: DatabaseConnection,
    pub authority: Arc<super::authority::live::Authority>,
    pub live: Liveness,
    pub owner: Option<super::ownership::Lease>,
    pub resources: Arc<super::resources::Resources>,
    pub proof: Arc<Proof>,
    pub admission: Arc<super::admission::Admission>,
    pub obligation_due: Arc<std::sync::Mutex<Option<chrono::DateTime<chrono::Utc>>>>,
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub observe_capacity: bool,
    pub http_continuation: Arc<crate::continuation::tenant::state::State>,
    #[cfg(feature = "core")]
    pub eval: Arc<super::eval::State>,
    #[cfg(feature = "core")]
    pub accepting: Arc<std::sync::Mutex<bool>>,
}

impl Worker {
    fn current(&self) -> Result<Arc<Snapshot>, Failure> {
        if !self.live.0.load(Ordering::Acquire) {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        self.authority
            .read()
            .map(|guard| Arc::new(Snapshot::from_header(guard.header())))
            .map_err(Failure::from)
    }

    fn unavailable(&self, code: ErrorCode) -> Failure {
        if code == ErrorCode::StorageUnavailable {
            self.admission.fence_storage();
        }
        self.live.0.store(false, Ordering::Release);
        code.into()
    }

    fn control(&self) -> Control {
        Control::new(self.proof.clone()).with_storage(super::storage::Budget::new(
            self.admission.clone(),
            self.proof.clone(),
        ))
    }
}
