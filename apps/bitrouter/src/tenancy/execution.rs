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
        operation::{DispatchBinding, ErrorCode},
        validation::Validate,
    },
    types::ApiProtocol,
};
use chrono::Utc;
use http::HeaderMap;

use super::{
    Service,
    admission::Lease,
    configuration_store,
    error::Failure,
    model::{Caller, Snapshot},
    resources::execution::RetainedRoute,
};
use crate::gateway::{
    caller::CallerContext, pipeline::types::RoutingTarget, providers::auth::credentials::Credential,
};

pub(crate) mod lifecycle;
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
    attempts: Mutex<u8>,
    rejection: Mutex<Option<ErrorCode>>,
    entered: AtomicBool,
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
        qualified && !state.saturated && !self.stop.is_cancelled()
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
        self.with_dispatch_authority(&submission, |_| Ok(()))
            .map_err(Failure::from)?;
        let (route, limits) = {
            let _read = self.read_permit()?;
            let retained = tokio::time::timeout(
                super::MANAGEMENT_TIMEOUT,
                configuration_store::configuration(
                    &self.db,
                    &submission.tenant,
                    &submission.binding.execution_revision,
                ),
            )
            .await
            .map_err(|_| self.unavailable())?
            .map_err(|_| self.unavailable())?
            .ok_or_else(|| Failure::from(ErrorCode::NotFound))?;
            let revision = self
                .resources
                .open_execution(&retained)
                .map_err(|_| self.unavailable())?;
            if revision.installation_id() != &submission.binding.installation_id
                || revision.tenant_id() != &submission.tenant
                || revision.revision_id() != &submission.binding.execution_revision
            {
                return Err(self.unavailable());
            }
            super::configuration::validate_profile(revision.document()).map_err(Failure::from)?;
            let route = revision.select(raw_model, inbound).map_err(Failure::from)?;
            self.with_dispatch_authority(&submission, |state| {
                if retained.authority_revision > state.revision {
                    return Err(ErrorCode::AuthorityUnavailable);
                }
                authorize_route(state, &submission, route.dependencies())
            })
            .map_err(Failure::from)?;
            (route, revision.document().limits.clone())
        };
        let lease = self
            .admission
            .acquire(&submission.tenant, &limits)
            .await
            .map_err(Failure::from)?;
        // Waiting for capacity never extends a token/binding lifetime. The
        // exact submitted revision remains pinned even after activation changes.
        self.with_dispatch_authority(&submission, |state| {
            authorize_route(state, &submission, route.dependencies())
        })
        .map_err(Failure::from)?;
        let execution = Arc::new(TenantExecution {
            service: self.clone(),
            submission,
            id: uuid::Uuid::new_v4()
                .to_string()
                .try_into()
                .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?,
            route,
            attempts: Mutex::new(0),
            rejection: Mutex::new(None),
            entered: AtomicBool::new(false),
            _lease: lease,
        });
        Ok(execution)
    }
}

impl TenantExecution {
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
                authorize_route(state, &self.submission, self.route.dependencies())
            })
    }

    pub(crate) async fn credential(
        &self,
        target: &RoutingTarget,
    ) -> Result<OwnedCredential, ErrorCode> {
        self.revalidate()?;
        let reference = self.route.match_candidate(target)?;
        let _read = self.service.read_permit().map_err(|f| f.0.code)?;
        let credential = tokio::time::timeout(
            super::MANAGEMENT_TIMEOUT,
            self.service
                .resources
                .credential(&self.service.db, &reference),
        )
        .await
        .map_err(|_| self.service.unavailable().0.code)?
        .map_err(|_| self.service.unavailable().0.code)?;
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
