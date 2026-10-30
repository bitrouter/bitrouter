//! Authenticated manual ledger operations and admitted execution observations.

pub(super) mod http;
pub(crate) mod observation;
mod wire;

use super::{
    Service, actor,
    error::Failure,
    model::{Caller, Snapshot},
    transaction::Proof,
};
use bitrouter_ai::{
    eval::{EvalSubject, EvaluationResult},
    tenancy::{
        authority::{GrantScope, Permission},
        identity::{CredentialId, InstallationId, PrincipalId, TenantId},
        operation::ErrorCode,
    },
};
use chrono::Utc;
use sea_orm::DatabaseConnection;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};
use tokio::time::Instant;

pub(super) const WORK: Duration = Duration::from_secs(5);
pub(super) const BODY_BYTES: usize = 1024 * 1024;
pub(super) const RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub(super) struct State {
    pub leases: Arc<Semaphore>,
    pub producers_open: AtomicBool,
    failed: AtomicBool,
    live: Arc<AtomicBool>,
    #[cfg(test)]
    pub producer_waiting: tokio::sync::Notify,
    #[cfg(test)]
    pub handoff: std::sync::Mutex<Option<Arc<Handoff>>>,
}
impl State {
    pub fn new(live: Arc<AtomicBool>) -> Arc<Self> {
        Arc::new(Self {
            leases: Arc::new(Semaphore::new(8)),
            producers_open: AtomicBool::new(true),
            failed: AtomicBool::new(false),
            live,
            #[cfg(test)]
            producer_waiting: tokio::sync::Notify::new(),
            #[cfg(test)]
            handoff: std::sync::Mutex::new(None),
        })
    }
    pub fn fail(&self) {
        self.failed.store(true, Ordering::Release);
        self.live.store(false, Ordering::Release);
    }
    pub fn healthy(&self) -> bool {
        !self.failed.load(Ordering::Acquire)
    }
}

#[cfg(test)]
#[derive(Default)]
pub(super) struct Handoff {
    pub entered: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}

/// Sealed identity captures origin and namespace, never durable entitlement.
pub(crate) struct EvalIdentity {
    credential: CredentialId,
    principal: PrincipalId,
    installation: InstallationId,
    tenant: TenantId,
    origin: Arc<Proof>,
    db: DatabaseConnection,
}
impl EvalIdentity {
    pub(crate) fn database(&self) -> &DatabaseConnection {
        &self.db
    }
    pub(crate) fn storage_identity(&self) -> (&InstallationId, &TenantId, &PrincipalId) {
        (&self.installation, &self.tenant, &self.principal)
    }
    pub(super) fn check(
        &self,
        state: &Snapshot,
        origin: &Arc<Proof>,
        action: Action,
    ) -> Result<(), ErrorCode> {
        if !Arc::ptr_eq(origin, &self.origin) || state.installation != self.installation {
            return Err(ErrorCode::CapabilityMismatch);
        }
        let caller = Caller::Credential(self.credential.clone());
        let effective = state.effective(&caller, &self.tenant, Utc::now())?;
        if effective.credential.principal != self.principal
            || !effective.covers(&GrantScope::Tenant {
                operator_pools: Vec::new(),
            })
            || !effective.permits(action.permission())
            || (action == Action::Freeze && !effective.permits(Permission::ResourceRead))
        {
            return Err(ErrorCode::Forbidden);
        }
        if action != Action::Read && state.saturated {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    Read,
    Submit,
    Freeze,
}
impl Action {
    fn permission(self) -> Permission {
        match self {
            Self::Read => Permission::ResourceRead,
            Self::Submit | Self::Freeze => Permission::EvaluationSubmit,
        }
    }
}

pub(super) enum Request {
    Put(EvalSubject),
    Subjects,
    Subject(String),
    Submit(EvaluationResult),
    Freeze(Option<String>),
    Snapshot(String),
    Status,
}
impl Request {
    pub fn validate(&self, identity: &EvalIdentity) -> Result<(), Failure> {
        match self {
            Self::Put(subject) => {
                wire::size(subject, BODY_BYTES)
                    .map_err(|_| Failure::from(ErrorCode::PayloadTooLarge))?;
                crate::storage::evaluation::validate_subject(subject)
                    .map_err(|_| ErrorCode::InvalidRequest.into())
            }
            Self::Submit(result) => {
                wire::size(result, BODY_BYTES)
                    .map_err(|_| Failure::from(ErrorCode::PayloadTooLarge))?;
                if result.evaluator.kind != bitrouter_ai::eval::EvaluatorKind::Generic
                    || result.evaluator.authority_id != identity.principal.as_str()
                    || !result.hard_violations.is_empty()
                {
                    return Err(ErrorCode::Forbidden.into());
                }
                crate::storage::evaluation::validate_result(result)
                    .map_err(|_| ErrorCode::InvalidRequest.into())
            }
            Self::Freeze(Some(time)) => chrono::DateTime::parse_from_rfc3339(time)
                .map(|_| ())
                .map_err(|_| ErrorCode::InvalidRequest.into()),
            _ => Ok(()),
        }
    }
    pub fn action(&self) -> Action {
        match self {
            Self::Put(_) | Self::Submit(_) => Action::Submit,
            Self::Freeze(_) => Action::Freeze,
            _ => Action::Read,
        }
    }
}

pub(super) struct Job {
    pub identity: Arc<EvalIdentity>,
    pub request: Request,
    pub deadline: Instant,
    pub lease: OwnedSemaphorePermit,
    pub reply: oneshot::Sender<Result<Reply, Failure>>,
}
pub(super) struct Reply {
    pub body: Vec<u8>,
    pub lease: OwnedSemaphorePermit,
}
pub(super) struct Settlement {
    pub execution: Arc<super::execution::TenantExecution>,
    pub subject: Arc<EvalSubject>,
    pub observation: Arc<observation::RequiredObservation>,
    pub deadline: Instant,
    pub reply: oneshot::Sender<Result<(), Failure>>,
}

impl Service {
    pub(super) fn eval_identity(
        &self,
        headers: &::http::HeaderMap,
        tenant: TenantId,
        action: Action,
    ) -> Result<Arc<EvalIdentity>, Failure> {
        wire::headers(headers)?;
        let one = super::http::one_header;
        one(headers, "origin")?;
        one(headers, "host")?;
        if headers.contains_key("x-api-key")
            || headers.contains_key(::http::header::CONTENT_ENCODING)
        {
            return Err(ErrorCode::InvalidRequest.into());
        }
        if !crate::remote_control::origin_matches_host(headers) {
            return Err(ErrorCode::Forbidden.into());
        }
        let installation: InstallationId = one(headers, "bitrouter-installation-id")?
            .ok_or(ErrorCode::InvalidRequest)?
            .try_into()
            .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
        let authorization = one(headers, "authorization")?.ok_or(ErrorCode::Unauthorized)?;
        let (scheme, token) = authorization
            .split_once(' ')
            .ok_or(ErrorCode::Unauthorized)?;
        if !scheme.eq_ignore_ascii_case("bearer") || token.bytes().any(|b| b.is_ascii_whitespace())
        {
            return Err(ErrorCode::Unauthorized.into());
        }
        let caller = self.authenticate(token)?;
        let state = self.current()?;
        if state.installation != installation {
            return Err(ErrorCode::CapabilityMismatch.into());
        }
        let effective = state
            .effective(&caller, &tenant, Utc::now())
            .map_err(Failure::from)?;
        let identity = Arc::new(EvalIdentity {
            credential: effective.credential.id.clone(),
            principal: effective.credential.principal.clone(),
            installation,
            tenant,
            origin: self.cleanup.clone(),
            db: self.db.clone(),
        });
        identity
            .check(&state, &self.cleanup, action)
            .map_err(Failure::from)?;
        Ok(identity)
    }

    pub(super) fn check_eval(
        &self,
        identity: &EvalIdentity,
        action: Action,
    ) -> Result<(), Failure> {
        let state = self.current()?;
        identity
            .check(&state, &self.cleanup, action)
            .map_err(Failure::from)
    }

    pub(super) fn eval_lease(&self) -> Result<OwnedSemaphorePermit, Failure> {
        let accepting = self
            .accepting
            .lock()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        if !*accepting || !self.eval.healthy() {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        self.eval
            .leases
            .clone()
            .try_acquire_owned()
            .map_err(|_| ErrorCode::CapacityExceeded.into())
    }

    pub(super) fn enqueue_eval(&self, job: Job) -> Result<(), Failure> {
        let accepting = self
            .accepting
            .lock()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        if !*accepting || !self.eval.healthy() {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        self.check_eval(&job.identity, job.request.action())?;
        self.commands
            .try_send(actor::Job::Eval(Box::new(job)))
            .map_err(|error| match error {
                tokio::sync::mpsc::error::TrySendError::Full(_) => {
                    ErrorCode::CapacityExceeded.into()
                }
                tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                    ErrorCode::AuthorityUnavailable.into()
                }
            })
    }
}

pub(super) fn encode(value: &impl serde::Serialize) -> Result<Vec<u8>, Failure> {
    struct Writer(Vec<u8>);
    impl std::io::Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > RESPONSE_BYTES.saturating_sub(self.0.len()) {
                return Err(std::io::Error::other(
                    "owned evaluation response exceeds bound",
                ));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Writer(Vec::new());
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
    Ok(writer.0)
}
