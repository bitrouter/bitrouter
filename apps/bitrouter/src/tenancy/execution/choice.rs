//! A paid canonical Decisions child with its own admission and receipt owner.
use super::{RetainedRoute, TenantExecution};
use crate::{
    gateway::{
        error::{BitrouterError, Result},
        pipeline::{
            native::NativeResponseByteLimit,
            types::{PipelineRequest, PipelineResponse},
        },
    },
    tenancy::{error::Failure, resources::execution::ChoiceProfile},
};
use bitrouter_ai::{
    decisions::DecisionRequest,
    tenancy::{identity::ExecutionId, operation::ErrorCode},
};
use std::sync::Arc;

/// One-use claim. Dropping it before pipeline attachment abandons the existing
/// AdmissionTicket; the existing Registry persists the real NotStarted receipt.
pub(crate) struct AdmittedChoice {
    execution: Arc<TenantExecution>,
    parent: ExecutionId,
    profile: ChoiceProfile,
    seeds: Option<(
        crate::tenancy::receipts::recovery::Seed,
        crate::tenancy::receipts::recovery::Seed,
    )>,
}
impl AdmittedChoice {
    pub(super) fn admit(
        parent: &Arc<TenantExecution>,
        route: Arc<RetainedRoute>,
    ) -> std::result::Result<Self, Failure> {
        parent.revalidate().map_err(Failure::from)?;
        route.require_capabilities().map_err(Failure::from)?;
        let profile = parent
            .route
            .choice_profile()
            .ok_or(ErrorCode::CapabilityMismatch)?;
        if route.reference() != &profile.child {
            return Err(ErrorCode::Forbidden.into());
        }
        let config = parent.route.configuration().map_err(Failure::from)?;
        let free = crate::tenancy::receipts::capacity::free_space(
            parent.service.receipt_volume.as_deref(),
        )
        .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
        parent.service.admission.receipt_probe(free);
        let lease = parent
            .service
            .admission
            .try_acquire(
                &parent.submission.tenant,
                &config.limits,
                parent.submission.deadline.min(
                    tokio::time::Instant::now()
                        + std::time::Duration::from_secs(
                            parent.service.execution_settings.submission_timeout_secs,
                        ),
                ),
            )
            .map_err(Failure::from)?;
        let mut submission = parent.submission.clone();
        submission.cancelled = parent.submission.cancelled.child_token();
        let execution = parent.service.finish_admission(
            submission,
            route,
            lease,
            config.learning.clone(),
            true,
        )?;
        crate::tenancy::receipts::journal::link_owned_child(parent, &execution)
            .map_err(Failure::from)?;
        let owner = execution
            .receipt_owner()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        let weak = Arc::downgrade(owner);
        parent
            .receipt_owner()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?
            .on_cancel(Box::new(move || {
                if let Some(owner) = weak.upgrade() {
                    owner.cancel();
                }
            }))
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        let seeds = match (
            parent
                .receipt_owner()
                .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?
                .recovery_seed(&parent.service),
            owner.recovery_seed(&parent.service),
        ) {
            (Ok(Some(parent)), Ok(Some(child))) => Some((parent, child)),
            (Ok(None), Ok(None)) => None,
            _ => return Err(ErrorCode::AuthorityUnavailable.into()),
        };
        Ok(Self {
            seeds,
            execution,
            parent: parent.id.clone(),
            profile,
        })
    }
    pub(crate) fn recovery_seeds(
        &self,
    ) -> Option<(
        crate::tenancy::receipts::recovery::Seed,
        crate::tenancy::receipts::recovery::Seed,
    )> {
        self.seeds.clone()
    }
    pub(crate) fn receipt_owner(&self) -> Result<Arc<crate::tenancy::receipts::owner::Owner>> {
        self.execution.receipt_owner().cloned()
    }
    pub(crate) fn execution_id(&self) -> &ExecutionId {
        &self.execution.id
    }
    pub(crate) fn parent_id(&self) -> &ExecutionId {
        &self.parent
    }
    pub(crate) fn profile(&self) -> &ChoiceProfile {
        &self.profile
    }
    pub(crate) fn deadline(&self) -> tokio::time::Instant {
        self.execution.submission.deadline.min(
            tokio::time::Instant::now()
                + std::time::Duration::from_secs(
                    self.execution
                        .service
                        .execution_settings
                        .submission_timeout_secs,
                ),
        )
    }

    pub(crate) async fn execute(
        self,
        request: DecisionRequest,
        gate: Arc<crate::core::session::choice::SendGate>,
    ) -> Result<PipelineResponse> {
        if request.model != self.profile.child.resource_id.as_str()
            || !gate.matches(self.execution_id(), &self.parent, &self.profile)
            || !gate.matches_request(&request)
        {
            return Err(self.execution.reject(ErrorCode::Forbidden));
        }
        let cancellation = gate.cancellation();
        self.execution
            .receipt_owner()?
            .on_cancel(Box::new(move || cancellation.cancel()))?;
        self.execution
            .choice
            .as_ref()
            .ok_or(BitrouterError::UpstreamUnavailable)?
            .set(gate)
            .map_err(|_| self.execution.reject(ErrorCode::Forbidden))?;
        let pipeline = self.execution.pipeline()?;
        let mut request =
            PipelineRequest::new_decisions(request.model.clone(), self.execution.caller(), request);
        request.request_id = self.execution.id.to_string();
        request.inbound_protocol = None;
        request.extensions.insert(Arc::new(NativeResponseByteLimit(
            self.profile
                .policy
                .max_request_bytes
                .checked_mul(2)
                .and_then(|bytes| bytes.checked_add(8192))
                .ok_or(BitrouterError::UpstreamUnavailable)? as u64,
        )));
        self.execution.attach(pipeline.clone(), &mut request)?;
        // No NativeExecutionControl: Decisions is its own typed operation and
        // cannot recursively prepare the Generation parent's context.
        pipeline.execute_without_server_tools(request).await
    }
}

impl TenantExecution {
    pub(crate) fn choice_gate(
        &self,
    ) -> Result<Option<Arc<crate::core::session::choice::SendGate>>> {
        self.choice
            .as_ref()
            .map(|slot| {
                slot.get()
                    .cloned()
                    .ok_or_else(|| self.reject(ErrorCode::Forbidden))
            })
            .transpose()
    }
}
