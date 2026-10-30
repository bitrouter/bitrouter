//! One required evidence disposition per already-owned tenant execution.
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};

use super::{BODY_BYTES, State};
use crate::gateway::{
    error::{BitrouterError, Result},
    observability::decision::{CapturedEvalDecision, EvalInvocation},
    pipeline::{context::PipelineContext, hooks::RequestOutcome},
};
use crate::tenancy::{execution::TenantExecution, transaction::Disposition};
use bitrouter_ai::eval::{EvalScope, EvalSubject};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    PreEntry,
    Entered,
    Recording,
    Waiting,
    Queued,
    Writing,
    Persisted,
    Skipped,
    SkippedBeforeDecision,
}
enum BeginProgress {
    Absent,
    Waiting,
    Committed(Arc<crate::tenancy::trajectory::StartProof>),
    NoStart,
}
pub(in crate::tenancy) enum TrajectoryStart {
    Absent,
    Committed(Arc<crate::tenancy::trajectory::StartProof>),
    NoStart,
}
struct Progress {
    start: BeginProgress,
    terminal: Option<Arc<crate::tenancy::trajectory::recorder::Facts>>,
    phase: Phase,
    route_completed: bool,
    failed: bool,
    subject: Option<Arc<EvalSubject>>,
    transaction: Option<Disposition>,
}

pub(crate) struct RequiredObservation {
    progress: Mutex<Progress>,
    execution: Weak<TenantExecution>,
    state: Arc<State>,
}
impl RequiredObservation {
    pub(in crate::tenancy) fn new(
        state: Arc<State>,
        execution: Weak<TenantExecution>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state,
            execution,
            progress: Mutex::new(Progress {
                start: BeginProgress::Absent,
                terminal: None,
                phase: Phase::PreEntry,
                route_completed: false,
                failed: false,
                subject: None,
                transaction: None,
            }),
        })
    }
    fn error() -> BitrouterError {
        BitrouterError::internal("required tenant evaluation observation is incomplete")
    }
    pub(crate) fn fail(&self) {
        self.state.fail();
        if let Ok(mut state) = self.progress.lock() {
            state.failed = true;
        }
    }
    pub(in crate::tenancy) fn transaction(&self, disposition: Disposition) {
        match self.progress.lock() {
            Ok(mut state) => state.transaction = Some(disposition),
            Err(_) => self.state.fail(),
        }
    }
    pub(in crate::tenancy) fn entered(&self) -> Result<()> {
        let mut state = self.progress.lock().map_err(|_| {
            self.state.fail();
            Self::error()
        })?;
        if state.failed || state.phase != Phase::PreEntry {
            state.failed = true;
            self.state.fail();
            return Err(Self::error());
        }
        state.phase = Phase::Entered;
        Ok(())
    }
    pub(in crate::tenancy) fn route_completed(&self) {
        match self.progress.lock() {
            Ok(mut state) => state.route_completed = true,
            Err(_) => self.state.fail(),
        }
    }
    pub(in crate::tenancy) fn before_decision_end(
        &self,
        context: &PipelineContext,
        outcome: &RequestOutcome,
    ) {
        let Some(execution) = self.execution.upgrade() else {
            self.fail();
            return;
        };
        let caller = execution.caller();
        let attached = context.extension::<TenantExecution>();
        let same = attached
            .as_ref()
            .is_some_and(|value| Arc::ptr_eq(value, &execution))
            && context.request_id() == execution.id().as_str()
            && context.caller().user_id() == caller.user_id()
            && context.caller().api_key_id() == caller.api_key_id()
            && !context.caller().is_local()
            && context.caller().launch_id().is_none();
        let no_decision = context.extension::<EvalInvocation>().is_none()
            && !context.has_event::<EvalInvocation>()
            && !context.has_event::<CapturedEvalDecision>();
        match self.progress.lock() {
            Ok(mut state)
                if same
                    && (no_decision && matches!(state.start, BeginProgress::Absent)
                        || matches!(state.start, BeginProgress::NoStart)
                            && !context.has_event::<CapturedEvalDecision>())
                    && matches!(outcome, RequestOutcome::Failed(_))
                    && !state.failed
                    && !state.route_completed
                    && state.phase == Phase::Entered =>
            {
                state.phase = Phase::SkippedBeforeDecision;
            }
            Ok(_) => {}
            Err(_) => self.state.fail(),
        }
    }
    pub(crate) fn begin(self: &Arc<Self>) -> Result<Guard> {
        let mut state = self.progress.lock().map_err(|_| {
            self.state.fail();
            Self::error()
        })?;
        if state.failed || state.phase != Phase::Entered {
            state.failed = true;
            self.state.fail();
            return Err(Self::error());
        }
        state.phase = Phase::Recording;
        Ok(Guard {
            observation: self.clone(),
            finished: false,
        })
    }
    pub(in crate::tenancy) fn start_requested(&self) -> Result<()> {
        let mut state = self.progress.lock().map_err(|_| Self::error())?;
        if state.failed
            || state.phase != Phase::Entered
            || !matches!(state.start, BeginProgress::Absent)
        {
            return Err(Self::error());
        }
        state.start = BeginProgress::Waiting;
        Ok(())
    }
    pub(in crate::tenancy) fn committed_start(
        &self,
        start: Arc<crate::tenancy::trajectory::StartProof>,
    ) -> Result<()> {
        let execution = self.execution.upgrade().ok_or_else(Self::error)?;
        let mut state = self.progress.lock().map_err(|_| Self::error())?;
        if !start.matches(&execution) || !matches!(state.start, BeginProgress::Waiting) {
            return Err(Self::error());
        }
        // Publish before responding, including a receiver lost after commit.
        state.start = BeginProgress::Committed(start);
        Ok(())
    }
    pub(in crate::tenancy) fn no_start(&self, disposition: Disposition) {
        match self.progress.lock() {
            Ok(mut state)
                if matches!(state.start, BeginProgress::Waiting)
                    && matches!(
                        disposition,
                        Disposition::NoTransaction | Disposition::RolledBack
                    ) =>
            {
                state.start = BeginProgress::NoStart
            }
            Ok(state) if matches!(state.start, BeginProgress::Committed(_)) => {}
            _ => self.state.fail(),
        }
    }
    pub(in crate::tenancy) fn start_resolved(&self) -> bool {
        self.progress.lock().is_ok_and(|state| {
            matches!(
                state.start,
                BeginProgress::Committed(_) | BeginProgress::NoStart
            )
        })
    }
    pub(in crate::tenancy) fn trajectory_start(&self) -> Result<TrajectoryStart> {
        let state = self.progress.lock().map_err(|_| Self::error())?;
        if state.failed {
            return Err(Self::error());
        }
        match &state.start {
            BeginProgress::Absent => Ok(TrajectoryStart::Absent),
            BeginProgress::NoStart => Ok(TrajectoryStart::NoStart),
            BeginProgress::Committed(value) => Ok(TrajectoryStart::Committed(value.clone())),
            BeginProgress::Waiting => Err(Self::error()),
        }
    }
    pub(in crate::tenancy) fn writing_trajectory(
        &self,
        start: &Arc<crate::tenancy::trajectory::StartProof>,
        facts: &Arc<crate::tenancy::trajectory::recorder::Facts>,
    ) -> std::result::Result<(), crate::tenancy::error::Failure> {
        let mut state = self.progress.lock().map_err(|_| {
            crate::tenancy::error::Failure::from(
                bitrouter_ai::tenancy::operation::ErrorCode::AuthorityUnavailable,
            )
        })?;
        if state.failed
            || state.phase != Phase::Queued
            || !matches!(&state.start,BeginProgress::Committed(value) if Arc::ptr_eq(value,start))
            || !state
                .terminal
                .as_ref()
                .is_some_and(|value| Arc::ptr_eq(value, facts))
            || state.subject.is_some()
        {
            return Err(bitrouter_ai::tenancy::operation::ErrorCode::AuthorityUnavailable.into());
        }
        state.phase = Phase::Writing;
        Ok(())
    }
    pub(in crate::tenancy) fn queued(&self) -> Result<()> {
        let mut state = self.progress.lock().map_err(|_| {
            self.state.fail();
            Self::error()
        })?;
        if state.failed || state.phase != Phase::Waiting {
            return Err(Self::error());
        }
        state.phase = Phase::Queued;
        Ok(())
    }
    pub(in crate::tenancy) fn writing(
        &self,
        subject: &Arc<EvalSubject>,
    ) -> std::result::Result<(), crate::tenancy::error::Failure> {
        let mut state = self.progress.lock().map_err(|_| {
            crate::tenancy::error::Failure::from(
                bitrouter_ai::tenancy::operation::ErrorCode::AuthorityUnavailable,
            )
        })?;
        if state.failed
            || state.phase != Phase::Queued
            || !state
                .subject
                .as_ref()
                .is_some_and(|value| Arc::ptr_eq(value, subject))
        {
            return Err(bitrouter_ai::tenancy::operation::ErrorCode::AuthorityUnavailable.into());
        }
        state.phase = Phase::Writing;
        Ok(())
    }
    pub(in crate::tenancy) fn persisted(&self) {
        match self.progress.lock() {
            Ok(mut state) if state.phase == Phase::Writing => state.phase = Phase::Persisted,
            _ => self.state.fail(),
        }
    }
    pub(crate) fn confirmed(&self) -> bool {
        self.progress.lock().is_ok_and(|state| {
            !state.failed
                && state.phase == Phase::Persisted
                && matches!(
                    state.transaction,
                    Some(Disposition::Committed | Disposition::RolledBack)
                )
        })
    }
    pub(in crate::tenancy) fn drained(&self) -> Result<()> {
        let state = self.progress.lock().map_err(|_| {
            self.state.fail();
            Self::error()
        })?;
        if !state.failed
            && (matches!(
                state.phase,
                Phase::PreEntry | Phase::Skipped | Phase::SkippedBeforeDecision
            ) || state.phase == Phase::Persisted
                && matches!(
                    state.transaction,
                    Some(Disposition::Committed | Disposition::RolledBack)
                ))
        {
            return Ok(());
        }
        self.state.fail();
        Err(Self::error())
    }
}

/// Armed before fallible recorder work. Dropping it cannot acknowledge evidence.
pub(crate) struct Guard {
    observation: Arc<RequiredObservation>,
    finished: bool,
}
impl Guard {
    pub(crate) fn skip(mut self) -> Result<()> {
        {
            let mut state = self
                .observation
                .progress
                .lock()
                .map_err(|_| RequiredObservation::error())?;
            if state.failed
                || state.phase != Phase::Recording
                || !matches!(state.start, BeginProgress::Absent)
            {
                return Err(RequiredObservation::error());
            }
            state.phase = Phase::Skipped;
        }
        self.finished = true;
        Ok(())
    }
    pub(in crate::tenancy) fn skip_no_start(mut self) -> Result<()> {
        {
            let mut state = self
                .observation
                .progress
                .lock()
                .map_err(|_| RequiredObservation::error())?;
            if state.failed
                || state.phase != Phase::Recording
                || !matches!(state.start, BeginProgress::NoStart)
            {
                return Err(RequiredObservation::error());
            }
            state.phase = Phase::Skipped;
        }
        self.finished = true;
        Ok(())
    }
    pub(in crate::tenancy) fn prepare_trajectory(
        &self,
        start: &Arc<crate::tenancy::trajectory::StartProof>,
        facts: &Arc<crate::tenancy::trajectory::recorder::Facts>,
    ) -> Result<()> {
        super::wire::size(facts.as_ref(), BODY_BYTES).map_err(|_| RequiredObservation::error())?;
        let mut state = self
            .observation
            .progress
            .lock()
            .map_err(|_| RequiredObservation::error())?;
        if state.failed
            || state.phase != Phase::Recording
            || !matches!(&state.start,BeginProgress::Committed(value) if Arc::ptr_eq(value,start))
            || state.subject.is_some()
        {
            return Err(RequiredObservation::error());
        }
        state.terminal = Some(facts.clone());
        state.phase = Phase::Waiting;
        Ok(())
    }
    pub(crate) fn prepare(&self, subject: EvalSubject) -> Result<Arc<EvalSubject>> {
        super::wire::size(&subject, BODY_BYTES).map_err(|_| RequiredObservation::error())?;
        let execution = self
            .observation
            .execution
            .upgrade()
            .ok_or_else(RequiredObservation::error)?;
        if subject.scope != EvalScope::Request
            || subject.subject_id != execution.id().as_str()
            || subject.eval_id != format!("request:{}", execution.id())
        {
            return Err(RequiredObservation::error());
        }
        let subject = Arc::new(subject);
        {
            let mut state = self
                .observation
                .progress
                .lock()
                .map_err(|_| RequiredObservation::error())?;
            if state.failed
                || state.phase != Phase::Recording
                || !matches!(state.start, BeginProgress::Absent)
            {
                return Err(RequiredObservation::error());
            }
            state.subject = Some(subject.clone());
            state.phase = Phase::Waiting;
        }
        Ok(subject)
    }
    pub(crate) fn finish(mut self) -> Result<()> {
        if !self.observation.confirmed() {
            return Err(RequiredObservation::error());
        }
        self.finished = true;
        Ok(())
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        if !self.finished {
            self.observation.fail();
        }
    }
}

impl crate::tenancy::Service {
    pub(in crate::tenancy) async fn persist_observation(
        &self,
        execution: Arc<TenantExecution>,
        observation: Arc<RequiredObservation>,
        subject: Arc<EvalSubject>,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        #[cfg(test)]
        self.eval.producer_waiting.notify_one();
        let permit = tokio::time::timeout_at(deadline, self.commands.clone().reserve_owned())
            .await
            .map_err(|_| RequiredObservation::error())?
            .map_err(|_| RequiredObservation::error())?;
        let (reply, receive) = tokio::sync::oneshot::channel();
        {
            let _gate = self
                .accepting
                .lock()
                .map_err(|_| RequiredObservation::error())?;
            if !self.eval.producers_open.load(Ordering::Acquire)
                || !self.eval.healthy()
                || !self.live.load(Ordering::Acquire)
                || !execution.eval_origin(&self.cleanup)
                || tokio::time::Instant::now() >= deadline
            {
                return Err(RequiredObservation::error());
            }
            observation.queued()?;
            drop(permit.send(crate::tenancy::actor::Job::Settlement(Box::new(
                super::Settlement {
                    execution,
                    subject,
                    observation: observation.clone(),
                    deadline,
                    reply,
                },
            ))));
        }
        match tokio::time::timeout_at(
            deadline + crate::tenancy::transaction::CLEANUP_TIMEOUT,
            receive,
        )
        .await
        {
            Ok(Ok(Ok(()))) => Ok(()),
            _ if observation.confirmed() => Ok(()),
            _ => {
                observation.fail();
                Err(RequiredObservation::error())
            }
        }
    }
}
