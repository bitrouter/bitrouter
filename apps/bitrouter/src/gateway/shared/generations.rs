//! Bounded ownership of complete applications, including failed candidates.

mod resource;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, ensure};
use tokio::time::Instant;

use crate::config::{Config, gateway_state::GatewayStateConfig};
use crate::continuation::ContinuationKey;
use crate::gateway::providers::auth::AuthAppliers;
use crate::policy::Policy;
use crate::policy_lock::PolicyLock;
use crate::trajectory::canonical::CorrelationKey;

use super::authority::{Authority, Revision};
use super::continuation::registration::Registration;
use super::engine::Engine;
use super::http::Serving;
use super::ownership::Generation;
use super::telemetry::Telemetry;
use resource::Resource;

/// Prepared authority/configuration only; this type contains no producer lease.
pub(super) struct Input {
    pub revision: Revision,
    pub config: Config,
    pub authority: Authority,
    pub continuation_key: ContinuationKey,
    pub correlation_key: CorrelationKey,
    pub auth_appliers: AuthAppliers,
    pub policies: Vec<Policy>,
    pub policy_lock: Option<PolicyLock>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BuildFailure {
    Assembly,
    Snapshot,
    Panicked,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Attempt {
    id: uuid::Uuid,
    deadline: Instant,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Report {
    pub generation: Generation,
    pub attempt: Attempt,
    pub reclaimed: bool,
    pub build_joined: bool,
    pub build_failure: Option<BuildFailure>,
    pub pipeline: Option<crate::gateway::pipeline::owned_tasks::Summary>,
    pub native: Option<crate::continuation::owned::Drain>,
    pub producer_closed: bool,
    pub custody: usize,
    pub custody_failed: bool,
}

impl Report {
    pub fn is_clean(self) -> bool {
        self.reclaimed
            && self.build_failure.is_none()
            && !self.custody_failed
            && self.pipeline.is_none_or(|summary| summary.is_clean())
            && self.native.is_none_or(|drain| drain.is_clean())
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Candidate {
    pub generation: Generation,
    pub joined: bool,
    pub ready: bool,
    pub failure: Option<BuildFailure>,
}

struct State {
    current: Option<Arc<Resource>>,
    retired: Vec<Arc<Resource>>,
    candidate: Option<Arc<Resource>>,
    history: VecDeque<Report>,
    closed: bool,
}

pub(super) struct Generations {
    serving: Arc<Serving>,
    telemetry: Arc<Telemetry>,
    namespace: String,
    max_retired: usize,
    state: Mutex<State>,
}

impl Generations {
    pub fn new(
        engine: Engine,
        config: &crate::config::gateway_state::SharedGatewayConfig,
        telemetry: Arc<Telemetry>,
    ) -> Result<Self> {
        ensure!(
            (1..=64).contains(&config.max_retired),
            "invalid retired generation limit"
        );
        ensure!(
            engine.namespace() == config.namespace,
            "generation manager namespace differs from its engine"
        );
        Ok(Self {
            serving: Serving::empty(engine, config)?,
            telemetry,
            namespace: config.namespace.clone(),
            max_retired: config.max_retired,
            state: Mutex::new(State {
                current: None,
                retired: Vec::with_capacity(config.max_retired),
                candidate: None,
                history: VecDeque::with_capacity(config.max_retired + 2),
                closed: false,
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub fn serving(&self) -> &Arc<Serving> {
        &self.serving
    }

    /// The slot exists before the first database transaction or assembly task.
    pub fn start_candidate(&self, input: Input) -> Result<Generation> {
        let runtime = tokio::runtime::Handle::try_current()?;
        ensure!(
            input.revision.config_generation > 0 && input.revision.key_generation > 0,
            "candidate requires a published authority revision"
        );
        let GatewayStateConfig::Shared(config) = &input.config.gateway_state else {
            anyhow::bail!("candidate requires shared process configuration");
        };
        ensure!(
            config.namespace == self.namespace
                && input.authority.store().namespace() == self.namespace,
            "candidate namespace differs from its process"
        );
        let mut state = self.lock();
        ensure!(!state.closed, "generation publication is closed");
        ensure!(
            state.candidate.is_none(),
            "a candidate still owns the build slot"
        );
        let registration = Arc::new(Registration::new(
            input.authority.clone(),
            input.continuation_key.clone(),
        ));
        ensure!(
            !state
                .current
                .iter()
                .chain(state.retired.iter())
                .any(|resource| resource.producer_id() == registration.id()),
            "candidate producer identity is already owned"
        );
        let resource = Arc::new(Resource::new(input.revision, registration));
        let id = resource.id;
        state.candidate = Some(resource.clone());
        resource.start(input, self.telemetry.clone(), &runtime);
        Ok(id)
    }

    pub async fn wait_candidate(&self, deadline: Instant) -> Result<Candidate> {
        let resource = self
            .lock()
            .candidate
            .clone()
            .context("no candidate is retained")?;
        resource.wait_build(deadline).await
    }

    /// Keep the previous complete application before releasing the publication
    /// gate's snapshot. The retired vector is preallocated to the exact bound.
    pub fn publish_candidate(&self) -> Result<Generation> {
        let (id, previous) = {
            let mut state = self.lock();
            ensure!(!state.closed, "generation publication is closed");
            ensure!(
                state.current.is_none() || state.retired.len() < self.max_retired,
                "retired generation budget is full"
            );
            let candidate = state
                .candidate
                .as_ref()
                .context("no candidate is retained")?;
            ensure!(!candidate.is_retiring(), "candidate is being discarded");
            let components = candidate
                .components()
                .context("candidate build has not joined")?;
            let snapshot = components
                .snapshot
                .as_ref()
                .context("candidate has no valid snapshot")?
                .clone();
            // All checks precede mutation; failure restores the same candidate.
            let candidate = state
                .candidate
                .take()
                .context("candidate slot disappeared")?;
            let previous = match self.serving.install(snapshot) {
                Ok(previous) => previous,
                Err(error) => {
                    state.candidate = Some(candidate);
                    return Err(error);
                }
            };
            let id = candidate.id;
            if let Some(old) = state.current.replace(candidate) {
                old.mark_retiring();
                state.retired.push(old);
            }
            (id, previous)
        };
        drop(previous);
        Ok(id)
    }

    pub fn discard_candidate(&self) -> Result<Generation> {
        let state = self.lock();
        let candidate = state
            .candidate
            .as_ref()
            .context("no candidate is retained")?;
        candidate.mark_retiring();
        Ok(candidate.id)
    }

    /// Terminal withdrawal also closes models/metrics acquisition. Current and
    /// candidate stay in their own slots, even when the retired budget is full.
    pub fn withdraw(&self) {
        let mut state = self.lock();
        state.closed = true;
        self.serving.withdraw();
        for resource in state
            .current
            .iter()
            .chain(state.retired.iter())
            .chain(state.candidate.iter())
        {
            resource.mark_retiring();
        }
    }

    fn retained(&self, id: Generation) -> Result<Arc<Resource>> {
        let state = self.lock();
        state
            .current
            .iter()
            .chain(state.retired.iter())
            .chain(state.candidate.iter())
            .find(|resource| resource.id == id)
            .cloned()
            .context("generation is not retained")
    }

    /// Explicitly choose a new attempt. Retrying an existing token never renews D.
    pub fn begin_retirement(&self, id: Generation, deadline: Instant) -> Result<Attempt> {
        self.retained(id)?.begin_attempt(deadline)
    }

    pub async fn retire(&self, id: Generation, attempt: Attempt) -> Result<Report> {
        if let Some(report) = self
            .lock()
            .history
            .iter()
            .find(|report| report.generation == id && report.attempt == attempt)
            .copied()
        {
            return Ok(report);
        }
        let resource = self.retained(id)?;
        let _waiter = resource.waiter()?;
        let mut report = resource.retire(attempt, &self.serving).await?;
        if !report.reclaimed {
            return Ok(report);
        }
        let removed = {
            let mut state = self.lock();
            let removed = if state
                .current
                .as_ref()
                .is_some_and(|current| current.id == id)
            {
                ensure!(state.closed, "cannot reclaim the serving generation");
                state.current.take()
            } else if state
                .candidate
                .as_ref()
                .is_some_and(|candidate| candidate.id == id)
            {
                state.candidate.take()
            } else {
                state
                    .retired
                    .iter()
                    .position(|old| old.id == id)
                    .map(|index| state.retired.remove(index))
            };
            report.reclaimed = removed.is_some();
            if report.reclaimed {
                if state.history.len() == self.max_retired + 2 {
                    state.history.pop_front();
                }
                state.history.push_back(report);
            }
            removed
        };
        drop(removed);
        Ok(report)
    }

    #[cfg(test)]
    fn retained_count(&self) -> usize {
        let state = self.lock();
        usize::from(state.current.is_some())
            + state.retired.len()
            + usize::from(state.candidate.is_some())
    }
}

impl Drop for Generations {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|error| error.into_inner());
        if state.current.is_some() || !state.retired.is_empty() || state.candidate.is_some() {
            tracing::error!("Generation manager dropped before retained resources were reclaimed");
        }
    }
}

#[cfg(test)]
mod tests;
