//! Current tenant-wide read authority; no legacy caller inference or cached grant.
use crate::tenancy::{
    Service,
    actor::Job,
    error::Failure,
    model::{Caller, Snapshot},
};
use bitrouter_ai::tenancy::{
    authority::{GrantScope, Permission},
    identity::{CorrelationId, ExecutionId, InstallationId, TenantId},
    operation::ErrorCode,
};
use std::sync::Arc;

pub(in crate::tenancy) enum Request {
    One(ExecutionId),
    UsageEvidence(ExecutionId),
    Page {
        correlation: CorrelationId,
        after: Option<ExecutionId>,
        limit: u64,
    },
}
pub(in crate::tenancy) struct Read {
    pub caller: Caller,
    pub installation: InstallationId,
    pub tenant: TenantId,
    pub request: Request,
    pub deadline: tokio::time::Instant,
    pub _permit: tokio::sync::OwnedSemaphorePermit,
    pub reply: tokio::sync::oneshot::Sender<Result<Vec<u8>, Failure>>,
}
pub(in crate::tenancy) fn authorize(
    state: &Snapshot,
    caller: &Caller,
    tenant: &TenantId,
) -> Result<(), ErrorCode> {
    if matches!(caller, Caller::Installation(_)) {
        return Ok(());
    }
    let now = chrono::Utc::now();
    let credential = state.credential(caller, now)?;
    if &credential.tenant != tenant {
        return Err(ErrorCode::Forbidden);
    }
    if credential.audit_reader {
        // Audit authority is separately provisioned by the installation operator.
        // No lifecycle bypass is granted to an ordinary ReceiptRead permission.
        if credential.binding.is_none()
            && credential.permissions.is_empty()
            && matches!(&credential.scope,GrantScope::Tenant{operator_pools} if operator_pools.is_empty())
        {
            return Ok(());
        }
        return Err(ErrorCode::Forbidden);
    }
    state.authorize(
        caller,
        tenant,
        Permission::ReceiptRead,
        &GrantScope::Tenant {
            operator_pools: Vec::new(),
        },
        now,
    )
}
impl Service {
    pub(in crate::tenancy) async fn read_receipt(
        &self,
        caller: Caller,
        tenant: TenantId,
        request: Request,
    ) -> Result<Vec<u8>, Failure> {
        let state = self.current()?;
        authorize(&state, &caller, &tenant).map_err(Failure::from)?;
        let installation = state.installation.clone();
        drop(state);
        let permit = Arc::clone(&self.reads)
            .try_acquire_owned()
            .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
        let deadline = tokio::time::Instant::now() + crate::tenancy::MANAGEMENT_TIMEOUT;
        let (reply, wait) = tokio::sync::oneshot::channel();
        self.commands
            .try_send(Job::ReceiptRead(Read {
                caller: caller.clone(),
                installation,
                tenant: tenant.clone(),
                request,
                deadline,
                _permit: permit,
                reply,
            }))
            .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
        let body = tokio::time::timeout_at(deadline, wait)
            .await
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))??;
        let state = self.current()?;
        authorize(&state, &caller, &tenant).map_err(Failure::from)?;
        Ok(body)
    }
}

/// The receipt decode/serialization body shared by the live actor and archive owner.
pub(in crate::tenancy) async fn body(
    db: &impl sea_orm::ConnectionTrait,
    installation: &InstallationId,
    tenant: &TenantId,
    request: &Request,
    storage_failure: impl Fn() -> Failure,
) -> Result<Vec<u8>, Failure> {
    let encoded = match request {
        Request::UsageEvidence(id) => {
            let candidate = super::store::lookup_candidate(db, installation, tenant, id)
                .await
                .map_err(|_| storage_failure())?
                .ok_or_else(|| Failure::from(ErrorCode::NotFound))?;
            let evidence = match candidate.usage_evidence {
                Some(evidence) => evidence.document,
                None => super::usage_evidence::historical(&candidate.receipt),
            };
            serde_json::to_vec(&evidence)
        }
        Request::One(id) => {
            let receipt = super::store::lookup(db, installation, tenant, id)
                .await
                .map_err(|_| storage_failure())?
                .ok_or_else(|| Failure::from(ErrorCode::NotFound))?;
            serde_json::to_vec(&receipt)
        }
        Request::Page {
            correlation,
            after,
            limit,
        } => {
            let page = super::store::page(
                db,
                installation,
                tenant,
                correlation,
                after.as_ref(),
                *limit,
            )
            .await
            .map_err(|_| storage_failure())?;
            serde_json::to_vec(&page)
        }
    };
    encoded.map_err(|_| Failure::from(ErrorCode::StorageUnavailable))
}
