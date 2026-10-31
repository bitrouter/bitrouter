//! Bounded single-writer actor; admitted commands outlive HTTP cancellation.

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
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};

use bitrouter_ai::tenancy::{
    operation::{ErrorCode, OperationResult, OperationStatus},
    validation::Validate,
};
use chrono::Utc;
use sea_orm::DatabaseConnection;
use tokio::sync::{OwnedSemaphorePermit, oneshot};
use tokio::time::Instant;

use super::{
    command::Command,
    error::Failure,
    model::{Caller, Snapshot},
    mutation,
    store::{self, Operation, StoredResponse},
    transaction::{Control, Disposition, Proof},
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
    pub oauth: Arc<super::oauth::State>,
    pub db: DatabaseConnection,
    pub snapshot: Arc<RwLock<Arc<Snapshot>>>,
    pub live: Liveness,
    pub owner: Option<super::ownership::Lease>,
    pub limits: OperationLimits,
    pub resources: Arc<super::resources::Resources>,
    pub proof: Arc<Proof>,
    pub admission: Arc<super::admission::Admission>,
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub observe_capacity: bool,
    pub http_continuation: Arc<crate::continuation::tenant::state::State>,
    #[cfg(feature = "core")]
    pub eval: Arc<super::eval::State>,
    #[cfg(feature = "core")]
    pub accepting: Arc<std::sync::Mutex<bool>>,
}

#[derive(Clone, Copy)]
pub(super) struct OperationLimits {
    pub ordinary: u32,
    pub safety: u32,
}

impl Default for OperationLimits {
    fn default() -> Self {
        Self {
            ordinary: store::ORDINARY_OPERATIONS,
            safety: store::SAFETY_OPERATIONS,
        }
    }
}

impl Worker {
    fn current(&self) -> Result<Arc<Snapshot>, Failure> {
        if !self.live.0.load(Ordering::Acquire) {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        self.snapshot
            .read()
            .map(|state| Arc::clone(&state))
            .map_err(|_| ErrorCode::AuthorityUnavailable.into())
    }

    fn unavailable(&self, code: ErrorCode) -> Failure {
        self.live.0.store(false, Ordering::Release);
        code.into()
    }

    async fn execute(
        &self,
        caller: &Caller,
        command: &Command,
        control: &Control,
    ) -> Result<StoredResponse, Failure> {
        let old = self.current()?;
        let now = Utc::now();
        if command.installation() != &old.installation {
            return Err(ErrorCode::CapabilityMismatch.into());
        }
        let principal = old.principal(caller, now).map_err(Failure::from)?.clone();
        let digest = command
            .digest(&principal)
            .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
        // A write-only credential must not leave a public offline guessing hash
        // in operation storage. Bind resource command digests to the wrapping key.
        let digest = self
            .resources
            .management_digest(command, &digest)
            .map_err(Failure::from)?;
        let previous = store::operation(
            &self.db,
            &old.installation,
            &principal,
            command.operation_id(),
        )
        .await
        .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        if let Some(previous) = previous {
            old.authorize_read(caller, &previous.context, Utc::now())
                .map_err(|_| Failure::from(ErrorCode::Forbidden))?;
            if previous.created_at + chrono::Duration::days(30) <= now {
                return Err(Failure::new(
                    ErrorCode::OperationExpired,
                    Some(command.operation_id().clone()),
                ));
            }
            if previous.digest != digest {
                return Err(Failure::new(
                    ErrorCode::OperationConflict,
                    Some(command.operation_id().clone()),
                ));
            }
            return Ok(previous.response);
        }
        let existing_target = match command {
            Command::Tenant(id, _) => old.tenants.contains_key(id),
            _ => true,
        };
        let safety = old.saturated
            && matches!(caller, Caller::Installation(_))
            && command.safety()
            && existing_target;
        if (old.saturated && !safety) || (safety && old.safety_operation_ids >= self.limits.safety)
        {
            return Err(Failure::new(
                ErrorCode::CapacityExceeded,
                Some(command.operation_id().clone()),
            ));
        }
        let mut candidate = (*old).clone();
        candidate
            .credentials
            .retain(|_, credential| credential.expires.instant > now);
        candidate
            .credential_hashes
            .retain(|_, id| candidate.credentials.contains_key(id));
        // Management reads may have waited on storage. Expiry and lifecycle are
        // checked once more at application, under the actor's current authority.
        let now = Utc::now();
        let prepared =
            mutation::prepare(&self.db, &candidate, &self.resources, caller, command, now)
                .await
                .map_err(|code| {
                    if matches!(
                        code,
                        ErrorCode::StorageUnavailable | ErrorCode::AuthorityUnavailable
                    ) {
                        self.unavailable(code)
                    } else {
                        code.into()
                    }
                })?;
        let now = Utc::now();
        let outcome = mutation::apply(&mut candidate, caller, &principal, command, &prepared, now);
        let (changes, result, mut response) = match outcome {
            Ok(applied) => {
                candidate.revision = old
                    .revision
                    .get()
                    .checked_add(1)
                    .and_then(|value| value.try_into().ok())
                    .ok_or_else(|| self.unavailable(ErrorCode::AuthorityUnavailable))?;
                let result = OperationResult {
                    operation_id: command.operation_id().clone(),
                    status: OperationStatus::Applied,
                    authority_revision: Some(candidate.revision),
                    resource_revision: Some(applied.revision),
                    resource_id: Some(applied.resource),
                    error: None,
                };
                let response = applied.issued.map_or_else(
                    || StoredResponse::Operation(result.clone()),
                    |mut issued| {
                        issued.authority_revision = candidate.revision;
                        StoredResponse::Issued(issued)
                    },
                );
                let response = applied
                    .configuration
                    .map_or(response, StoredResponse::Configuration);
                (applied.changes, result, response)
            }
            Err(code) => {
                candidate = (*old).clone();
                let failure = Failure::new(code, Some(command.operation_id().clone())).0;
                let result = OperationResult {
                    operation_id: command.operation_id().clone(),
                    status: OperationStatus::Rejected,
                    authority_revision: Some(old.revision),
                    resource_revision: None,
                    resource_id: None,
                    error: Some(failure.clone()),
                };
                (Vec::new(), result, StoredResponse::Failure(failure))
            }
        };
        if safety {
            candidate.safety_operation_ids += 1;
        } else {
            candidate.operation_ids += 1;
        }
        candidate.saturated |= candidate.operation_ids >= self.limits.ordinary;
        result
            .validate()
            .map_err(|_| self.unavailable(ErrorCode::AuthorityUnavailable))?;
        let operation = Operation {
            principal,
            id: command.operation_id().clone(),
            context: prepared.context,
            digest,
            result,
            response: response.without_secret(),
            created_at: now,
            safety,
        };
        // Allocate the publication before committing. After commit there is no
        // await between replacing the immutable snapshot and acknowledging it.
        let next = Arc::new(candidate);
        if !self.live.0.load(Ordering::Acquire) {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        // The transaction control enters Beginning before its first await.
        // Durable management intent is retained even after the caller deadline.
        if store::commit(&self.db, &next, &changes, &operation, control)
            .await
            .is_err()
        {
            return Err(self.unavailable(ErrorCode::StorageUnavailable));
        }
        if self.proof.disposition() != Disposition::Committed {
            self.proof.quarantine();
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        match self.snapshot.write() {
            Ok(mut state) => *state = next,
            Err(_) => {
                if let StoredResponse::Issued(issued) = &mut response {
                    issued.secret = None;
                }
                return Err(self.unavailable(ErrorCode::AuthorityUnavailable));
            }
        }
        if self.live.0.load(Ordering::Acquire) {
            Ok(response)
        } else {
            Err(ErrorCode::AuthorityUnavailable.into())
        }
    }
}
