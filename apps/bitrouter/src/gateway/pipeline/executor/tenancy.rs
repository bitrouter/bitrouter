//! Private tenant authentication and actual-send gates. Shared authority and
//! ordinary local callers keep their existing paths and credential registries.

use std::sync::Arc;

use bitrouter_ai::tenancy::operation::ErrorCode;
use sha2::{Digest, Sha256};

use super::{AppliedAuth, PipelineContext, Result, RoutingTarget};
use crate::{
    gateway::{
        model::protocol::Transport,
        providers::auth::{
            AuthOperation, CredentialAuthority, credentials::Credential,
            normalize_auth_extension_error,
        },
    },
    tenancy::execution::TenantExecution,
};

pub(crate) mod http_continuation;
pub(crate) mod native;

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub(super) struct BoundAuthentication {
    execution: Arc<TenantExecution>,
    digest: [u8; 32],
    oauth: Option<crate::tenancy::oauth::ModelStamp>,
    stamp: crate::tenancy::receipts::journal::TargetStamp,
    candidate: Vec<u8>,
    credential: bitrouter_ai::tenancy::configuration::ResourceRef,
}

/// The private proof travels beside AppliedAuth so no public transport contract
/// grows an application service handle or accepts a caller-created capability.
pub(super) struct AuthenticatedRequest {
    applied: AppliedAuth,
    binding: Option<BoundAuthentication>,
    reservation: Option<crate::tenancy::receipts::journal::AttemptPermit>,
}

impl std::ops::Deref for AuthenticatedRequest {
    type Target = AppliedAuth;
    fn deref(&self) -> &Self::Target {
        &self.applied
    }
}

impl AuthenticatedRequest {
    pub(super) fn new_reserved(
        applied: AppliedAuth,
        target: &RoutingTarget,
        ctx: &PipelineContext,
        url: &str,
        reservation: Option<crate::tenancy::receipts::journal::AttemptPermit>,
        oauth: Option<crate::tenancy::oauth::ModelStamp>,
    ) -> Result<Self> {
        let binding = execution(ctx)
            .map(|execution| {
                execution
                    .revalidate()
                    .map_err(|code| execution.reject(code))?;
                let stamp = reservation
                    .as_ref()
                    .ok_or_else(|| execution.reject(ErrorCode::Forbidden))?
                    .stamp()
                    .clone();
                let actual = crate::tenancy::receipts::journal::TargetStamp::for_execution(
                    &execution, target,
                )
                .map_err(|error| {
                    execution.reject(crate::tenancy::receipts::journal_error(error))
                })?;
                if !stamp.matches(&actual) {
                    return Err(execution.reject(ErrorCode::Forbidden));
                }
                let expected = reqwest::Url::parse(url)
                    .map_err(|_| execution.reject(ErrorCode::InvalidRequest))?;
                if applied.url() != &expected {
                    return Err(execution.reject(ErrorCode::Forbidden));
                }
                Ok(BoundAuthentication {
                    digest: digest(&applied).map_err(|code| execution.reject(code))?,
                    candidate: stamp.candidate().to_vec(),
                    credential: stamp.credential().clone(),
                    stamp,
                    oauth,
                    execution,
                })
            })
            .transpose()?;
        if binding.is_none() != reservation.is_none() {
            return Err(crate::gateway::error::BitrouterError::Forbidden(
                "receipt reservation lost binding".into(),
            ));
        }
        Ok(Self {
            applied,
            binding,
            reservation,
        })
    }

    pub(super) fn tenant_binding(&self) -> Option<BoundAuthentication> {
        self.binding.clone()
    }

    pub(super) fn into_parts(
        self,
    ) -> (
        reqwest::Request,
        Option<crate::gateway::providers::auth::ContinuationAuthority>,
        Option<crate::tenancy::receipts::journal::AttemptPermit>,
    ) {
        let (request, authority) = self.applied.into_parts();
        (request, authority, self.reservation)
    }
}

pub(super) fn execution(ctx: &PipelineContext) -> Option<Arc<TenantExecution>> {
    ctx.extension()
}

pub(super) fn has_execution(ctx: &PipelineContext) -> bool {
    execution(ctx).is_some()
}

pub(super) fn reserve(
    target: &RoutingTarget,
    ctx: &PipelineContext,
) -> Result<Option<crate::tenancy::receipts::journal::AttemptPermit>> {
    execution(ctx)
        .map(|execution| {
            execution
                .reserve_attempt(target)
                .map_err(|code| execution.reject(code))
        })
        .transpose()
}

pub(super) async fn authenticate(
    request: reqwest::Request,
    target: &RoutingTarget,
    transport: &Arc<dyn Transport>,
    execution: &Arc<TenantExecution>,
    reservation: &crate::tenancy::receipts::journal::AttemptPermit,
) -> Result<(AppliedAuth, Option<crate::tenancy::oauth::ModelStamp>)> {
    if let Some(store) = execution
        .oauth(target)
        .await
        .map_err(|code| execution.reject(code))?
    {
        use crate::gateway::providers::auth::AuthApplier;
        if execution
            .route()
            .match_candidate(target)
            .map_err(|code| execution.reject(code))?
            != *reservation.stamp().credential()
        {
            return Err(execution.reject(ErrorCode::Forbidden));
        }
        let applier = store.applier();
        // Retained targets have no explicit/fallback credential; the applier must
        // resolve the exact bound store, never its compatibility shortcut.
        let mut request = applier.apply(request, &target.model_target()).await?;
        if let Some(bearer) = request
            .headers_mut()
            .get_mut(reqwest::header::AUTHORIZATION)
        {
            bearer.set_sensitive(true);
        }
        let stamp = store.stamp().map_err(|code| execution.reject(code))?;
        let reference = serde_json::to_string(reservation.stamp().credential())
            .map_err(|_| execution.reject(ErrorCode::AuthorityUnavailable))?;
        return Ok((
            AppliedAuth::proven(
                request,
                CredentialAuthority::derive_scoped(
                    "tenant-credential-revision-v1",
                    execution.installation_id().as_str(),
                    &reference,
                ),
            ),
            Some(stamp),
        ));
    }
    Ok((
        authenticate_static_exact(
            request,
            target,
            transport,
            execution,
            reservation.stamp().credential(),
        )
        .await?,
        None,
    ))
}

pub(in crate::gateway::pipeline::executor) async fn authenticate_static_exact(
    request: reqwest::Request,
    target: &RoutingTarget,
    transport: &Arc<dyn Transport>,
    execution: &Arc<TenantExecution>,
    expected: &bitrouter_ai::tenancy::configuration::ResourceRef,
) -> Result<AppliedAuth> {
    let owned = execution
        .credential(target)
        .await
        .map_err(|code| execution.reject(code))?;
    if &owned.reference != expected {
        return Err(execution.reject(ErrorCode::Forbidden));
    }
    let Credential::ApiKey { value } = owned.credential else {
        return Err(execution.reject(ErrorCode::CapabilityMismatch));
    };
    // An account header supplied by the caller cannot select another billable
    // account under a retained slot's authority. Explicit fixed headers remain.
    if target
        .headers
        .iter()
        .any(|rule| rule.passthrough() && super::is_continuation_scope_header(rule.name().as_str()))
    {
        return Err(execution.reject(ErrorCode::Forbidden));
    }
    let mut model_target = target.model_target();
    model_target.api_key = value;
    model_target.credential_priority = crate::gateway::model::target::CredentialPriority::Explicit;
    let request = transport
        .authorise(request, &model_target)
        .await
        .map_err(|error| {
            crate::gateway::error::BitrouterError::from(normalize_auth_extension_error(
                error,
                AuthOperation::RequestAuthentication,
            ))
        })?;
    execution
        .revalidate()
        .map_err(|code| execution.reject(code))?;
    let reference = serde_json::to_string(&owned.reference)
        .map_err(|_| execution.reject(ErrorCode::AuthorityUnavailable))?;
    Ok(AppliedAuth::proven(
        request,
        CredentialAuthority::derive_scoped(
            "tenant-credential-revision-v1",
            execution.installation_id().as_str(),
            &reference,
        ),
    ))
}

pub(super) fn admit_send(
    binding: Option<&BoundAuthentication>,
    permit: &mut Option<crate::tenancy::receipts::journal::AttemptPermit>,
    request: &mut reqwest::Request,
    target: &RoutingTarget,
    ctx: &PipelineContext,
) -> Result<Option<bitrouter_ai::tenancy::identity::AttemptId>> {
    match (binding, execution(ctx)) {
        (None, None) if permit.is_none() => Ok(None),
        (Some(binding), Some(execution)) if Arc::ptr_eq(&binding.execution, &execution) => {
            if digest(request).map_err(|code| execution.reject(code))? != binding.digest
                || execution
                    .route()
                    .candidate_key(target)
                    .map_err(|code| execution.reject(code))?
                    != binding.stamp.candidate()
            {
                return Err(execution.reject(ErrorCode::Forbidden));
            }
            let remaining = execution.remaining();
            let timeout = request
                .timeout()
                .copied()
                .map_or(remaining, |timeout| timeout.min(remaining));
            *request.timeout_mut() = Some(timeout);
            let permit = permit
                .as_mut()
                .ok_or_else(|| execution.reject(ErrorCode::Forbidden))?;
            let counted = if execution
                .count_required()
                .map_err(|code| execution.reject(code))?
            {
                Some(super::input_count::request_digest(request, target)?)
            } else {
                None
            };
            execution
                .arm_attempt_with_oauth(
                    target,
                    permit,
                    &binding.stamp,
                    binding.oauth.as_ref(),
                    counted.as_deref(),
                )
                .map(Some)
                .map_err(|code| execution.reject(code))
        }
        (_, Some(execution)) => Err(execution.reject(ErrorCode::Forbidden)),
        (_, None) => Err(crate::gateway::error::BitrouterError::Forbidden(
            "tenant request lost execution custody".into(),
        )),
    }
}

fn digest(request: &reqwest::Request) -> std::result::Result<[u8; 32], ErrorCode> {
    let mut digest = Sha256::new();
    fn add(digest: &mut Sha256, bytes: &[u8]) {
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    digest.update(b"bitrouter.tenant.authenticated-request.v1");
    add(&mut digest, request.method().as_str().as_bytes());
    add(&mut digest, request.url().as_str().as_bytes());
    for (name, value) in request.headers() {
        add(&mut digest, name.as_str().as_bytes());
        add(&mut digest, value.as_bytes());
    }
    let body = request
        .body()
        .and_then(reqwest::Body::as_bytes)
        .ok_or(ErrorCode::InvalidRequest)?;
    add(&mut digest, body);
    Ok(digest.finalize().into())
}
