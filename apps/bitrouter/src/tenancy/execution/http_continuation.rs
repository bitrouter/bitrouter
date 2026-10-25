//! Admitted-value lookup proof, without retaining Service in the actor job.
use super::{Permission, Snapshot, Submission, TenantExecution, authorize_route};
use crate::tenancy::{continuation::http::codec::Owner, transaction::Proof};
use bitrouter_ai::tenancy::operation::ErrorCode;
use std::sync::{Arc, Weak};

pub(crate) struct Access {
    submission: Submission,
    route: Arc<super::RetainedRoute>,
    owner: Owner,
    origin: Weak<Proof>,
}
impl Access {
    pub(crate) fn owner(&self) -> &Owner {
        &self.owner
    }
    pub(crate) fn deadline(&self) -> tokio::time::Instant {
        self.submission.deadline
    }
    pub(in crate::tenancy) fn validate(
        &self,
        state: &Snapshot,
        origin: &Arc<Proof>,
    ) -> Result<(), ErrorCode> {
        if !Weak::ptr_eq(&self.origin, &Arc::downgrade(origin))
            || state.installation != self.owner.installation
            || state.saturated
        {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        if self.deadline() <= tokio::time::Instant::now() {
            return Err(ErrorCode::CapacityExceeded);
        }
        let effective = state.effective(
            &self.submission.caller,
            &self.submission.tenant,
            chrono::Utc::now(),
        )?;
        if effective.credential.principal != self.submission.principal
            || effective.credential.id != self.submission.credential
            || !effective.permits(Permission::ExecutionSubmit)
        {
            return Err(ErrorCode::Forbidden);
        }
        authorize_route(state, &self.submission, self.route.dependencies())
    }
}
impl TenantExecution {
    pub(crate) fn http_owner(&self) -> Owner {
        Owner {
            installation: self.installation_id().clone(),
            tenant: self.submission.tenant.clone(),
            principal: self.submission.principal.clone(),
            route: self.route.reference().clone(),
        }
    }
    pub(crate) fn http_access(&self) -> Result<Access, ErrorCode> {
        self.revalidate()?;
        let Submission {
            caller,
            tenant,
            principal,
            credential,
            binding,
            deadline,
        } = &self.submission;
        Ok(Access {
            submission: Submission {
                caller: caller.clone(),
                tenant: tenant.clone(),
                principal: principal.clone(),
                credential: credential.clone(),
                binding: binding.clone(),
                deadline: *deadline,
            },
            route: self.route.clone(),
            owner: self.http_owner(),
            origin: Arc::downgrade(&self.service.cleanup),
        })
    }
    pub(crate) fn http_port(&self) -> &Arc<crate::tenancy::continuation::http::Port> {
        &self.service.http_continuation
    }
    pub(crate) fn http_revision(&self) -> &bitrouter_ai::tenancy::identity::RevisionId {
        &self.submission.binding.execution_revision
    }
}
