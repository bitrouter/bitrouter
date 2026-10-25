//! Checked, lease-free authority for the in-process native harness.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use bitrouter_ai::core::protocol::{CoreError, ErrorCode as CoreCode, OwnershipGrant};
use bitrouter_ai::tenancy::{
    authority::{GrantScope, Permission},
    configuration::ResourceRef,
    identity::{InstallationId, PrincipalId, RevisionId, TenantId},
    operation::ErrorCode,
    validation::Validate,
};
use serde::{Deserialize, Serialize};

use super::{Service, TenantExecution};
use crate::tenancy::error::Failure;

/// Persisted bounds are an immutable upper envelope, never current entitlement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Descriptor {
    pub installation: InstallationId,
    pub tenant: TenantId,
    pub principal: PrincipalId,
    pub name: String,
    pub revision: RevisionId,
    pub credential_scope: GrantScope,
    pub binding_scope: GrantScope,
    pub harness: String,
    pub session: String,
}

/// Its only constructor authenticates with the same Service that owns the DB.
pub(crate) struct Authority {
    host: Arc<super::host::Authority>,
    proposed: Descriptor,
    required: Vec<ResourceRef>,
}

/// Completed from authenticated lookup and a validated immutable row descriptor.
pub(crate) struct Binding {
    authority: Arc<Authority>,
    descriptor: Descriptor,
    grant: OwnershipGrant,
    closing: Arc<AtomicBool>,
}

impl Service {
    pub(crate) async fn session_authority(
        &self,
        headers: &http::HeaderMap,
        name: &str,
    ) -> Result<Arc<Authority>, Failure> {
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return Err(ErrorCode::InvalidRequest.into());
        }
        let host = self.host_authority(headers)?;
        // Authenticate and resolve retained R1 before any harness storage or path I/O.
        let retained = host.retained().await?;
        // The opener has no model input yet. This fixed-R1 profile therefore
        // approves its complete native route closure before any journal read.
        // No retained cache root or execution capacity lives with the session.
        let mut required = Vec::new();
        for reference in &retained.document().routes {
            let route = retained
                .select_native(
                    reference.resource_id.as_str(),
                    bitrouter_ai::types::ModelOperation::Generation,
                )
                .map_err(Failure::from)?;
            for dependency in route.dependencies() {
                if !required.contains(dependency) {
                    if required.len() == 4096 {
                        return Err(ErrorCode::CapacityExceeded.into());
                    }
                    required.push(dependency.clone());
                }
            }
        }
        host.with_authority(&required, |effective| {
            if !effective.permits(Permission::CredentialUse) {
                return Err(ErrorCode::Forbidden);
            }
            Ok(())
        })
        .map_err(Failure::from)?;
        drop(retained);
        let (installation, tenant, principal) = host.identity();
        let (credential_scope, binding_scope) = host.scopes();
        let proposed = Descriptor {
            installation: installation.clone(),
            tenant: tenant.clone(),
            principal: principal.clone(),
            name: name.to_owned(),
            revision: host.revision().clone(),
            credential_scope: credential_scope.clone(),
            binding_scope: binding_scope.clone(),
            harness: format!("harness_{}", uuid::Uuid::new_v4()),
            session: format!("session_{}", uuid::Uuid::new_v4()),
        };
        Ok(Arc::new(Authority {
            host,
            proposed,
            required,
        }))
    }
}

impl Authority {
    pub(crate) fn caller(&self) -> crate::gateway::caller::CallerContext {
        self.host.caller()
    }
    pub(crate) fn service(&self) -> &Service {
        self.host.service()
    }
    pub(crate) fn host(&self) -> &super::host::Authority {
        &self.host
    }
    pub(crate) fn descriptor(&self) -> &Descriptor {
        &self.proposed
    }
    pub(crate) fn storage_identity(&self) -> (&InstallationId, &TenantId, &PrincipalId) {
        self.host.identity()
    }
    pub(crate) fn check(&self) -> Result<(), Failure> {
        self.with_authority(|| ()).map_err(Failure::from)
    }
    pub(crate) fn with_authority<T>(&self, work: impl FnOnce() -> T) -> Result<T, ErrorCode> {
        self.host.with_authority(&self.required, |effective| {
            if !effective.permits(Permission::CredentialUse) {
                return Err(ErrorCode::Forbidden);
            }
            Ok(work())
        })
    }
    pub(in crate::tenancy) fn origin(
        &self,
        proof: &Arc<crate::tenancy::transaction::Proof>,
    ) -> bool {
        self.host.origin(proof)
    }
    pub(crate) fn matches(&self, descriptor: &Descriptor) -> bool {
        descriptor.installation == self.proposed.installation
            && descriptor.tenant == self.proposed.tenant
            && descriptor.principal == self.proposed.principal
            && descriptor.name == self.proposed.name
            && descriptor.revision == self.proposed.revision
    }
    pub(crate) fn validate_descriptor(&self, descriptor: &Descriptor) -> Result<(), Failure> {
        descriptor
            .credential_scope
            .validate()
            .map_err(|_| Failure::from(ErrorCode::Forbidden))?;
        descriptor
            .binding_scope
            .validate()
            .map_err(|_| Failure::from(ErrorCode::Forbidden))?;
        if !self.matches(descriptor)
            || self
                .required
                .iter()
                .any(|reference| !approved(descriptor, reference))
        {
            return Err(ErrorCode::Forbidden.into());
        }
        Ok(())
    }
    pub(crate) fn bind(
        self: &Arc<Self>,
        descriptor: Descriptor,
        grant: OwnershipGrant,
    ) -> Result<Arc<Binding>, CoreError> {
        self.check().map_err(core_error)?;
        self.validate_descriptor(&descriptor).map_err(core_error)?;
        grant.validate()?;
        if !self.matches(&descriptor)
            || grant.session_id != descriptor.session
            || grant.harness_id != descriptor.harness
        {
            return Err(CoreError::rejected(
                CoreCode::UnauthorizedScope,
                "native session binding mismatch",
            ));
        }
        Ok(Arc::new(Binding {
            authority: self.clone(),
            descriptor,
            grant,
            closing: Arc::new(AtomicBool::new(false)),
        }))
    }
}

impl Binding {
    pub(crate) fn authority(&self) -> &Arc<Authority> {
        &self.authority
    }
    pub(crate) fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }
    pub(crate) fn check(&self) -> Result<(), CoreError> {
        self.with_dispatch(|| ())
    }
    pub(crate) fn with_dispatch<T>(&self, work: impl FnOnce() -> T) -> Result<T, CoreError> {
        self.authority
            .with_authority(|| {
                if self.closing.load(Ordering::Acquire) {
                    Err(CoreError::rejected(
                        CoreCode::CheckpointUnavailable,
                        "native owner is closing",
                    ))
                } else {
                    Ok(work())
                }
            })
            .map_err(|code| core_error(code.into()))?
    }
    pub(crate) fn close_new_work(&self) -> Result<(), CoreError> {
        let _guard = self
            .authority
            .service()
            .snapshot
            .write()
            .map_err(|_| core_error(ErrorCode::AuthorityUnavailable.into()))?;
        self.closing.store(true, Ordering::Release);
        Ok(())
    }
    pub(crate) fn grant(&self, grant: &OwnershipGrant) -> Result<(), CoreError> {
        if &self.grant != grant {
            return Err(CoreError::rejected(
                CoreCode::UnauthorizedScope,
                "native grant changed",
            ));
        }
        Ok(())
    }
    pub(crate) fn belongs_to(&self, service: &Service) -> bool {
        self.authority.origin(&service.cleanup)
    }
    pub(crate) async fn admit(&self, model: &str) -> Result<Arc<TenantExecution>, Failure> {
        let descriptor = &self.descriptor;
        let mut submission = self.authority.host.submission();
        submission.session_closed = Some(self.closing.clone());
        self.authority
            .service()
            .admit_selected_execution(submission, |revision| {
                let route = revision
                    .select_native(model, bitrouter_ai::types::ModelOperation::Generation)?;
                for reference in route.dependencies() {
                    if !approved(descriptor, reference) {
                        return Err(ErrorCode::Forbidden);
                    }
                }
                Ok(route)
            })
            .await
    }
}

fn approved(descriptor: &Descriptor, reference: &ResourceRef) -> bool {
    let key = reference.key();
    crate::tenancy::authority::covers_key(&descriptor.credential_scope, &descriptor.tenant, &key)
        && crate::tenancy::authority::covers_key(
            &descriptor.binding_scope,
            &descriptor.tenant,
            &key,
        )
}

pub(crate) fn core_error(error: Failure) -> CoreError {
    let code = match error.0.code {
        ErrorCode::CapacityExceeded => CoreCode::LimitExceeded,
        ErrorCode::AuthorityUnavailable | ErrorCode::StorageUnavailable => {
            CoreCode::CheckpointUnavailable
        }
        _ => CoreCode::UnauthorizedScope,
    };
    CoreError::rejected(code, "authenticated native session is unavailable")
}
