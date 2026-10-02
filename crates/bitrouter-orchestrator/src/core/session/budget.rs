//! Active-time admission and durable cleanup after the shared budget expires.

use super::*;
use crate::core::checkpoint::ToolStartFence;

pub(super) fn ensure(state: &SessionSnapshot) -> Result<(), CoreError> {
    if let Some(run) = &state.run {
        if let Some(error) = &run.resource_error {
            return Err(reject(error.code, &error.message));
        }
        if run.active_ms >= run.limits.active_seconds.saturating_mul(1000) {
            return Err(reject(
                ErrorCode::LimitExceeded,
                "run active-time budget exhausted",
            ));
        }
    }
    Ok(())
}

pub(super) fn ensure_live(live: &LiveSession) -> Result<(), CoreError> {
    ensure(&live.state)?;
    if live.state.run.as_ref().is_some_and(|run| {
        live.activity.elapsed_ms() >= run.limits.active_seconds.saturating_mul(1000)
    }) {
        return Err(reject(
            ErrorCode::LimitExceeded,
            "run active-time budget exhausted",
        ));
    }
    Ok(())
}

pub(super) fn start_fences(state: &SessionSnapshot, kind: &str) -> Vec<ToolStartFence> {
    if kind != "run.limit_reached" {
        return Vec::new();
    }
    state
        .agents
        .values()
        .filter_map(|agent| agent.turn.as_ref())
        .filter(|turn| {
            state
                .run
                .as_ref()
                .is_some_and(|run| run.run_id == turn.run_id)
        })
        .flat_map(|turn| &turn.invocations)
        .filter(|call| call.result.is_none())
        .map(|call| ToolStartFence {
            invocation_id: call.dispatch.invocation_id.clone(),
            attempt_id: call.dispatch.attempt_id.clone(),
        })
        .collect()
}

/// At most one timer observes a session. Waiting retains only a weak session
/// reference; a dropped host/session is not kept alive until its deadline.
pub(super) fn watch(session: &CoreSession, live: &mut LiveSession) {
    session.shared.budget_changed.notify_one();
    if live.budget_watching || !eligible(live) {
        return;
    }
    live.budget_watching = true;
    let shared = Arc::downgrade(&session.shared);
    let changed = session.shared.budget_changed.clone();
    tokio::spawn(watch_loop(shared, changed));
}

fn eligible(live: &LiveSession) -> bool {
    live.state.run.as_ref().is_some_and(|run| {
        run.resource_error.is_none()
            && matches!(run.status, RunStatus::Running | RunStatus::Waiting)
            && (live.activity.is_running()
                || live.activity.elapsed_ms().max(run.active_ms)
                    >= run.limits.active_seconds.saturating_mul(1000))
    })
}

async fn watch_loop(shared: std::sync::Weak<Shared>, changed: Arc<Notify>) {
    loop {
        let notified = changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let Some(current) = shared.upgrade() else {
            return;
        };
        let remaining = {
            let mut live = current.live.lock().await;
            if !eligible(&live) || live.disconnected.is_cancelled() {
                live.budget_watching = false;
                return;
            }
            live.state.run.as_ref().map(|run| {
                (
                    run.run_id.clone(),
                    run.limits
                        .active_seconds
                        .saturating_mul(1000)
                        .saturating_sub(live.activity.elapsed_ms().max(run.active_ms)),
                )
            })
        };
        let Some((run_id, remaining_ms)) = remaining else {
            return;
        };
        if remaining_ms == 0 {
            let session = CoreSession { shared: current };
            let enforced = session.enforce_active_time(Some(&run_id)).await;
            {
                let mut live = session.shared.live.lock().await;
                live.budget_watching = false;
                if matches!(enforced, Ok(false)) {
                    watch(&session, &mut live);
                }
            }
            match enforced {
                Ok(true) => {
                    // A live driver receives the state-change notification. If
                    // it has returned for tools, perform the same cleanup here.
                    if let Err(error) = Box::pin(session.drive()).await
                        && error.code != ErrorCode::Busy
                    {
                        tracing::debug!(%error, "resource cleanup awaits further evidence");
                    }
                }
                Ok(false) => {}
                Err(error) => tracing::debug!(%error, "resource boundary awaits durable authority"),
            }
            return;
        }
        drop(current);
        tokio::select! {
            _ = &mut notified => {}
            _ = tokio::time::sleep(std::time::Duration::from_millis(remaining_ms.min(60_000))) => {}
        }
    }
}

pub(super) fn validate(state: &SessionSnapshot) -> Result<(), CoreError> {
    if let Some(run) = &state.run
        && let Some(error) = &run.resource_error
        && (error.code != ErrorCode::LimitExceeded
            || error.commit_status != CommitStatus::Committed
            || run.active_ms < run.limits.active_seconds.saturating_mul(1000)
            || matches!(run.status, RunStatus::Completed | RunStatus::Cancelled)
            || state
                .agents
                .values()
                .filter_map(|agent| agent.turn.as_ref())
                .any(|turn| {
                    turn.run_id == run.run_id
                        && !turn.status.terminal()
                        && !turn.cancellation_requested
                }))
    {
        return Err(reject(
            ErrorCode::CheckpointConflict,
            "resource failure lost its budget or cleanup facts",
        ));
    }
    Ok(())
}

impl CoreSession {
    pub(super) async fn enforce_active_time(
        &self,
        expected_run: Option<&str>,
    ) -> Result<bool, CoreError> {
        let _input = self.shared.inputs.lock().await;
        {
            let live = self.shared.live.lock().await;
            if !eligible(&live)
                || ensure_live(&live).is_ok()
                || expected_run
                    .is_some_and(|id| live.state.run.as_ref().is_none_or(|run| run.run_id != id))
            {
                return Ok(false);
            }
            if !live.gate.can_dispatch() {
                return Err(reject(
                    ErrorCode::CheckpointUnavailable,
                    "resource failure awaits durable authority",
                ));
            }
        }
        self.transition("run.limit_reached", |state, _| {
            let run = active_run(state)?;
            if run.resource_error.is_some()
                || !matches!(run.status, RunStatus::Running | RunStatus::Waiting)
                || run.active_ms < run.limits.active_seconds.saturating_mul(1000)
                || expected_run.is_some_and(|id| run.run_id != id)
            {
                return Err(reject(ErrorCode::Busy, "resource boundary changed"));
            }
            let mut error = reject(ErrorCode::LimitExceeded, "run active-time budget exhausted");
            error.commit_status = CommitStatus::Committed;
            run.resource_error = Some(error.clone());
            let run_id = run.run_id.clone();
            for agent in state.agents.values_mut() {
                agent.queue.retain(|work| work.run_id != run_id);
                if let Some(turn) = &mut agent.turn
                    && turn.run_id == run_id
                    && !turn.status.terminal()
                {
                    turn.status = AgentStatus::Cancelling;
                    turn.cancellation_requested = true;
                }
            }
            encode(&error)
        })
        .await?;
        Ok(true)
    }
}
