//! Request-owned dispatch evidence. No database transaction spans provider I/O.

use std::sync::{Arc, Mutex, MutexGuard};

use bitrouter_ai::gateway::execution::{ExecutionUsage, TerminalObservation, TerminalOutcome};
use bitrouter_ai::types::{FinishReason, StreamPart, Usage, UsageOrigin};

use crate::gateway::error::{BitrouterError, Result};
use crate::gateway::pipeline::{
    context::PipelineContext, stream::PricingTargetKey, types::RoutingTarget,
};
use crate::metering::{
    pricing::PricingSource,
    tariff::{FrozenTariff, TargetTariffSnapshot},
};

use super::{
    admitted::{SharedAdmission, public_error},
    credentials::CredentialStamp,
};

#[derive(Default)]
pub(super) struct Tracker(Mutex<State>);

#[derive(Default)]
struct State {
    /// Remains set across cancellation or an ambiguous storage acknowledgement.
    occupied: bool,
    dropped: bool,
    attempt_id: Option<String>,
    evidence: Option<TerminalObservation>,
}

impl Tracker {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl SharedAdmission {
    pub fn blocks_fallback(&self) -> bool {
        self.dispatch.lock().occupied
    }

    /// The owner retains this admission until pending evidence is reconciled.
    /// A dropped attempt never spawns an unbounded detached database task.
    pub async fn reconcile_dispatch(&self) -> Result<()> {
        let pending = {
            let state = self.dispatch.lock();
            if !state.occupied || !state.dropped {
                return Ok(());
            }
            (state.attempt_id.clone(), state.evidence.clone())
        };
        if let (Some(id), Some(evidence)) = pending {
            self.engine
                .record_terminal(&self.execution_id, &id, &evidence)
                .await
                .map_err(public_error)?;
            // Even successful late persistence does not authorize replay of a
            // request whose owning provider future was already interrupted.
        } else {
            self.engine
                .interrupt_attempts(&self.execution_id)
                .await
                .map_err(public_error)?;
        }
        Ok(())
    }
}

/// One send authorization. Dropping the future retains content-free evidence
/// for the bounded execution owner, including a terminal seen before disconnect.
pub(crate) struct Attempt {
    admission: Arc<SharedAdmission>,
    tariff: Option<FrozenTariff>,
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
        let tariff = ctx
            .get_events::<TargetTariffSnapshot>()
            .into_iter()
            .rev()
            .find(|snapshot| snapshot.target == PricingTargetKey::from_target(target))
            .map(|snapshot| snapshot.tariff.clone());
        // Construct before either await: cancellation of preparation can leave
        // a committed reservation even if its acknowledgement was never read.
        let attempt = Self {
            admission,
            tariff,
            usage: None,
            terminal: None,
            completed: false,
        };
        let reserved = attempt
            .admission
            .engine
            .prepare_attempt(
                &attempt.admission.key_hash,
                &attempt.admission.execution_id,
                credential,
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

    fn observation(&self) -> Option<TerminalObservation> {
        self.terminal.as_ref().map(|outcome| TerminalObservation {
            outcome: outcome.clone(),
            usage: usage_evidence(self.usage.as_ref(), self.tariff.as_ref()),
            recorded_at_ms: 0, // Replaced by the transaction's database clock.
        })
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
            .record_terminal(&self.admission.execution_id, &id, &observation)
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
            let mut state = self.admission.dispatch.lock();
            state.dropped = true;
            // Store only counts and stable outcome codes; raw usage and content
            // remain out of the coordinator's reconciliation queue.
            state.evidence = self.observation();
        }
    }
}

pub(crate) fn failed(code: &str) -> TerminalOutcome {
    TerminalOutcome::Failed { code: code.into() }
}

fn usage_evidence(usage: Option<&Usage>, tariff: Option<&FrozenTariff>) -> ExecutionUsage {
    let usage = usage.filter(|usage| usage.origin != UsageOrigin::Unknown);
    ExecutionUsage {
        origin: usage.map_or(UsageOrigin::Unknown, |usage| usage.origin),
        prompt_tokens: usage.map(|usage| usage.prompt_tokens),
        completion_tokens: usage.map(|usage| usage.completion_tokens),
        reasoning_tokens: usage.map(|usage| usage.reasoning_tokens),
        cache_read_tokens: usage.map(|usage| usage.cache_read_tokens),
        cache_write_tokens: usage.map(|usage| usage.cache_write_tokens),
        total_tokens: usage
            .and_then(|usage| usage.prompt_tokens.checked_add(usage.completion_tokens)),
        cost_micro_usd: usage.and_then(|usage| {
            tariff.and_then(|tariff| {
                tariff
                    .charge_evidence(usage, PricingSource::Configured)
                    .charge_micro_usd
                    .and_then(|value| u64::try_from(value).ok())
            })
        }),
    }
}
