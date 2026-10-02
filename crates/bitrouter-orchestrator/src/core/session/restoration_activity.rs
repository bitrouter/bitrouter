//! A bounded stop-report bridge while the restored session awaits its ACKs.

use super::*;
use crate::core::protocol::{ToolObservation, ToolStatus};

/// The authenticated host installs this observer before the first restoration
/// commit. It must report actual stops concurrently with commit waits. The
/// timestamp is on the restoring host's monotonic clock, not a remote wall clock.
/// Pending approvals remain fenced against starting during restoration.
#[derive(Clone)]
pub struct RestorationActivity {
    shared: std::sync::Weak<Shared>,
}

impl RestorationActivity {
    pub(super) fn new(session: &CoreSession) -> Self {
        Self {
            shared: Arc::downgrade(&session.shared),
        }
    }

    /// Entry to the restore operation, before validation and artifact reads.
    /// On registration, replay locally captured stops at or after this instant
    /// before waiting for any restoration checkpoint acknowledgement.
    pub async fn started_at(&self) -> Result<Instant, CoreError> {
        let shared = self.shared.upgrade().ok_or_else(closed)?;
        shared
            .live
            .lock()
            .await
            .restoration_started
            .ok_or_else(closed)
    }

    pub async fn require_quiescent(&self) -> Result<(), CoreError> {
        let shared = self.shared.upgrade().ok_or_else(closed)?;
        let live = shared.live.lock().await;
        if live.activity.is_running() {
            Err(reject(
                ErrorCode::RecoveryRequired,
                "running restoration requires a lifecycle observer",
            ))
        } else {
            Ok(())
        }
    }

    /// Record a stop without inferring an outcome or waiting for a pending
    /// checkpoint. Exact repeated stop reports are harmless. The core commits
    /// an ordinary tool.status receipt before opening execution admission.
    pub async fn stopped(
        &self,
        invocation_id: &str,
        attempt_id: &str,
        at: Instant,
    ) -> Result<(), CoreError> {
        let shared = self.shared.upgrade().ok_or_else(closed)?;
        let mut live = shared.live.lock().await;
        let started = live.restoration_started.ok_or_else(closed)?;
        if at < started || at > Instant::now() {
            return Err(reject(
                ErrorCode::OperationConflict,
                "stop is outside the restoration clock interval",
            ));
        }
        let state = live.pending.as_ref().unwrap_or(&live.state);
        let call = state
            .agents
            .values()
            .filter_map(|agent| agent.turn.as_ref())
            .flat_map(|turn| &turn.invocations)
            .find(|call| call.dispatch.invocation_id == invocation_id)
            .ok_or_else(|| reject(ErrorCode::InvalidToolResult, "unknown restoring invocation"))?;
        let observation = ToolObservation {
            invocation_id: invocation_id.into(),
            attempt_id: attempt_id.into(),
            status: ToolStatus::Stopped,
            evidence: Vec::new(),
        };
        tool_status::validate_identity(call, &observation)?;
        if live.restoration_stops.contains_key(invocation_id)
            || tool_status::observed(call, ToolStatus::Stopped)
        {
            return Ok(());
        }
        if !tool_status::activity_ids(state).contains(&format!("tool/{invocation_id}")) {
            return Err(reject(
                ErrorCode::OperationConflict,
                "restoring tool was not running",
            ));
        }
        live.activity
            .finish_at(&format!("tool/{invocation_id}"), at);
        live.restoration_stops
            .insert(invocation_id.into(), (id("restore_stop"), observation));
        Ok(())
    }
}

fn closed() -> CoreError {
    reject(
        ErrorCode::Busy,
        "restoration observer is closed; use the live session",
    )
}

pub(super) async fn finish(session: &CoreSession) -> Result<(), CoreError> {
    loop {
        session
            .shared
            .harness
            .synchronize_restoration(RestorationActivity::new(session))
            .await?;
        let next = {
            let mut live = session.shared.live.lock().await;
            if let Some(next) = live.restoration_stops.values().next() {
                Some(next.clone())
            } else {
                live.restoration_started = None;
                budget::watch(session, &mut live);
                None
            }
        };
        let Some((operation, observation)) = next else {
            return Ok(());
        };
        session.tool_status(&operation, observation.clone()).await?;
        session
            .shared
            .live
            .lock()
            .await
            .restoration_stops
            .remove(&observation.invocation_id);
    }
}
