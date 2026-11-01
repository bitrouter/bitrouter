//! Authenticated host identity without a native session, journal or execution lease.
use std::sync::Arc;

use bitrouter_ai::tenancy::{
    authority::GrantScope,
    configuration::ResourceRef,
    identity::{InstallationId, PrincipalId, RevisionId, TenantId},
    operation::ErrorCode,
};
use chrono::Utc;

use super::{Service, Submission};
use crate::tenancy::{
    authority::{Effective, covers_key},
    cache::Pin,
    error::Failure,
    resources::execution::RetainedExecutionRevision,
    transaction::Proof,
};

/// Constructed only by real Service authentication. These original scopes are
/// ceilings; each consumer supplies its actual retained closure and permissions.
pub(crate) struct Authority {
    service: Service,
    identity: Submission,
    credential_scope: GrantScope,
    binding_scope: GrantScope,
}

impl Service {
    pub(in crate::tenancy) fn host_authority(
        &self,
        headers: &http::HeaderMap,
    ) -> Result<Arc<Authority>, Failure> {
        super::native::bounded_headers(headers)?;
        if headers.contains_key(super::BLOCK_LINK_HEADER) {
            return Err(ErrorCode::InvalidRequest.into());
        }
        let identity = self.submission(headers)?;
        let (credential_scope, binding_scope) = self
            .with_dispatch_authority(&identity, |state| {
                let effective = state.effective(&identity.caller, &identity.tenant, Utc::now())?;
                Ok((
                    effective.credential.scope.clone(),
                    effective.binding.scope.clone(),
                ))
            })
            .map_err(Failure::from)?;
        Ok(Arc::new(Authority {
            service: self.clone(),
            identity,
            credential_scope,
            binding_scope,
        }))
    }
}

impl Authority {
    pub(in crate::tenancy) async fn admit_assessment(
        &self,
        binding: crate::tenancy::learning::source::Binding,
        reference: &ResourceRef,
    ) -> Result<Arc<super::TenantExecution>, Failure> {
        let execution = self
            .service
            .admit_selected_execution(self.submission(), true, |revision| {
                let route = revision.select_native(
                    reference.resource_id.as_str(),
                    bitrouter_ai::types::ModelOperation::Generation,
                )?;
                if route.reference() != reference {
                    return Err(ErrorCode::Forbidden);
                }
                Ok(route)
            })
            .await?;
        self.with_authority(execution.route.dependencies(), |effective| {
            if !effective.permits(bitrouter_ai::tenancy::authority::Permission::EvaluationSubmit) {
                return Err(ErrorCode::Forbidden);
            }
            Ok(())
        })
        .map_err(Failure::from)?;
        execution
            .assessment
            .set(binding)
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        execution.revalidate().map_err(Failure::from)?;
        Ok(execution)
    }

    pub(in crate::tenancy) fn credential_id(&self) -> &str {
        self.identity.credential.as_str()
    }
    pub(in crate::tenancy) fn matches_submission(&self, request: &Submission) -> bool {
        self.identity.tenant == request.tenant
            && self.identity.principal == request.principal
            && self.identity.credential == request.credential
            && self.identity.binding.installation_id == request.binding.installation_id
            && self.identity.binding.instance_id == request.binding.instance_id
            && self.identity.binding.execution_revision == request.binding.execution_revision
    }

    pub(crate) fn service(&self) -> &Service {
        &self.service
    }

    pub(crate) fn identity(&self) -> (&InstallationId, &TenantId, &PrincipalId) {
        (
            &self.identity.binding.installation_id,
            &self.identity.tenant,
            &self.identity.principal,
        )
    }

    pub(in crate::tenancy) fn revision(&self) -> &RevisionId {
        &self.identity.binding.execution_revision
    }

    pub(in crate::tenancy) fn scopes(&self) -> (&GrantScope, &GrantScope) {
        (&self.credential_scope, &self.binding_scope)
    }

    pub(crate) fn caller(&self) -> crate::gateway::caller::CallerContext {
        crate::gateway::caller::CallerContext::new(
            self.identity.credential.as_str(),
            self.identity.principal.as_str(),
        )
    }

    pub(super) fn submission(&self) -> Submission {
        // A host's lifetime must not extend or reuse one operation deadline.
        Submission {
            caller: self.identity.caller.clone(),
            tenant: self.identity.tenant.clone(),
            principal: self.identity.principal.clone(),
            credential: self.identity.credential.clone(),
            binding: self.identity.binding.clone(),
            deadline: self.service.submission_deadline(),
            cancelled: tokio_util::sync::CancellationToken::new(),
            session_closed: None,
            block: None,
        }
    }

    pub(in crate::tenancy) fn origin(&self, proof: &Arc<Proof>) -> bool {
        Arc::ptr_eq(proof, &self.service.cleanup)
    }

    /// Baseline identity eligibility only. Acquiring host capacity authorizes no
    /// storage/process effect; the consumer must use with_authority before those.
    pub(in crate::tenancy) fn check(&self) -> Result<(), Failure> {
        self.service
            .with_dispatch_authority(&self.submission(), |_| Ok(()))
            .map_err(Failure::from)
    }

    pub(in crate::tenancy) async fn retained(
        &self,
    ) -> Result<Pin<RetainedExecutionRevision>, Failure> {
        self.service.retained_execution(&self.submission()).await
    }

    /// The current snapshot read lock covers both the complete resource check
    /// and the consumer's synchronous permission check / dispatch admission.
    pub(in crate::tenancy) fn with_authority<T>(
        &self,
        dependencies: &[ResourceRef],
        work: impl FnOnce(&Effective<'_>) -> Result<T, ErrorCode>,
    ) -> Result<T, ErrorCode> {
        let submission = self.submission();
        self.service.with_dispatch_authority(&submission, |state| {
            self.in_snapshot(state, dependencies, work)
        })
    }

    pub(in crate::tenancy) fn in_snapshot<T>(
        &self,
        state: &crate::tenancy::model::Snapshot,
        dependencies: &[ResourceRef],
        work: impl FnOnce(&Effective<'_>) -> Result<T, ErrorCode>,
    ) -> Result<T, ErrorCode> {
        let submission = self.submission();
        for reference in dependencies {
            if !covers_key(&self.credential_scope, &submission.tenant, &reference.key())
                || !covers_key(&self.binding_scope, &submission.tenant, &reference.key())
            {
                return Err(ErrorCode::Forbidden);
            }
        }
        super::authorize_route(state, &submission, dependencies)?;
        let effective = state.effective(&submission.caller, &submission.tenant, Utc::now())?;
        work(&effective)
    }
}
