//! Request-owned dispatch evidence. No database transaction spans provider I/O.

use std::sync::{Arc, Mutex, MutexGuard};

use bitrouter_ai::gateway::execution::TerminalOutcome;
use bitrouter_ai::types::{FinishReason, StreamPart, Usage};

use crate::gateway::error::{BitrouterError, Result};
use crate::gateway::pipeline::{
    context::PipelineContext, stream::PricingTargetKey, types::RoutingTarget,
};
use crate::metering::tariff::TargetTariffSnapshot;

use super::{
    admitted::{SharedAdmission, public_error},
    credentials::CredentialStamp,
    engine::{
        Engine,
        evidence::{Attribution, ObservedTerminal},
    },
};

#[derive(Default)]
pub(super) struct Tracker(Mutex<State>);

#[derive(Default)]
struct State {
    /// Remains set across cancellation or an ambiguous storage acknowledgement.
    occupied: bool,
    dropped: bool,
    attempt_id: Option<String>,
    evidence: Option<Arc<ObservedTerminal>>,
}

impl Tracker {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(super) async fn reconcile(
        &self,
        engine: &Engine,
        execution_id: &str,
        require_quiescent: bool,
    ) -> anyhow::Result<()> {
        let pending = {
            let state = self.lock();
            anyhow::ensure!(
                !require_quiescent || !state.occupied || state.dropped,
                "quiescent custody still has a live provider attempt"
            );
            if !state.occupied || !state.dropped {
                return Ok(());
            }
            (state.attempt_id.clone(), state.evidence.clone())
        };
        if let (Some(id), Some(evidence)) = pending {
            engine
                .record_observed_terminal(execution_id, &id, &evidence)
                .await?;
        } else {
            engine.interrupt_attempts(execution_id).await?;
        }
        // Persistence cannot authorize replay after the provider future ended.
        Ok(())
    }
}

impl SharedAdmission {
    pub fn blocks_fallback(&self) -> bool {
        self.dispatch.lock().occupied
    }

    /// The owner retains this admission until pending evidence is reconciled.
    /// A dropped attempt never spawns an unbounded detached database task.
    pub async fn reconcile_dispatch(&self) -> Result<()> {
        self.dispatch
            .reconcile(&self.engine, &self.execution_id, false)
            .await
            .map_err(public_error)
    }
}

/// One send authorization. Dropping the future retains content-free evidence
/// for the bounded execution owner, including a terminal seen before disconnect.
pub(crate) struct Attempt {
    admission: Arc<SharedAdmission>,
    attribution: Box<Attribution>,
    started: std::time::Instant,
    observed: Option<Arc<ObservedTerminal>>,
    usage: Option<Usage>,
    terminal: Option<TerminalOutcome>,
    completed: bool,
}

impl Attempt {
    pub async fn begin(
        admission: Arc<SharedAdmission>,
        credential: &CredentialStamp,
        target: &RoutingTarget,
        ctx: &PipelineContext,
    ) -> Result<Self> {
        // Verify the typed principal and caller binding before touching quota.
        admission.revalidate(ctx).await?;
        if target.api_protocol.operation() != ctx.operation() {
            return Err(BitrouterError::bad_request(
                "shared dispatch operation differs from admission",
            ));
        }
        let tariff = ctx
            .get_events::<TargetTariffSnapshot>()
            .into_iter()
            .rev()
            .find(|snapshot| snapshot.target == PricingTargetKey::from_target(target))
            .map(|snapshot| snapshot.tariff.clone());
        let attribution = Attribution::capture(target, tariff).map_err(public_error)?;
        {
            let mut state = admission.dispatch.lock();
            if state.occupied {
                return Err(BitrouterError::UpstreamUnavailable);
            }
            *state = State {
                occupied: true,
                ..Default::default()
            };
        }
        // Retain ownership before either await: preparation may commit even
        // when its acknowledgement is never observed.
        let attempt = Self {
            admission,
            attribution: Box::new(attribution),
            started: std::time::Instant::now(),
            observed: None,
            usage: None,
            terminal: None,
            completed: false,
        };
        let reserved = attempt
            .admission
            .engine
            .prepare_observed_attempt(
                &attempt.admission.key_hash,
                &attempt.admission.execution_id,
                credential,
                &attempt.attribution,
            )
            .await
            .map_err(public_error)?;
        attempt.admission.dispatch.lock().attempt_id = Some(reserved.attempt_id.clone());
        attempt
            .admission
            .engine
            .dispatch(
                &attempt.admission.key_hash,
                &attempt.admission.execution_id,
                &reserved.attempt_id,
            )
            .await
            .map_err(public_error)?;
        Ok(attempt)
    }

    pub fn observe(&mut self, part: &StreamPart) {
        match part {
            StreamPart::Usage { usage } => self.usage = Some(usage.clone()),
            StreamPart::ResponseCompleted { status, usage, .. } => {
                if let Some(usage) = usage {
                    self.usage = Some(usage.clone());
                }
                self.terminal = Some(if status == "failed" {
                    failed("provider_failed")
                } else {
                    TerminalOutcome::Succeeded
                });
            }
            StreamPart::Finish { reason } => {
                self.terminal = Some(if matches!(reason, FinishReason::Error(_)) {
                    failed("provider_failed")
                } else {
                    TerminalOutcome::Succeeded
                });
            }
            _ => {}
        }
    }

    pub async fn rejected(&mut self, status: u16) -> Result<()> {
        self.terminal = Some(failed(&format!("upstream_http_{status}")));
        self.persist().await
    }

    pub async fn complete(
        &mut self,
        outcome: TerminalOutcome,
        usage: Option<&Usage>,
    ) -> Result<()> {
        self.terminal = Some(outcome);
        self.usage = usage.cloned();
        self.persist().await
    }

    pub async fn finish_stream(&mut self) -> Result<()> {
        if self.terminal.is_none() {
            return Err(BitrouterError::UpstreamInvalidResponse {
                message: "shared stream has no provider terminal".into(),
                usage: None,
            });
        }
        self.persist().await
    }

    pub async fn failed_stream(&mut self) -> Result<()> {
        self.terminal = Some(failed("provider_failed"));
        self.persist().await
    }

    fn observation(&mut self) -> Option<Arc<ObservedTerminal>> {
        if self.observed.is_none() {
            self.observed = self.terminal.as_ref().map(|outcome| {
                Arc::new(ObservedTerminal::capture(
                    outcome.clone(),
                    self.usage.as_ref(),
                    &self.attribution,
                    u64::try_from(self.started.elapsed().as_millis()).ok(),
                ))
            });
        }
        self.observed.clone()
    }

    #[cfg(test)]
    pub(super) fn captured_terminal(&self) -> Option<&ObservedTerminal> {
        self.observed.as_deref()
    }

    async fn persist(&mut self) -> Result<()> {
        let observation = self
            .observation()
            .ok_or(BitrouterError::UpstreamUnavailable)?;
        let id = {
            let mut state = self.admission.dispatch.lock();
            state.evidence = Some(observation.clone());
            state
                .attempt_id
                .clone()
                .ok_or(BitrouterError::UpstreamUnavailable)?
        };
        self.admission
            .engine
            .record_observed_terminal(&self.admission.execution_id, &id, &observation)
            .await
            .map_err(public_error)?;
        *self.admission.dispatch.lock() = State::default();
        self.completed = true;
        Ok(())
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        if !self.completed {
            let evidence = self.observation();
            let mut state = self.admission.dispatch.lock();
            state.dropped = true;
            // Store only counts and stable outcome codes; raw usage and content
            // remain out of the coordinator's reconciliation queue.
            state.evidence = evidence;
        }
    }
}

pub(crate) fn failed(code: &str) -> TerminalOutcome {
    TerminalOutcome::Failed { code: code.into() }
}
