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

pub(crate) mod native;

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub(super) struct BoundAuthentication {
    execution: Arc<TenantExecution>,
    digest: [u8; 32],
    candidate: Vec<u8>,
    credential: bitrouter_ai::tenancy::configuration::ResourceRef,
}

/// The private proof travels beside AppliedAuth so no public transport contract
/// grows an application service handle or accepts a caller-created capability.
pub(super) struct AuthenticatedRequest {
    applied: AppliedAuth,
    binding: Option<BoundAuthentication>,
}

impl std::ops::Deref for AuthenticatedRequest {
    type Target = AppliedAuth;
    fn deref(&self) -> &Self::Target {
        &self.applied
    }
}

impl AuthenticatedRequest {
    pub(super) fn new(
        applied: AppliedAuth,
        target: &RoutingTarget,
        ctx: &PipelineContext,
        url: &str,
    ) -> Result<Self> {
        let binding = execution(ctx)
            .map(|execution| {
                execution
                    .revalidate()
                    .map_err(|code| execution.reject(code))?;
                let candidate = execution
                    .route()
                    .candidate_key(target)
                    .map_err(|code| execution.reject(code))?;
                let expected = reqwest::Url::parse(url)
                    .map_err(|_| execution.reject(ErrorCode::InvalidRequest))?;
                if applied.url() != &expected {
                    return Err(execution.reject(ErrorCode::Forbidden));
                }
                Ok(BoundAuthentication {
                    digest: digest(&applied).map_err(|code| execution.reject(code))?,
                    candidate,
                    credential: execution
                        .route()
                        .match_candidate(target)
                        .map_err(|code| execution.reject(code))?,
                    execution,
                })
            })
            .transpose()?;
        Ok(Self { applied, binding })
    }

    pub(super) fn tenant_binding(&self) -> Option<BoundAuthentication> {
        self.binding.clone()
    }

    pub(super) fn into_parts(
        self,
    ) -> (
        reqwest::Request,
        Option<crate::gateway::providers::auth::ContinuationAuthority>,
    ) {
        self.applied.into_parts()
    }
}

pub(super) fn execution(ctx: &PipelineContext) -> Option<Arc<TenantExecution>> {
    ctx.extension()
}

pub(super) fn has_execution(ctx: &PipelineContext) -> bool {
    execution(ctx).is_some()
}

pub(super) async fn authenticate(
    request: reqwest::Request,
    target: &RoutingTarget,
    transport: &Arc<dyn Transport>,
    execution: &Arc<TenantExecution>,
) -> Result<AppliedAuth> {
    let owned = execution
        .credential(target)
        .await
        .map_err(|code| execution.reject(code))?;
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
    request: &mut reqwest::Request,
    target: &RoutingTarget,
    ctx: &PipelineContext,
) -> Result<Option<bitrouter_ai::tenancy::identity::AttemptId>> {
    match (binding, execution(ctx)) {
        (None, None) => Ok(None),
        (Some(binding), Some(execution)) if Arc::ptr_eq(&binding.execution, &execution) => {
            if digest(request).map_err(|code| execution.reject(code))? != binding.digest
                || execution
                    .route()
                    .candidate_key(target)
                    .map_err(|code| execution.reject(code))?
                    != binding.candidate
            {
                return Err(execution.reject(ErrorCode::Forbidden));
            }
            let permit = execution
                .admit_send(target)
                .map_err(|code| execution.reject(code))?;
            let remaining = execution.remaining();
            let timeout = request
                .timeout()
                .copied()
                .map_or(remaining, |timeout| timeout.min(remaining));
            *request.timeout_mut() = Some(timeout);
            Ok(Some(permit))
        }
        (_, Some(execution)) => Err(execution.reject(ErrorCode::Forbidden)),
        (Some(_), None) => Err(crate::gateway::error::BitrouterError::Forbidden(
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
