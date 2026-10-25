//! Request-local authority for one exact retained execution revision.
//!
//! Public caller metadata is never a proof. Only this module can construct an
//! execution, after authenticating its credential and acquiring bounded capacity.
//! Current authority is checked again at every provider boundary; finalization
//! retains ownership without granting permission to start more provider work.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use bitrouter_ai::{
    tenancy::{
        authority::{GrantScope, OwnerKind, Permission, ResourceKind},
        configuration::ResourceRef,
        identity::{
            AttemptId, CredentialId, ExecutionId, InstallationId, PrincipalId, RevisionId, TenantId,
        },
        operation::{DispatchBinding, DispatchState, ErrorCode},
        validation::Validate,
    },
    types::ApiProtocol,
};
use chrono::Utc;
use http::HeaderMap;

use super::{
    Service,
    admission::Lease,
    error::Failure,
    model::{Caller, Snapshot},
    resources::execution::{RetainedExecutionRevision, RetainedRoute},
};
use crate::gateway::{
    caller::CallerContext, pipeline::types::RoutingTarget, providers::auth::credentials::Credential,
};

pub(super) mod cached;
mod continuation;
pub(crate) mod lifecycle;
mod native;
mod profile;
#[cfg(test)]
mod tests;

const MAX_ATTEMPTS: u8 = 8;

/// Authenticated ingress identity. The tenant and principal come only from the
/// current credential record, never from caller-controlled headers or JSON.
pub(crate) struct Submission {
    caller: Caller,
    tenant: TenantId,
    principal: PrincipalId,
    credential: CredentialId,
    binding: DispatchBinding,
    deadline: tokio::time::Instant,
}

impl Submission {
    pub(super) fn deadline(&self) -> tokio::time::Instant {
        self.deadline
    }
}

/// One invocation, shared with detached execution, stream draining and delivery.
/// No Clone implementation can duplicate the lease or attempt counter.
pub(crate) struct TenantExecution {
    service: Service,
    submission: Submission,
    id: ExecutionId,
    route: Arc<RetainedRoute>,
    #[cfg(feature = "core")]
    learning: bitrouter_ai::tenancy::configuration::Learning,
    attempts: Mutex<u8>,
    rejection: Mutex<Option<ErrorCode>>,
    entered: AtomicBool,
    #[cfg(feature = "core")]
    observation: std::sync::OnceLock<Arc<super::eval::observation::RequiredObservation>>,
    continuation: Option<Arc<continuation::Policy>>,
    _lease: Lease,
}

pub(crate) struct OwnedCredential {
    pub(crate) reference: ResourceRef,
    pub(crate) credential: Credential,
}

impl Service {
    pub(super) fn dispatch_ready(&self, state: &Snapshot) -> bool {
        // Positive dispatch exists only in unit/integration fixtures until the
        // strict receipt and ownership stages are implemented and qualified.
        #[cfg(test)]
        let qualified = self.dispatch_test_ready.load(Ordering::Acquire);
        #[cfg(not(test))]
        let qualified = false;
        qualified && !state.saturated && !self.stop.is_cancelled() && !self.caches.is_closed()
    }

    pub(super) fn capability_revision(&self, state: &Snapshot) -> Result<RevisionId, ErrorCode> {
        format!(
            "{}-{}-{}",
            self.instance,
            u8::from(state.saturated),
            u8::from(self.dispatch_ready(state))
        )
        .try_into()
        .map_err(|_| ErrorCode::AuthorityUnavailable)
    }

    /// This lock spans eligibility and authorization. A snapshot obtained before
    /// a revoke acknowledgement is never reused to authorize a later attempt.
    fn with_dispatch_authority<T>(
        &self,
        submission: &Submission,
        check: impl FnOnce(&Snapshot) -> Result<T, ErrorCode>,
    ) -> Result<T, ErrorCode> {
        let guard = self
            .snapshot
            .read()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if !self.live.load(Ordering::Acquire) {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        let state = guard.as_ref();
        if submission.deadline <= tokio::time::Instant::now() {
            return Err(ErrorCode::CapacityExceeded);
        }
        if !self.dispatch_ready(state)
            || submission.binding.installation_id != state.installation
            || submission.binding.instance_id != self.instance
            || submission.binding.capability_revision != self.capability_revision(state)?
        {
            return Err(ErrorCode::CapabilityMismatch);
        }
        let effective = state.effective(&submission.caller, &submission.tenant, Utc::now())?;
        if effective.credential.principal != submission.principal
            || effective.credential.id != submission.credential
            || !effective.permits(Permission::ExecutionSubmit)
        {
            return Err(ErrorCode::Forbidden);
        }
        check(state)
    }

    pub(crate) fn submission(&self, headers: &HeaderMap) -> Result<Submission, Failure> {
        let binding = dispatch_binding(headers)?;
        if headers.contains_key("x-api-key")
            || headers.contains_key("bitrouter-tenant-id")
            || headers.contains_key("bitrouter-principal-id")
        {
            return Err(ErrorCode::InvalidRequest.into());
        }
        let authorization = one_header(headers, "authorization")?
            .ok_or_else(|| Failure::from(ErrorCode::Unauthorized))?;
        let (scheme, token) = authorization
            .split_once(' ')
            .ok_or_else(|| Failure::from(ErrorCode::Unauthorized))?;
        if !scheme.eq_ignore_ascii_case("bearer")
            || token.is_empty()
            || token.bytes().any(|b| b.is_ascii_whitespace())
        {
            return Err(ErrorCode::Unauthorized.into());
        }
        let caller = self.authenticate(token)?;
        let state = self.current()?;
        let credential = state
            .credential(&caller, Utc::now())
            .map_err(Failure::from)?;
        let submission = Submission {
            caller,
            tenant: credential.tenant.clone(),
            principal: credential.principal.clone(),
            credential: credential.id.clone(),
            binding,
            deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(60),
        };
        self.with_dispatch_authority(&submission, |_| Ok(()))
            .map_err(Failure::from)?;
        Ok(submission)
    }

    pub(crate) async fn admit_execution(
        &self,
        submission: Submission,
        raw_model: &str,
        inbound: &ApiProtocol,
    ) -> Result<Arc<TenantExecution>, Failure> {
        self.admit_selected_execution(submission, |revision| revision.select(raw_model, inbound))
            .await
    }

    async fn admit_native_execution(
        &self,
        submission: Submission,
        raw_model: &str,
    ) -> Result<Arc<TenantExecution>, Failure> {
        self.admit_selected_execution(submission, |revision| {
            revision.select_native(raw_model, bitrouter_ai::types::ModelOperation::Generation)
        })
        .await
    }

    async fn admit_selected_execution(
        &self,
        submission: Submission,
        select: impl FnOnce(
            &super::cache::Pin<RetainedExecutionRevision>,
        ) -> Result<Arc<RetainedRoute>, ErrorCode>,
    ) -> Result<Arc<TenantExecution>, Failure> {
        self.with_dispatch_authority(&submission, |_| Ok(()))
            .map_err(Failure::from)?;
        let revision = self.retained_execution(&submission).await?;
        let route = select(&revision).map_err(Failure::from)?;
        self.with_dispatch_authority(&submission, |state| {
            authorize_route(state, &submission, route.dependencies())
        })
        .map_err(Failure::from)?;
        let limits = revision.document().limits.clone();
        route.require_capabilities().map_err(Failure::from)?;
        #[cfg(feature = "core")]
        let learning = revision.document().learning.clone();
        drop(revision);
        let lease = self
            .admission
            .acquire_until(&submission.tenant, &limits, submission.deadline)
            .await
            .map_err(Failure::from)?;
        // Waiting for capacity never extends a token/binding lifetime. The
        // exact submitted revision remains pinned even after activation changes.
        self.with_dispatch_authority(&submission, |state| {
            authorize_route(state, &submission, route.dependencies())
        })
        .map_err(Failure::from)?;
        let id = uuid::Uuid::new_v4()
            .to_string()
            .try_into()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        let continuation = self.native_continuation.clone().map(|crypto| {
            Arc::new(continuation::Policy::new(
                crypto,
                &submission,
                &id,
                route.clone(),
            ))
        });
        let execution = Arc::new(TenantExecution {
            service: self.clone(),
            submission,
            id,
            continuation,
            route,
            #[cfg(feature = "core")]
            learning,
            attempts: Mutex::new(0),
            rejection: Mutex::new(None),
            entered: AtomicBool::new(false),
            #[cfg(feature = "core")]
            observation: std::sync::OnceLock::new(),
            _lease: lease,
        });
        Ok(execution)
    }
}

impl TenantExecution {
    pub(crate) fn record_native_source(
        &self,
        proof: crate::gateway::pipeline::executor::tenancy::native::Proof,
    ) {
        if let Some(policy) = &self.continuation {
            policy.record(proof);
        }
    }

    #[cfg(feature = "core")]
    pub(in crate::tenancy) fn send_trajectory(
        &self,
        permit: tokio::sync::mpsc::OwnedPermit<super::actor::Job>,
        job: super::actor::Job,
        deadline: tokio::time::Instant,
    ) -> Result<(), ErrorCode> {
        let _gate = self
            .service
            .accepting
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if !self.trajectory_producer_ready() || tokio::time::Instant::now() >= deadline {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        drop(permit.send(job));
        Ok(())
    }

    #[cfg(feature = "core")]
    pub(crate) fn storage_database(&self) -> &sea_orm::DatabaseConnection {
        &self.service.db
    }
    #[cfg(feature = "core")]
    pub(in crate::tenancy) fn trajectory_identity(
        &self,
    ) -> (
        &bitrouter_ai::tenancy::identity::RevisionId,
        &bitrouter_ai::tenancy::configuration::Learning,
    ) {
        (&self.submission.binding.execution_revision, &self.learning)
    }
    #[cfg(feature = "core")]
    pub(in crate::tenancy) fn trajectory_resources(&self) -> &super::resources::Resources {
        &self.service.resources
    }
    #[cfg(all(test, feature = "core"))]
    pub(in crate::tenancy) fn trajectory_observation(
        &self,
    ) -> Option<&Arc<super::eval::observation::RequiredObservation>> {
        self.observation.get()
    }
    #[cfg(feature = "core")]
    pub(in crate::tenancy) fn trajectory_sender(
        &self,
    ) -> tokio::sync::mpsc::Sender<super::actor::Job> {
        self.service.commands.clone()
    }
    #[cfg(feature = "core")]
    pub(in crate::tenancy) fn trajectory_producer_ready(&self) -> bool {
        self.service.eval.producers_open.load(Ordering::Acquire)
            && self.service.eval.healthy()
            && self.service.live.load(Ordering::Acquire)
    }

    #[cfg(feature = "core")]
    pub(crate) fn storage_identity(&self) -> (&InstallationId, &TenantId, &PrincipalId) {
        (
            &self.submission.binding.installation_id,
            &self.submission.tenant,
            &self.submission.principal,
        )
    }

    #[cfg(feature = "core")]
    pub(crate) fn eval_store(&self) -> crate::core::eval::store::EvalStore {
        crate::core::eval::store::EvalStore::for_execution(self.service.db.clone(), self)
    }

    #[cfg(feature = "core")]
    pub(super) fn eval_origin(&self, origin: &Arc<super::transaction::Proof>) -> bool {
        Arc::ptr_eq(origin, &self.service.cleanup)
    }

    #[cfg(feature = "core")]
    pub(super) fn observation_matches(
        &self,
        observation: &Arc<super::eval::observation::RequiredObservation>,
    ) -> bool {
        self.observation
            .get()
            .is_some_and(|own| Arc::ptr_eq(own, observation))
    }

    #[cfg(feature = "core")]
    pub(crate) async fn persist_observation(
        self: &Arc<Self>,
        observation: Arc<super::eval::observation::RequiredObservation>,
        subject: Arc<bitrouter_ai::eval::EvalSubject>,
        deadline: tokio::time::Instant,
    ) -> crate::gateway::error::Result<()> {
        self.service
            .persist_observation(self.clone(), observation, subject, deadline)
            .await
    }

    pub(crate) fn installation_id(&self) -> &InstallationId {
        &self.submission.binding.installation_id
    }

    pub(crate) fn id(&self) -> &ExecutionId {
        &self.id
    }

    pub(crate) fn remaining(&self) -> std::time::Duration {
        self.submission
            .deadline
            .saturating_duration_since(tokio::time::Instant::now())
    }

    pub(crate) fn caller(&self) -> CallerContext {
        CallerContext::new(
            self.submission.credential.as_str(),
            self.submission.principal.as_str(),
        )
    }

    pub(crate) fn route(&self) -> &Arc<RetainedRoute> {
        &self.route
    }

    pub(crate) fn revalidate(&self) -> Result<(), ErrorCode> {
        self.service
            .with_dispatch_authority(&self.submission, |state| {
                let rejection = self
                    .rejection
                    .lock()
                    .map_err(|_| ErrorCode::AuthorityUnavailable)?;
                if let Some(code) = *rejection {
                    return Err(code);
                }
                authorize_route(state, &self.submission, self.route.dependencies())
            })
    }

    pub(crate) async fn credential(
        &self,
        target: &RoutingTarget,
    ) -> Result<OwnedCredential, ErrorCode> {
        self.revalidate()?;
        let reference = self.route.match_candidate(target)?;
        let credential = self
            .service
            .retained_credential(&self.submission, &reference)
            .await
            .map_err(|failure| failure.0.code)?;
        self.revalidate()?;
        Ok(OwnedCredential {
            reference,
            credential,
        })
    }

    /// Call immediately before an actual send, after auth, waits and refresh.
    /// Current grants and a fresh attempt reservation share one linearization.
    pub(crate) fn admit_send(&self, target: &RoutingTarget) -> Result<AttemptId, ErrorCode> {
        self.route.match_candidate(target)?;
        self.service
            .with_dispatch_authority(&self.submission, |state| {
                // Hold the failure latch through the reservation. Outcome
                // cleanup may still run after rejection, but no new send may
                // cross this gate once a prior callback has failed.
                let rejection = self
                    .rejection
                    .lock()
                    .map_err(|_| ErrorCode::AuthorityUnavailable)?;
                if let Some(code) = *rejection {
                    return Err(code);
                }
                authorize_route(state, &self.submission, self.route.dependencies())?;
                let mut attempts = self
                    .attempts
                    .lock()
                    .map_err(|_| ErrorCode::AuthorityUnavailable)?;
                if *attempts >= MAX_ATTEMPTS {
                    return Err(ErrorCode::CapacityExceeded);
                }
                let attempt_id = uuid::Uuid::new_v4()
                    .to_string()
                    .try_into()
                    .map_err(|_| ErrorCode::AuthorityUnavailable)?;
                *attempts += 1;
                Ok(attempt_id)
            })
    }

    /// Authority failures stop fallback and remain available to the HTTP layer
    /// as typed tenancy failures without exposing secret-bearing diagnostics.
    pub(crate) fn reject(&self, code: ErrorCode) -> crate::gateway::error::BitrouterError {
        if let Ok(mut rejection) = self.rejection.lock() {
            rejection.get_or_insert(code);
        }
        crate::gateway::error::BitrouterError::Forbidden(
            "tenant execution is no longer eligible".into(),
        )
    }

    /// Keep internal eligibility failures non-retryable, while exposing their
    /// actual tenancy class at HTTP ingress. A reserved send cannot prove that
    /// no provider work began; full per-attempt evidence is a separate stage.
    pub(crate) fn failure(&self) -> Option<Failure> {
        let code = match self.rejection.lock() {
            Ok(rejection) => *rejection,
            Err(_) => Some(ErrorCode::AuthorityUnavailable),
        }?;
        let mut failure = Failure::new(code, None);
        failure.0.execution_id = Some(self.id.clone());
        failure.0.dispatch_state = match self.attempts.lock() {
            Ok(attempts) if *attempts == 0 => DispatchState::NotStarted,
            _ => DispatchState::Unknown,
        };
        Some(failure)
    }
}

fn authorize_route(
    state: &Snapshot,
    submission: &Submission,
    dependencies: &[ResourceRef],
) -> Result<(), ErrorCode> {
    let effective = state.effective(&submission.caller, &submission.tenant, Utc::now())?;
    if dependencies.is_empty() {
        return Err(ErrorCode::Forbidden);
    }
    for reference in dependencies {
        if reference.owner_kind == OwnerKind::Tenant
            && reference.owner_id.as_str() != submission.tenant.as_str()
        {
            return Err(ErrorCode::Forbidden);
        }
        let permission = if reference.resource_kind == ResourceKind::Credential {
            Permission::CredentialUse
        } else {
            Permission::ExecutionSubmit
        };
        if !effective.permits(permission)
            || !effective.covers(&GrantScope::Resources {
                resources: vec![reference.key()],
            })
        {
            return Err(ErrorCode::Forbidden);
        }
    }
    Ok(())
}

fn one_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, Failure> {
    let mut values = headers.get_all(name).iter();
    let value = values
        .next()
        .map(|v| v.to_str())
        .transpose()
        .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    if values.next().is_some() {
        return Err(ErrorCode::InvalidRequest.into());
    }
    Ok(value)
}

fn dispatch_binding(headers: &HeaderMap) -> Result<DispatchBinding, Failure> {
    let mut document = serde_json::Map::new();
    for name in [
        "Bitrouter-Tenancy-Version",
        "Bitrouter-Installation-Id",
        "Bitrouter-Instance-Id",
        "Bitrouter-Capability-Revision",
        "Bitrouter-Execution-Revision",
        "Bitrouter-Correlation-Id",
        "Bitrouter-Receipt-Mode",
    ] {
        let value =
            one_header(headers, name)?.ok_or_else(|| Failure::from(ErrorCode::InvalidRequest))?;
        document.insert(name.into(), value.into());
    }
    let binding: DispatchBinding = serde_json::from_value(document.into())
        .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    binding
        .validate()
        .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    Ok(binding)
}
