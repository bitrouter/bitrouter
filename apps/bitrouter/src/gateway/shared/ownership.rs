//! Bounded request custody and cancellation-safe, owner-only reconciliation.
//!
//! The registry never lends an existing custody handle or stores admissions.
//! Without weak handles, a sole registry reference proves that local request,
//! provider and finalization futures can no longer create work for that entry.
//! Transport buffering and native continuation compensation have separate bounds.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::execution::{
    AttemptState, ExecutionState, GatewayExecution, TerminalOutcome,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::admitted::SharedAdmission;
use super::dispatched::{Tracker, failed};
use super::engine::{Admission, AdmissionResult, Engine, Rejection};

#[derive(Clone, Copy)]
enum AdmissionPhase {
    NotStarted,
    UnknownAck,
    Accepted,
    NotAccepted,
}

pub(super) struct Custody {
    pub(super) engine: Engine,
    pub(super) execution_id: String,
    pub(super) dispatch: Arc<Tracker>,
    pub(super) capture: super::capture::Capture,
    generation: Generation,
    phase: Mutex<AdmissionPhase>,
    pipeline_failed: Mutex<bool>,
    _permit: OwnedSemaphorePermit,
}

/// The temporary pre-extractor owner. Its lifetime includes parsing and the
/// admission transaction, even when that transaction's acknowledgement is lost.
pub(super) struct Reservation(Arc<Custody>);

/// Process-local resource identity, distinct even for unpublished candidates
/// assembled from the same authority revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Generation(uuid::Uuid);

impl Generation {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

pub(super) enum Submission {
    Accepted(Arc<SharedAdmission>),
    Existing(Box<GatewayExecution>),
}

impl Reservation {
    /// Available before the admission await, including an uncertain commit.
    pub fn execution_id(&self) -> &str {
        &self.0.execution_id
    }

    pub async fn admit(self, request: &Admission<'_>) -> Result<Submission> {
        *lock(&self.0.phase) = AdmissionPhase::UnknownAck;
        match self
            .0
            .engine
            .admit_with_id(&self.0.execution_id, request)
            .await
        {
            Ok(AdmissionResult::Accepted(execution, principal)) => {
                *lock(&self.0.phase) = AdmissionPhase::Accepted;
                self.0.capture.initialize(super::capture::Inputs::new(
                    request.requirements.clone(),
                    &execution.execution_id,
                    &principal.user_id,
                    execution.operation,
                )?)?;
                Ok(Submission::Accepted(SharedAdmission::with_custody(
                    self.0,
                    request.key_hash.to_owned(),
                    execution,
                    principal,
                )))
            }
            Ok(AdmissionResult::Existing(execution)) => {
                // The duplicate belongs to its original execution. This fresh
                // candidate never acquires authority to finalize that work.
                *lock(&self.0.phase) = AdmissionPhase::NotAccepted;
                Ok(Submission::Existing(Box::new(execution)))
            }
            Err(error) => {
                // Typed rejection is produced before acceptance can commit.
                // Storage errors and cancellation retain UnknownAck instead.
                if error.downcast_ref::<Rejection>().is_some() {
                    *lock(&self.0.phase) = AdmissionPhase::NotAccepted;
                }
                Err(error)
            }
        }
    }
}

#[derive(Default)]
struct Entries {
    owned: HashMap<String, Arc<Custody>>,
    queued: VecDeque<String>,
    generations: HashMap<Generation, usize>,
}

pub(super) struct Registry {
    engine: Engine,
    slots: Arc<Semaphore>,
    entries: Mutex<Entries>,
}

impl Registry {
    pub fn new(engine: Engine, limit: usize) -> Result<Arc<Self>> {
        ensure!((1..=65_536).contains(&limit), "invalid custody limit");
        Ok(Arc::new(Self {
            engine,
            slots: Arc::new(Semaphore::new(limit)),
            entries: Mutex::new(Entries::default()),
        }))
    }

    /// Nonqueueing acquisition must precede ingress body aggregation.
    pub fn reserve_for(&self, generation: Generation) -> Result<Reservation> {
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Rejection::Unavailable)?;
        let custody = Arc::new(Custody {
            engine: self.engine.clone(),
            execution_id: uuid::Uuid::new_v4().to_string(),
            dispatch: Arc::new(Tracker::default()),
            capture: super::capture::Capture::default(),
            generation,
            phase: Mutex::new(AdmissionPhase::NotStarted),
            pipeline_failed: Mutex::new(false),
            _permit: permit,
        });
        // Clone the live request reference before publishing the map entry.
        let reservation = Reservation(custody.clone());
        let mut entries = lock(&self.entries);
        ensure!(
            !entries.owned.contains_key(&custody.execution_id),
            "local execution identity collision"
        );
        entries.queued.push_back(custody.execution_id.clone());
        entries.owned.insert(custody.execution_id.clone(), custody);
        *entries.generations.entry(generation).or_default() += 1;
        Ok(reservation)
    }

    #[cfg(test)]
    pub fn reserve(&self) -> Result<Reservation> {
        self.reserve_for(Generation(uuid::Uuid::nil()))
    }

    pub fn pending(&self) -> usize {
        lock(&self.entries).owned.len()
    }

    #[cfg(test)]
    pub fn reconciling(&self) -> usize {
        let entries = lock(&self.entries);
        entries.owned.len().saturating_sub(entries.queued.len())
    }

    pub fn pending_for(&self, generation: Generation) -> usize {
        lock(&self.entries)
            .generations
            .get(&generation)
            .copied()
            .unwrap_or(0)
    }

    /// Visit one fair queue entry. Heartbeat scheduling must be independent of
    /// this awaited database work; a count-bounded batch is not a time bound.
    pub async fn reconcile_one(self: &Arc<Self>) -> Result<bool> {
        let candidate = {
            let mut entries = lock(&self.entries);
            let Some(id) = entries.queued.pop_front() else {
                return Ok(false);
            };
            let custody = entries
                .owned
                .get(&id)
                .context("custody queue lost its owner")?;
            if Arc::strong_count(custody) != 1 {
                entries.queued.push_back(id);
                return Ok(false);
            }
            Candidate {
                registry: self.clone(),
                custody: custody.clone(),
                retired: false,
            }
        };
        candidate.custody.reconcile().await?;
        candidate.retire()?;
        Ok(true)
    }
}

/// Owns exactly one popped queue entry. Drop only restores discoverability;
/// the map keeps both the evidence and permit through cancellation or failure.
struct Candidate {
    registry: Arc<Registry>,
    custody: Arc<Custody>,
    retired: bool,
}

impl Candidate {
    fn retire(mut self) -> Result<()> {
        let mut entries = lock(&self.registry.entries);
        let count = entries
            .generations
            .get(&self.custody.generation)
            .copied()
            .context("custody lost its generation")?;
        ensure!(count > 0, "custody generation count underflow");
        ensure!(
            entries.owned.remove(&self.custody.execution_id).is_some(),
            "custody lost its entry"
        );
        if count == 1 {
            entries.generations.remove(&self.custody.generation);
        } else {
            entries
                .generations
                .insert(self.custody.generation, count - 1);
        }
        self.retired = true;
        Ok(())
    }
}

impl Drop for Candidate {
    fn drop(&mut self) {
        if !self.retired {
            lock(&self.registry.entries)
                .queued
                .push_back(self.custody.execution_id.clone());
        }
    }
}

impl Custody {
    /// A frozen failure belongs to the registry owner, even if its delivery
    /// future is never polled or is cancelled before the terminal commit.
    pub(super) fn record_pipeline_outcome(&self, successful: bool) {
        if !successful {
            *lock(&self.pipeline_failed) = true;
        }
    }

    /// Normal delivery and quiescent retirement use the same decision. Keep
    /// observed provider errors and their usage intact; a failed pipeline over
    /// an otherwise successful provider still cannot finalize as success.
    pub(super) fn terminal_outcome(&self, execution: &GatewayExecution) -> TerminalOutcome {
        let observed = execution
            .attempts
            .iter()
            .rev()
            .find_map(|attempt| match &attempt.state {
                AttemptState::Terminal { result } => Some(&result.outcome),
                _ => None,
            });
        match observed {
            Some(outcome @ TerminalOutcome::Failed { .. }) => outcome.clone(),
            _ if *lock(&self.pipeline_failed) => failed("pipeline_failed"),
            Some(outcome) => outcome.clone(),
            None => failed("request_ended_before_dispatch"),
        }
    }

    async fn reconcile(&self) -> Result<()> {
        let phase = *lock(&self.phase);
        match phase {
            AdmissionPhase::NotStarted | AdmissionPhase::NotAccepted => return Ok(()),
            AdmissionPhase::UnknownAck => {
                if self
                    .engine
                    .quiescent_execution(&self.execution_id)
                    .await?
                    .is_none()
                {
                    return Ok(());
                }
                *lock(&self.phase) = AdmissionPhase::Accepted;
            }
            AdmissionPhase::Accepted => {}
        }
        self.dispatch
            .reconcile(&self.engine, &self.execution_id, true)
            .await?;
        let execution = self.engine.owned_execution(&self.execution_id).await?;
        match execution.state {
            ExecutionState::Terminal { .. }
            | ExecutionState::Unknown { .. }
            | ExecutionState::Resolved { .. } => Ok(()),
            ExecutionState::Accepted | ExecutionState::Running => {
                // Persisted attempts remain authoritative after the in-memory
                // tracker has cleared. A failed locked transition retains this
                // entry; a stale read never proves that capacity was released.
                let outcome = self.terminal_outcome(&execution);
                let inputs = self.capture.terminal()?;
                self.engine
                    .finish_with_inputs(&self.execution_id, &outcome, inputs.as_deref())
                    .await?;
                Ok(())
            }
        }
    }

    #[cfg(test)]
    pub(super) fn fixture(engine: Engine, execution_id: String) -> Result<Arc<Self>> {
        let permit = Arc::new(Semaphore::new(1)).try_acquire_owned()?;
        Ok(Arc::new(Self {
            engine,
            execution_id,
            dispatch: Arc::new(Tracker::default()),
            capture: super::capture::Capture::default(),
            generation: Generation::new(),
            phase: Mutex::new(AdmissionPhase::Accepted),
            pipeline_failed: Mutex::new(false),
            _permit: permit,
        }))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
pub(super) mod tests;
