//! Paid Choice is owned by the actual Generation step and native journal.
use super::*;
use crate::storage::checkpoint::snapshot::context::view::SourceRevision;
use crate::{
    harness::resources::managed::store::choice::{JournalView, Record},
    tenancy::resources::execution::ChoiceProfile,
};
use bitrouter_ai::{decisions::DecisionRequest as NativeRequest, tenancy::identity::ExecutionId};
use std::sync::atomic::{AtomicBool, Ordering};

mod driver;
mod send;
pub(crate) mod wire;

/// Created only after the actual owned child is admitted and its Core intent ACKed.
pub(crate) struct SendGate {
    control: Arc<StepControl>,
    source: SourceRevision,
    profile: ChoiceProfile,
    record: Record,
    native: Arc<NativeRequest>,
    journal: JournalView,
    cancellation: CancellationToken,
    deadline: tokio::time::Instant,
    claimed: AtomicBool,
    armed: AtomicBool,
}

impl SendGate {
    pub(crate) fn matches(
        &self,
        child: &ExecutionId,
        parent: &ExecutionId,
        profile: &ChoiceProfile,
    ) -> bool {
        child == &self.record.child_execution
            && parent == &self.record.parent_execution
            && profile == &self.profile
    }
    pub(crate) fn matches_request(&self, request: &NativeRequest) -> bool {
        self.native.as_ref() == request
    }
    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

impl CoreSession {
    pub(super) async fn validate_owned_choice_reuse(
        &self,
        receipt: &crate::storage::checkpoint::snapshot::context::DecisionReceipt,
        profile: &ChoiceProfile,
    ) -> Result<(), CoreError> {
        let _commit = self.shared.commits.lock().await;
        let head = self.shared.live.lock().await.gate.head().clone();
        let view = self.shared.owned_journal.as_ref().ok_or_else(|| {
            reject(
                ErrorCode::CheckpointUnavailable,
                "owned Choice journal missing",
            )
        })?;
        let record = view
            .choice(&receipt.decision_id, &head)
            .await?
            .ok_or_else(|| {
                reject(
                    ErrorCode::CheckpointConflict,
                    "owned Choice provenance missing",
                )
            })?;
        if record.profile(receipt) != *profile
            || record.intent_digest
                != crate::harness::resources::managed::store::choice::intent(receipt)?
            || record.outcome_digest.is_none()
            || receipt.outcome.is_none()
        {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "owned Choice reuse changed its binding",
            ));
        }
        Ok(())
    }
}

/// Preserve the planner's stable tail reduction, charging the native wire too.
/// Removing a candidate leaves its source unjudged and therefore retained in full.
pub(super) fn request(
    candidates: &mut crate::storage::checkpoint::snapshot::context::view::CandidateSet,
    store: &crate::storage::checkpoint::snapshot::context::ContextStore,
    profile: &ChoiceProfile,
    input: &crate::gateway::routing::input::Input,
) -> Result<
    Option<(
        crate::gateway::model::decision::types::DecisionRequest,
        wire::Prepared,
    )>,
    CoreError,
> {
    loop {
        let Some(request) = candidates.request(
            store,
            profile.child.resource_id.as_str(),
            &profile.policy,
            input,
        )?
        else {
            return Ok(None);
        };
        if let Ok(prepared) = wire::prepare(
            &request,
            profile.child.resource_id.as_str(),
            profile.policy.max_request_bytes,
        ) {
            return Ok(Some((request, prepared)));
        }
        // At most the already bounded candidate count plus the final routing-only
        // pass. No admission, intent or provider work exists until this fits.
        if candidates.candidates.pop_last().is_none() {
            return Ok(None);
        }
    }
}
