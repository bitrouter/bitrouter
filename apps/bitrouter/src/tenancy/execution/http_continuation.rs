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
    deadline: tokio::time::Instant,
}
impl Access {
    pub(crate) fn owner(&self) -> &Owner {
        &self.owner
    }
    pub(crate) fn deadline(&self) -> tokio::time::Instant {
        self.deadline
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
        #[cfg(all(feature = "core", feature = "harness"))]
        if self
            .submission
            .session_closed
            .as_ref()
            .is_some_and(|closed| closed.load(std::sync::atomic::Ordering::Acquire))
        {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        if self.deadline() <= tokio::time::Instant::now()
            || self.submission.cancelled.is_cancelled()
        {
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
        Ok(Access {
            submission: self.submission.clone(),
            deadline: self
                .submission
                .deadline
                .min(tokio::time::Instant::now() + crate::tenancy::MANAGEMENT_TIMEOUT),
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
