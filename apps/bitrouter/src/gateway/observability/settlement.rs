//! Gateway-owned trajectory evidence and independent request evaluation.

#[cfg(feature = "eval-publication")]
use std::sync::Arc;

use async_trait::async_trait;

#[cfg(feature = "eval-publication")]
use super::decision::CapturedEvalDecision;
use super::decision::{EvalInvocation, PendingEvalDecision, PendingEvalDecisionStore};
use crate::gateway::error::{BitrouterError, Result};
use crate::gateway::pipeline::settlement::{SettlementContext, SettlementRecorder};
use crate::trajectory::settlement::{
    TrajectorySettlementDisposition, TrajectorySettlementRecorder,
};

pub(crate) struct PolicySettlementRecorder {
    pending: PendingEvalDecisionStore,
    trajectory: Option<TrajectorySettlementRecorder>,
    #[cfg(feature = "eval-publication")]
    evaluation: Option<Arc<dyn SettlementRecorder>>,
}

impl PolicySettlementRecorder {
    pub(crate) fn new(
        pending: PendingEvalDecisionStore,
        trajectory: Option<TrajectorySettlementRecorder>,
        #[cfg(feature = "eval-publication")] evaluation: Option<Arc<dyn SettlementRecorder>>,
    ) -> Self {
        Self {
            pending,
            trajectory,
            #[cfg(feature = "eval-publication")]
            evaluation,
        }
    }
}

#[async_trait]
impl SettlementRecorder for PolicySettlementRecorder {
    async fn record(&self, context: &mut SettlementContext) -> Result<()> {
        if crate::gateway::metering::replay::unadmitted(context) {
            return Ok(());
        }
        let invocation = context.get_event::<EvalInvocation>().cloned();
        let pending = invocation
            .as_ref()
            .and_then(|invocation| self.pending.peek(invocation, context.caller.user_id()));
        if let Some(trajectory) = &self.trajectory {
            let snapshot = pending
                .as_ref()
                .map(PendingEvalDecision::observation_snapshot);
            match trajectory
                .record_if_tracked(context, snapshot.as_ref())
                .await
                .map_err(|error| {
                    BitrouterError::internal(format!("persisting trajectory settlement: {error}"))
                })? {
                TrajectorySettlementDisposition::AwaitingAuthoritativeMetering => return Ok(()),
                TrajectorySettlementDisposition::Untracked
                | TrajectorySettlementDisposition::Persisted
                | TrajectorySettlementDisposition::AlreadyTerminal => {}
            }
        }
        // Episode persistence does not consume the independent request policy decision.
        #[cfg(feature = "eval-publication")]
        if let Some(consumer) = &self.evaluation {
            if let Some(invocation) = &invocation {
                // Preserve this lookup even if its cache entry expires or is evicted
                // while trajectory storage is awaited. A missing snapshot stays missing.
                context.emit(CapturedEvalDecision::new(invocation.clone(), pending));
            }
            // Consumer failure retains pending evidence for the existing retry path.
            return consumer.record(context).await;
        }
        if let Some(invocation) = &invocation {
            // No evaluation consumer was assembled. These are bounded process-local
            // observations; durable trajectory evidence was handled above.
            self.pending.remove(invocation, context.caller.user_id());
        }
        Ok(())
    }
}
