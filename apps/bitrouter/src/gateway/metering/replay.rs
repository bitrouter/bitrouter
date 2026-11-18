//! Request identity rejection markers consumed by always-run accounting.

use bitrouter_ai::event::PipelineEvent;
use serde::Serialize;

/// The ordinary HTTP ingress received an explicit, caller-chosen request ID.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ClientRequestIdClaim;

impl PipelineEvent for ClientRequestIdClaim {
    fn event_name(&self) -> &'static str {
        "gateway.client_request_id_claim"
    }
}

/// An early authentication or preparation failure cannot settle over the
/// globally reserved request, even if it races the actual owner's response.
pub(crate) fn unadmitted(ctx: &crate::gateway::pipeline::settlement::SettlementContext) -> bool {
    ctx.has_event::<CanonicalRequestReplayRejected>()
        || (ctx.has_event::<ClientRequestIdClaim>()
            && !ctx.has_event::<crate::gateway::observability::requests::GatewayRequestObserved>())
}

/// A durable execution already owns this request identity. Always-run
/// bookkeeping must not overwrite that execution with the rejected replay.
#[derive(Debug, Serialize)]
pub(crate) struct CanonicalRequestReplayRejected;

impl PipelineEvent for CanonicalRequestReplayRejected {
    fn event_name(&self) -> &'static str {
        "evolution.request_replay_rejected"
    }
}

/// Shared with metering: rejecting a duplicate must preserve the first call's
/// charge, even though settlement otherwise runs for rejected requests too.
#[derive(Debug, Serialize)]
pub(crate) struct JudgeAttemptReplayRejected;

impl PipelineEvent for JudgeAttemptReplayRejected {
    fn event_name(&self) -> &'static str {
        "evolution.judge_attempt_replay_rejected"
    }
}
