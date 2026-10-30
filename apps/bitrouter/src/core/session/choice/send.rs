//! The final Core gate spans the synchronous owned-provider send admission.
use super::*;
use crate::gateway::{
    error::{BitrouterError, Result as GatewayResult},
    model::protocol::decisions::DecisionsCodec,
};

impl SendGate {
    pub(crate) async fn arm<T>(
        &self,
        body: &Value,
        provider_model: &str,
        work: impl FnOnce() -> GatewayResult<T>,
    ) -> GatewayResult<T> {
        wire::encoded(body, self.profile.policy.max_request_bytes)
            .map_err(|()| BitrouterError::bad_request("owned Choice request exceeds its bound"))?;
        let actual = DecisionsCodec::parse_request(body.clone())
            .map_err(|_| BitrouterError::bad_request("invalid owned Choice serving body"))?;
        let mut expected = self.native.as_ref().clone();
        expected.model = provider_model.into();
        if actual != expected {
            return Err(BitrouterError::Forbidden(
                "owned Choice serving body changed".into(),
            ));
        }
        tokio::select! {
            biased;
            () = self.cancellation.cancelled() => Err(BitrouterError::UpstreamUnavailable),
            () = tokio::time::sleep_until(self.deadline) => Err(BitrouterError::UpstreamUnavailable),
            outcome = self.arm_current(work) => outcome,
        }
    }

    async fn arm_current<T>(&self, work: impl FnOnce() -> GatewayResult<T>) -> GatewayResult<T> {
        let session = &self.control.session;
        let _commit = session.shared.commits.lock().await;
        // Refusing new paid work must not itself disconnect the cleanup lane.
        session.check_owned().map_err(sdk_error)?;
        session
            .shared
            .harness
            .authorize_dispatch()
            .await
            .map_err(sdk_error)?;
        session.check_owned().map_err(sdk_error)?;
        session
            .shared
            .harness
            .admit_model_work()
            .await
            .map_err(sdk_error)?;
        let head = session.shared.live.lock().await.gate.head().clone();
        let recorded = self
            .journal
            .choice(&self.record.decision_id, &head)
            .await
            .map_err(sdk_error)?;
        if recorded.as_ref() != Some(&self.record) {
            return Err(BitrouterError::UpstreamUnavailable);
        }
        let mut live = session.shared.live.lock().await;
        budget::ensure_live(&live).map_err(sdk_error)?;
        if self.cancellation.is_cancelled()
            || tokio::time::Instant::now() >= self.deadline
            || !live.gate.can_dispatch()
            || steering::provisionally_blocked(&live, &self.control.agent_id)
            || live.state.run.as_ref().is_none_or(|run| {
                run.run_id != self.control.run_id
                    || !matches!(run.status, RunStatus::Running | RunStatus::Waiting)
                    || run.model_attempts >= run.limits.model_attempts
            })
            || live
                .state
                .agents
                .get(&self.control.agent_id)
                .and_then(|agent| agent.turn.as_ref())
                .is_none_or(|turn| {
                    turn.agent_turn_id != self.control.agent_turn_id
                        || turn.status != AgentStatus::ModelRunning
                })
            || live
                .state
                .context_store
                .decisions
                .get(&self.record.decision_id)
                .is_none_or(|receipt| receipt.outcome.is_some())
        {
            return Err(BitrouterError::UpstreamUnavailable);
        }
        context_decisions::validate_decision_source(
            &live.state,
            &self.control.agent_id,
            &self.record.step_id,
            &self.source,
        )
        .map_err(sdk_error)?;
        if self
            .claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(BitrouterError::Forbidden(
                "owned Choice send was already claimed".into(),
            ));
        }
        #[cfg(all(test, unix))]
        self.control
            .owned_step
            .as_ref()
            .ok_or(BitrouterError::UpstreamUnavailable)?
            .choice_final_gate_for_test(&self.record.child_execution)
            .await
            .map_err(|_| BitrouterError::UpstreamUnavailable)?;
        live.activity.start(self.record.decision_id.clone());
        budget::watch(session, &mut live);
        // Core commit/live guards remain held across the existing synchronous
        // tenancy authority gate, and are released before any network await.
        let result = work();
        if result.is_ok() {
            self.armed.store(true, Ordering::Release);
        }
        result
    }
}
