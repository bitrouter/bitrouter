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
    error::Failure,
    model::{Caller, Snapshot},
    resources::execution::{RetainedExecutionRevision, RetainedRoute},
};
use crate::gateway::{
    caller::CallerContext, pipeline::types::RoutingTarget, providers::auth::credentials::Credential,
};

#[cfg(all(feature = "core", feature = "harness"))]
pub(in crate::tenancy) mod block;
pub(super) mod cached;
#[cfg(all(feature = "core", feature = "harness"))]
pub(crate) mod choice;
mod continuation;
mod count;
#[cfg(all(feature = "core", feature = "harness"))]
mod experiment;
#[cfg(all(feature = "core", feature = "harness"))]
pub(crate) mod host;
pub(crate) mod http_continuation;
pub(crate) mod lifecycle;
mod native;
mod oauth;
mod profile;
pub(crate) const BLOCK_LINK_HEADER: &str = "bitrouter-capture-link";
#[cfg(all(feature = "core", feature = "harness"))]
pub(crate) mod session;
#[cfg(test)]
mod tests;

/// Authenticated ingress identity. The tenant and principal come only from the
/// current credential record, never from caller-controlled headers or JSON.
#[derive(Clone)]
pub(crate) struct Submission {
    caller: Caller,
    tenant: TenantId,
    principal: PrincipalId,
    credential: CredentialId,
    binding: DispatchBinding,
    deadline: tokio::time::Instant,
    #[cfg(all(feature = "core", feature = "harness"))]
    session_closed: Option<Arc<AtomicBool>>,
    #[cfg(all(feature = "core", feature = "harness"))]
    block: Option<Arc<super::capture::Binding>>,
}

impl Submission {
    #[cfg(test)]
    pub(in crate::tenancy) fn narrow_deadline_for_test(
        mut self,
        deadline: tokio::time::Instant,
    ) -> Self {
        self.deadline = self.deadline.min(deadline);
        self
    }
    pub(super) fn deadline(&self) -> tokio::time::Instant {
        self.deadline
    }
}

/// One invocation, shared with detached execution, stream draining and delivery.
/// No Clone implementation can duplicate the lease or attempt counter.
pub(crate) struct TenantExecution {
    #[cfg(all(feature = "core", feature = "harness"))]
    experiment: std::sync::OnceLock<Arc<super::experiment::Witness>>,
    #[cfg(all(feature = "core", feature = "harness"))]
    assessment: std::sync::OnceLock<super::learning::source::Binding>,
    service: Service,
    submission: Submission,
    id: ExecutionId,
    route: Arc<RetainedRoute>,
    #[cfg(all(feature = "core", feature = "harness"))]
    block: std::sync::OnceLock<Arc<block::Witness>>,
    #[cfg(feature = "core")]
    learning: bitrouter_ai::tenancy::configuration::Learning,
    journal: std::sync::OnceLock<Arc<super::receipts::journal::Journal>>,
    rejection: Mutex<Option<ErrorCode>>,
    entered: AtomicBool,
    #[cfg(feature = "core")]
    observation: std::sync::OnceLock<Arc<super::eval::observation::RequiredObservation>>,
    continuation: Option<Arc<continuation::Policy>>,
    receipt: std::sync::OnceLock<Arc<super::receipts::owner::Owner>>,
    receipt_ticket: std::sync::OnceLock<lifecycle::AdmissionTicket>,
    #[cfg(all(feature = "core", feature = "harness"))]
    choice: Option<std::sync::OnceLock<Arc<crate::core::session::choice::SendGate>>>,
}

pub(crate) struct OwnedCredential {
    pub(crate) reference: ResourceRef,
    pub(crate) credential: Credential,
}

impl Service {
    pub(super) fn dispatch_ready(&self, state: &Snapshot) -> bool {
        // Production construction consumes the exclusive owned database and
        // completes bootstrap before publication. Unix eligibility still requires
        // every live authority, storage and admission fence below.
        #[cfg(test)]
        let qualified = self.dispatch_test_ready.load(Ordering::Acquire);
        #[cfg(not(test))]
        let qualified = cfg!(unix);
        qualified
            && !state.saturated
            && !self.stop.is_cancelled()
            && !self.caches.is_closed()
            && self.admission.receipts_ready()
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
        #[cfg(all(feature = "core", feature = "harness"))]
        if submission
            .session_closed
            .as_ref()
            .is_some_and(|closed| closed.load(Ordering::Acquire))
        {
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
        #[cfg(all(feature = "core", feature = "harness"))]
        if let Some(binding) = &submission.block {
            binding.block_in(state)?;
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
        #[cfg(not(all(feature = "core", feature = "harness")))]
        if headers.contains_key(BLOCK_LINK_HEADER) {
            return Err(ErrorCode::CapabilityMismatch.into());
        }
        let submission = Submission {
            caller,
            tenant: credential.tenant.clone(),
            principal: credential.principal.clone(),
            credential: credential.id.clone(),
            binding,
            deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(60),
            #[cfg(all(feature = "core", feature = "harness"))]
            session_closed: None,
            #[cfg(all(feature = "core", feature = "harness"))]
            block: one_header(headers, BLOCK_LINK_HEADER)?
                .map(|link| self.linked_capture(link))
                .transpose()?,
        };
        self.with_dispatch_authority(&submission, |_| Ok(()))
            .map_err(Failure::from)?;
        #[cfg(all(feature = "core", feature = "harness"))]
        if submission
            .block
            .as_ref()
            .is_some_and(|binding| !binding.authority().matches_submission(&submission))
        {
            return Err(ErrorCode::Forbidden.into());
        }
        Ok(submission)
    }

    pub(crate) async fn admit_execution(
        &self,
        submission: Submission,
        raw_model: &str,
        inbound: &ApiProtocol,
    ) -> Result<Arc<TenantExecution>, Failure> {
        self.admit_selected_execution(submission, false, |revision| {
            revision.select(raw_model, inbound)
        })
        .await
    }

    async fn admit_native_execution(
        &self,
        submission: Submission,
        raw_model: &str,
    ) -> Result<Arc<TenantExecution>, Failure> {
        self.admit_selected_execution(submission, false, |revision| {
            revision.select_native(raw_model, bitrouter_ai::types::ModelOperation::Generation)
        })
        .await
    }

    async fn admit_selected_execution(
        &self,
        submission: Submission,
        fail_fast: bool,
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
        let mut limits = revision.document().limits.clone();
        if fail_fast {
            limits.queued_work = 0;
        }
        route.require_capabilities().map_err(Failure::from)?;
        #[cfg(all(feature = "core", feature = "harness"))]
        if route.block_profile().is_some() != submission.block.is_some()
            || submission
                .block
                .as_ref()
                .is_some_and(|binding| binding.block_route() != Some(route.reference()))
        {
            return Err(ErrorCode::Forbidden.into());
        }
        #[cfg(feature = "core")]
        let learning = revision.document().learning.clone();
        drop(revision);
        let free = super::receipts::capacity::free_space(self.receipt_volume.as_deref())
            .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
        self.admission.receipt_probe(free);
        let lease = self
            .admission
            .acquire_until(&submission.tenant, &limits, submission.deadline)
            .await
            .map_err(Failure::from)?;
        self.finish_admission(
            submission,
            route,
            lease,
            #[cfg(feature = "core")]
            learning,
            #[cfg(all(feature = "core", feature = "harness"))]
            false,
        )
    }

    fn finish_admission(
        &self,
        submission: Submission,
        route: Arc<RetainedRoute>,
        lease: super::admission::Lease,
        #[cfg(feature = "core")] learning: bitrouter_ai::tenancy::configuration::Learning,
        #[cfg(all(feature = "core", feature = "harness"))] choice: bool,
    ) -> Result<Arc<TenantExecution>, Failure> {
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
            #[cfg(all(feature = "core", feature = "harness"))]
            experiment: std::sync::OnceLock::new(),
            #[cfg(all(feature = "core", feature = "harness"))]
            assessment: std::sync::OnceLock::new(),
            service: self.clone(),
            submission,
            id,
            continuation,
            route,
            #[cfg(all(feature = "core", feature = "harness"))]
            block: std::sync::OnceLock::new(),
            #[cfg(feature = "core")]
            learning,
            journal: std::sync::OnceLock::new(),
            rejection: Mutex::new(None),
            entered: AtomicBool::new(false),
            #[cfg(feature = "core")]
            observation: std::sync::OnceLock::new(),
            receipt: std::sync::OnceLock::new(),
            receipt_ticket: std::sync::OnceLock::new(),
            #[cfg(all(feature = "core", feature = "harness"))]
            choice: choice.then(std::sync::OnceLock::new),
        });
        execution
            .journal
            .set(super::receipts::journal::Journal::new(&execution))
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        let receipt = super::receipts::owner::Owner::new(
            super::receipts::owner::Identity {
                installation: execution.submission.binding.installation_id.clone(),
                tenant: execution.submission.tenant.clone(),
                principal: execution.submission.principal.clone(),
                execution: execution.id.clone(),
                correlation: execution.submission.binding.correlation_id.clone(),
                revision: execution.submission.binding.execution_revision.clone(),
            },
            execution.journal().map_err(Failure::from)?.clone(),
            self.completions.clone(),
            Arc::new(lease),
            self.admission.clone(),
            self.cleanup.clone(),
            (
                execution.submission.deadline,
                self.receipt_observer
                    .timing(&execution.submission.binding.correlation_id),
            ),
        );
        let ticket = self
            .pipelines
            .admit(receipt.clone())
            .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
        execution
            .receipt
            .set(receipt.clone())
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        execution
            .receipt_ticket
            .set(ticket)
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        receipt.observation.admitted();
        Ok(execution)
    }
}

impl TenantExecution {
    pub(crate) fn receipt_owner(
        &self,
    ) -> crate::gateway::error::Result<&Arc<super::receipts::owner::Owner>> {
        self.receipt.get().ok_or_else(|| {
            crate::gateway::error::BitrouterError::internal("admitted receipt owner missing")
        })
    }
    pub(crate) async fn terminal_receipt(
        &self,
        outcome: bitrouter_ai::tenancy::execution::Outcome,
    ) -> crate::gateway::error::Result<()> {
        self.receipt_owner()?.finish(outcome).await.map_err(|code| {
            self.reject(code);
            crate::gateway::error::BitrouterError::UpstreamUnavailable
        })
    }
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
                #[cfg(all(feature = "core", feature = "harness"))]
                if let Some(binding) = &self.submission.block {
                    binding.block_in(state)?;
                }
                #[cfg(all(feature = "core", feature = "harness"))]
                if let Some(binding) = self.assessment.get() {
                    binding.assessment_in(state)?;
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
        #[cfg(test)]
        self.service
            .receipt_credential_read_for_test(&self.id)
            .await;
        self.revalidate()?;
        Ok(OwnedCredential {
            reference,
            credential,
        })
    }

    pub(crate) fn journal(&self) -> Result<&Arc<super::receipts::journal::Journal>, ErrorCode> {
        self.journal.get().ok_or(ErrorCode::AuthorityUnavailable)
    }

    /// Exact static identity is validated and a slot is reserved before auth.
    pub(crate) fn reserve_attempt(
        &self,
        target: &RoutingTarget,
    ) -> Result<super::receipts::journal::AttemptPermit, ErrorCode> {
        self.revalidate()?;
        let stamp = super::receipts::journal::TargetStamp::for_execution(self, target)
            .map_err(super::receipts::journal_error)?;
        self.journal()?
            .reserve(stamp)
            .map_err(super::receipts::journal_error)
    }

    /// The last synchronous current-grant check and arming share one authority lock.
    #[cfg(test)]
    pub(crate) fn arm_attempt(
        &self,
        target: &RoutingTarget,
        permit: &mut super::receipts::journal::AttemptPermit,
        authenticated: &super::receipts::journal::TargetStamp,
    ) -> Result<AttemptId, ErrorCode> {
        self.arm_attempt_with_oauth(target, permit, authenticated, None, None)
    }

    pub(crate) fn arm_attempt_with_oauth(
        &self,
        target: &RoutingTarget,
        permit: &mut super::receipts::journal::AttemptPermit,
        authenticated: &super::receipts::journal::TargetStamp,
        oauth: Option<&super::oauth::ModelStamp>,
        counted: Option<&str>,
    ) -> Result<AttemptId, ErrorCode> {
        let actual = super::receipts::journal::TargetStamp::for_execution(self, target)
            .map_err(super::receipts::journal_error)?;
        if !actual.matches(authenticated) {
            return Err(ErrorCode::Forbidden);
        }
        self.service
            .with_dispatch_authority(&self.submission, |state| {
                let rejection = self
                    .rejection
                    .lock()
                    .map_err(|_| ErrorCode::AuthorityUnavailable)?;
                if let Some(code) = *rejection {
                    return Err(code);
                }
                #[cfg(all(feature = "core", feature = "harness"))]
                if let Some(binding) = &self.submission.block {
                    binding.block_in(state)?;
                }
                #[cfg(all(feature = "core", feature = "harness"))]
                if let Some(binding) = self.assessment.get() {
                    binding.assessment_in(state)?;
                }
                authorize_route(state, &self.submission, self.route.dependencies())?;
                #[cfg(all(feature = "core", feature = "harness"))]
                self.block_send(target)?;
                self.validate_oauth_model(target, oauth)?;
                self.verify_count_send(target, counted)?;
                permit
                    .arm(authenticated)
                    .map_err(super::receipts::journal_error)
            })
    }

    #[cfg(test)]
    pub(crate) fn admit_send(&self, target: &RoutingTarget) -> Result<AttemptId, ErrorCode> {
        let mut permit = self.reserve_attempt(target)?;
        let stamp = permit.stamp().clone();
        self.arm_attempt(target, &mut permit, &stamp)
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
        let code = self.receipt.get().and_then(|owner| owner.failure()).or(
            match self.rejection.lock() {
                Ok(rejection) => *rejection,
                Err(_) => Some(ErrorCode::AuthorityUnavailable),
            },
        )?;
        let mut failure = Failure::new(code, None);
        failure.0.execution_id = Some(self.id.clone());
        failure.0.dispatch_state = self
            .journal()
            .and_then(|journal| journal.dispatch().map_err(super::receipts::journal_error))
            .unwrap_or(DispatchState::Unknown);
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
