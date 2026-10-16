//! Request identity rejection markers consumed by always-run accounting.

use bitrouter_ai::event::PipelineEvent;
use serde::Serialize;

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
