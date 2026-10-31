use super::store::Prepared;
use crate::core::evolution::judge::JudgeOutput;
use crate::tenancy::receipts::{owner::Owner as ReceiptOwner, store::Candidate};
use crate::tenancy::{
    admission::LearningLease, learning::source::Binding, transaction::Disposition,
};
use bitrouter_ai::tenancy::{identity::ExecutionId, operation::ErrorCode};
use std::sync::{Arc, Mutex, OnceLock, Weak};

#[derive(Clone, Copy)]
enum Phase {
    New,
    Accepted,
    Observing,
    Finalizing,
    Unused,
    Finished,
    Unknown,
}
struct State {
    phase: Phase,
    runtime_done: bool,
    output: Option<JudgeOutput>,
    observation_acknowledged: bool,
    lease: Option<LearningLease>,
    native_source: Option<crate::tenancy::execution::session::PaidSourcePin>,
}

/// Only a checked checkpoint plus an admitted execution can construct this.
/// No receipt, DTO, public control or stored record can assert clean completion.
pub(in crate::tenancy) struct Owner {
    pub binding: Binding,
    pub prepared: Arc<Prepared>,
    pub execution: ExecutionId,
    pub deadline: crate::tenancy::execution::deadline::Deadline,
    pub(in crate::tenancy) recovery: Option<super::recovery::Seed>,
    receipt: Weak<ReceiptOwner>,
    observation: OnceLock<Observation>,
    state: Mutex<State>,
    changed: tokio::sync::Notify,
}
pub(in crate::tenancy) struct Observation {
    pub output: JudgeOutput,
    pub terminal: Arc<Candidate>,
}
pub(super) struct Runtime {
    owner: Arc<Owner>,
    output: Option<JudgeOutput>,
}
impl Runtime {
    pub(super) fn output(&mut self, output: JudgeOutput) {
        self.output = Some(output);
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        if let Ok(mut state) = self.owner.state.lock() {
            state.output = self.output.take();
            state.runtime_done = true;
        }
        self.owner.changed.notify_waiters();
    }
}
impl Owner {
    pub(super) fn new(
        binding: Binding,
        prepared: Arc<Prepared>,
        receipt: &Arc<ReceiptOwner>,
        lease: LearningLease,
        deadline: crate::tenancy::execution::deadline::Deadline,
    ) -> anyhow::Result<Arc<Self>> {
        let recovery = receipt.learning_seed(&binding, &prepared)?;
        Ok(Arc::new(Self {
            binding,
            prepared,
            execution: receipt.id().clone(),
            recovery,
            receipt: Arc::downgrade(receipt),
            observation: OnceLock::new(),
            deadline,
            state: Mutex::new(State {
                phase: Phase::New,
                runtime_done: false,
                output: None,
                observation_acknowledged: false,
                lease: Some(lease),
                native_source: None,
            }),
            changed: tokio::sync::Notify::new(),
        }))
    }
    pub(super) fn retain_native_source(
        &self,
        pin: crate::tenancy::execution::session::PaidSourcePin,
    ) -> Result<(), ErrorCode> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if !matches!(state.phase, Phase::New) || state.native_source.is_some() {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        state.native_source = Some(pin);
        Ok(())
    }
    pub(super) fn acceptance_status(&self) -> bitrouter_ai::core::protocol::CommitStatus {
        use bitrouter_ai::core::protocol::CommitStatus;
        match self.state.lock().map(|state| state.phase) {
            Ok(Phase::Unused) => CommitStatus::NotCommitted,
            Ok(Phase::Accepted | Phase::Observing | Phase::Finalizing | Phase::Finished) => {
                CommitStatus::Committed
            }
            _ => CommitStatus::Unknown,
        }
    }
    pub(super) fn runtime(self: &Arc<Self>) -> Runtime {
        Runtime {
            owner: self.clone(),
            output: None,
        }
    }
    pub(in crate::tenancy) fn accepted(&self, fresh: bool) {
        if let Ok(mut state) = self.state.lock() {
            state.phase = if fresh {
                Phase::Accepted
            } else {
                Phase::Unused
            };
        }
        self.changed.notify_waiters();
    }
    pub(super) fn prepare_observation(&self, output: JudgeOutput) -> Result<(), ErrorCode> {
        let seed = self
            .recovery
            .as_ref()
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        let terminal = self
            .receipt
            .upgrade()
            .ok_or(ErrorCode::AuthorityUnavailable)?
            .learning_terminal()?;
        seed.receipt(&terminal)
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        let encoded = crate::harness::trajectory::owned::io::json(&output, 512 * 1024)
            .map_err(|_| ErrorCode::CapacityExceeded)?;
        if let Some(existing) = self.observation.get() {
            if existing.terminal.learning_digest() != terminal.learning_digest()
                || crate::harness::trajectory::owned::io::json(&existing.output, 512 * 1024)
                    .map_err(|_| ErrorCode::StorageUnavailable)?
                    != encoded
            {
                return Err(ErrorCode::StorageUnavailable);
            }
            return Ok(());
        }
        self.observation
            .set(Observation { output, terminal })
            .map_err(|_| ErrorCode::AuthorityUnavailable)
    }
    pub(in crate::tenancy) fn observation(&self) -> Result<&Observation, ErrorCode> {
        self.observation
            .get()
            .ok_or(ErrorCode::AuthorityUnavailable)
    }
    pub(super) fn begin_observe(&self) -> Result<bool, ErrorCode> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if state.observation_acknowledged {
            return Ok(false);
        }
        if !matches!(state.phase, Phase::Accepted) || self.observation.get().is_none() {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        state.phase = Phase::Observing;
        Ok(true)
    }
    pub(in crate::tenancy) fn observed(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.observation_acknowledged = true;
            state.phase = Phase::Accepted;
        }
        self.changed.notify_waiters();
    }
    pub(in crate::tenancy) fn observe_settled(&self, disposition: Disposition) {
        if let Ok(mut state) = self.state.lock()
            && matches!(state.phase, Phase::Observing)
        {
            state.phase = match disposition {
                Disposition::NoTransaction | Disposition::RolledBack => Phase::Accepted,
                _ => Phase::Unknown,
            };
        }
        self.changed.notify_waiters();
    }
    pub(super) fn begin_finish(&self) -> Result<(), ErrorCode> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if !matches!(state.phase, Phase::Accepted)
            || !state.runtime_done
            || (self.recovery.is_some()
                && state.output.is_some()
                && !state.observation_acknowledged)
        {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        state.phase = Phase::Finalizing;
        Ok(())
    }
    pub(in crate::tenancy) fn finish_settled(&self, disposition: Disposition) {
        if let Ok(mut state) = self.state.lock()
            && matches!(state.phase, Phase::Finalizing)
        {
            state.phase = match disposition {
                Disposition::NoTransaction | Disposition::RolledBack => Phase::Accepted,
                _ => Phase::Unknown,
            };
        }
        self.changed.notify_waiters();
    }
    pub(in crate::tenancy) fn finished(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.phase = Phase::Finished;
        }
        self.changed.notify_waiters();
    }
    pub(in crate::tenancy) fn settled(&self, disposition: Disposition) {
        if let Ok(mut state) = self.state.lock()
            && matches!(state.phase, Phase::New)
        {
            state.phase = match disposition {
                Disposition::NoTransaction | Disposition::RolledBack => Phase::Unused,
                _ => Phase::Unknown,
            };
        }
        self.changed.notify_waiters();
    }
    pub(in crate::tenancy) fn output(&self) -> Result<Option<JudgeOutput>, ErrorCode> {
        let state = self
            .state
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if !state.runtime_done || !matches!(state.phase, Phase::Finalizing | Phase::Finished) {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        Ok(state.output.clone())
    }
    pub(super) async fn wait_terminal(&self) -> Result<(), ErrorCode> {
        let deadline = self.cleanup_deadline()?;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self
                .state
                .lock()
                .map_err(|_| ErrorCode::AuthorityUnavailable)?
                .phase
            {
                Phase::Finished | Phase::Unused => return Ok(()),
                Phase::Unknown => return Err(ErrorCode::AuthorityUnavailable),
                _ => {}
            }
            tokio::time::timeout_at(deadline, changed)
                .await
                .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        }
    }
    /// Only Registry uses this after its actual local pipeline joins. This is
    /// a readiness predicate, never an admission or a completed-receipt claim.
    pub(in crate::tenancy) async fn stopped(&self) -> Result<bool, ErrorCode> {
        let deadline = self.cleanup_deadline()?;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self
                    .state
                    .lock()
                    .map_err(|_| ErrorCode::AuthorityUnavailable)?;
                match (state.phase, state.runtime_done) {
                    (Phase::Finished | Phase::Unused, true) => return Ok(false),
                    (Phase::Accepted | Phase::Finalizing, true)
                        if self.recovery.is_some()
                            && (state.output.is_none() || state.observation_acknowledged) =>
                    {
                        return Ok(true);
                    }
                    (Phase::Unknown, _) => return Err(ErrorCode::AuthorityUnavailable),
                    _ => {}
                }
            }
            tokio::time::timeout_at(deadline, changed)
                .await
                .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        }
    }
    pub(in crate::tenancy) async fn drain(self: &Arc<Self>) -> Result<(), ErrorCode> {
        let deadline = self.cleanup_deadline()?;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let (phase, done) = {
                let state = self
                    .state
                    .lock()
                    .map_err(|_| ErrorCode::AuthorityUnavailable)?;
                (state.phase, state.runtime_done)
            };
            match (phase, done) {
                (Phase::Finished | Phase::Unused, true) => break,
                (Phase::Accepted, true) => {
                    let output = {
                        let state = self
                            .state
                            .lock()
                            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
                        if self.recovery.is_some() && !state.observation_acknowledged {
                            state.output.clone()
                        } else {
                            None
                        }
                    };
                    if let Some(output) = output {
                        super::queue::observe(self.clone(), output).await?;
                    }
                    if let Err(error) = super::queue::finish(self.clone()).await
                        && !matches!(
                            self.state
                                .lock()
                                .map_err(|_| ErrorCode::AuthorityUnavailable)?
                                .phase,
                            Phase::Finished
                        )
                    {
                        return Err(error);
                    }
                }
                (Phase::Unknown, _) => return Err(ErrorCode::AuthorityUnavailable),
                _ => {
                    tokio::time::timeout_at(deadline, changed)
                        .await
                        .map_err(|_| ErrorCode::AuthorityUnavailable)?;
                }
            }
        }
        let (lease, source) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| ErrorCode::AuthorityUnavailable)?;
            (state.lease.take(), state.native_source.take())
        };
        drop((lease, source));
        Ok(())
    }

    pub(super) fn cleanup_deadline(&self) -> Result<tokio::time::Instant, ErrorCode> {
        self.receipt
            .upgrade()
            .map(|receipt| receipt.cleanup_deadline())
            .ok_or(ErrorCode::AuthorityUnavailable)
    }
}
