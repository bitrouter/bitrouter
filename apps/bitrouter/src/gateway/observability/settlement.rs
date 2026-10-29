//! Gateway-owned terminal evidence, with an optional untracked-request consumer.

#[cfg(feature = "core")]
use std::sync::Arc;

use async_trait::async_trait;

#[cfg(feature = "core")]
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
    #[cfg(feature = "core")]
    untracked: Option<Arc<dyn SettlementRecorder>>,
}

impl PolicySettlementRecorder {
    pub(crate) fn new(
        pending: PendingEvalDecisionStore,
        trajectory: Option<TrajectorySettlementRecorder>,
        #[cfg(feature = "core")] untracked: Option<Arc<dyn SettlementRecorder>>,
    ) -> Self {
        Self {
            pending,
            trajectory,
            #[cfg(feature = "core")]
            untracked,
        }
    }
}

#[async_trait]
impl SettlementRecorder for PolicySettlementRecorder {
    async fn record(&self, context: &mut SettlementContext) -> Result<()> {
        if context.has_event::<crate::gateway::metering::replay::CanonicalRequestReplayRejected>() {
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
                TrajectorySettlementDisposition::Untracked => {}
                TrajectorySettlementDisposition::AwaitingAuthoritativeMetering => return Ok(()),
                TrajectorySettlementDisposition::Persisted
                | TrajectorySettlementDisposition::AlreadyTerminal => {
                    if let Some(invocation) = &invocation {
                        self.pending.remove(invocation, context.caller.user_id());
                    }
                    return Ok(());
                }
            }
        }
        #[cfg(feature = "core")]
        if let Some(consumer) = &self.untracked {
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
