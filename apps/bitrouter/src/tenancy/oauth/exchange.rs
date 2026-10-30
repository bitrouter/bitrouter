//! One inline refresh exchange, retained by H jobs and the existing receipt owner.
use super::storage::{Row, State, Work};
use crate::{
    gateway::providers::auth::credentials::OAuthToken,
    tenancy::{
        execution::TenantExecution,
        receipts::{
            auxiliary::{Outcome, Permit},
            owner::Owner,
        },
    },
};
use bitrouter_ai::tenancy::operation::ErrorCode;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Notify;

pub(in crate::tenancy) struct Exchange {
    pub execution: Arc<TenantExecution>,
    pub owner: Arc<Owner>,
    pub target: crate::gateway::pipeline::types::RoutingTarget,
    pub original: Arc<Row>,
    current: Mutex<Arc<Row>>,
    permit: Mutex<Option<Permit>>,
    pub start: std::sync::OnceLock<Arc<Row>>,
    pub replacement: std::sync::OnceLock<Arc<Row>>,
    pub release: std::sync::OnceLock<Arc<Row>>,
    pub armed: AtomicBool,
    busy: AtomicBool,
    abandoned: AtomicBool,
    release_queued: AtomicBool,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::tenancy) enum Operation {
    Start,
    Commit,
    Release,
}
pub(in crate::tenancy) struct Job {
    pub exchange: Arc<Exchange>,
    pub operation: Operation,
    pub progress: Arc<Progress>,
}
pub(in crate::tenancy) struct Progress {
    result: std::sync::OnceLock<Result<(), ErrorCode>>,
    changed: Notify,
}
impl Progress {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            result: std::sync::OnceLock::new(),
            changed: Notify::new(),
        })
    }
    pub fn finish(&self, result: Result<(), ErrorCode>) {
        if self.result.set(result).is_err() {
            tracing::error!("OAuth job progress published twice");
        }
        self.changed.notify_waiters();
    }
    async fn wait(&self) -> Result<(), ErrorCode> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self.result.get() {
                return *result;
            }
            notified.await;
        }
    }
}
impl Exchange {
    pub fn new(
        execution: Arc<TenantExecution>,
        target: crate::gateway::pipeline::types::RoutingTarget,
        row: Row,
    ) -> Result<Arc<Self>, ErrorCode> {
        let owner = execution
            .receipt_owner()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?
            .clone();
        let row = Arc::new(row);
        Ok(Arc::new(Self {
            execution,
            owner,
            target,
            original: row.clone(),
            current: Mutex::new(row),
            permit: Mutex::new(None),
            start: std::sync::OnceLock::new(),
            replacement: std::sync::OnceLock::new(),
            release: std::sync::OnceLock::new(),
            armed: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            abandoned: AtomicBool::new(false),
            release_queued: AtomicBool::new(false),
        }))
    }
    pub fn current(&self) -> Result<Arc<Row>, ErrorCode> {
        self.current
            .lock()
            .map(|row| row.clone())
            .map_err(|_| ErrorCode::AuthorityUnavailable)
    }
    pub fn published(&self, row: Arc<Row>) -> Result<(), ErrorCode> {
        *self
            .current
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)? = row;
        Ok(())
    }
    pub fn reserve(&self) -> Result<(), ErrorCode> {
        let metadata = &self.original.metadata;
        let permit = self.owner.auxiliary.reserve(
            metadata.reference.clone(),
            metadata.slot.clone(),
            metadata.generation,
        )?;
        let work = self
            .execution
            .oauth_work(permit.work(), metadata.generation)?;
        let mut started = metadata.clone();
        started.state = State::RefreshStarted(work);
        let crypto = self.execution.oauth_crypto()?;
        let token = self
            .original
            .token(&crypto)
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        let row = Row::new(&crypto, started, &token).map_err(|_| ErrorCode::StorageUnavailable)?;
        self.start
            .set(Arc::new(row))
            .map_err(|_| ErrorCode::Forbidden)?;
        *self
            .permit
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)? = Some(permit);
        Ok(())
    }
    pub fn work(&self) -> Result<&Work, ErrorCode> {
        match &self.start.get().ok_or(ErrorCode::Forbidden)?.metadata.state {
            State::RefreshStarted(work) => Ok(work),
            _ => Err(ErrorCode::Forbidden),
        }
    }
    pub fn arm(&self, digest: [u8; 32]) -> Result<(), ErrorCode> {
        let mut permit = self
            .permit
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        permit.as_mut().ok_or(ErrorCode::Forbidden)?.arm(digest)?;
        self.armed.store(true, Ordering::Release);
        Ok(())
    }
    pub fn status(&self, status: u16) -> Result<(), ErrorCode> {
        self.permit
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?
            .as_mut()
            .ok_or(ErrorCode::Forbidden)?
            .status(status)
    }
    pub fn finish(&self, outcome: Outcome) -> Result<(), ErrorCode> {
        let permit = self
            .permit
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?
            .take();
        if let Some(permit) = permit {
            permit.finish(outcome)?;
        }
        Ok(())
    }
    pub fn stage(&self, token: OAuthToken) -> Result<(), ErrorCode> {
        let mut metadata = self.current()?.metadata.clone();
        if !matches!(metadata.state, State::RefreshStarted(_)) {
            return Err(ErrorCode::Forbidden);
        }
        metadata.generation = metadata
            .generation
            .checked_add(1)
            .filter(|g| *g <= i64::MAX as u64)
            .ok_or(ErrorCode::CapacityExceeded)?;
        metadata.state = State::Ready;
        let row = Row::new(&self.execution.oauth_crypto()?, metadata, &token)
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        let row = Arc::new(row);
        self.owner.retain_oauth_replacement(row.clone())?;
        self.replacement
            .set(row)
            .map_err(|_| ErrorCode::Forbidden)?;
        Ok(())
    }
    pub fn release_candidate(&self) -> Result<Arc<Row>, ErrorCode> {
        if let Some(row) = self.release.get() {
            return Ok(row.clone());
        }
        let old = self.current()?;
        let mut metadata = old.metadata.clone();
        if self.armed.load(Ordering::Acquire) {
            metadata.state = State::Quarantined(self.work()?.clone());
        } else {
            metadata.state = State::Ready;
        }
        let crypto = self.execution.oauth_crypto()?;
        let token = old
            .token(&crypto)
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        let row = Arc::new(
            Row::new(&crypto, metadata, &token).map_err(|_| ErrorCode::StorageUnavailable)?,
        );
        self.release
            .set(row.clone())
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        Ok(row)
    }
    pub async fn submit(self: &Arc<Self>, operation: Operation) -> Result<(), ErrorCode> {
        let progress = self.enqueue(operation)?;
        // The actor owns the same progress cell before a waiter can disappear.
        // An elapsed waiter never cancels the job or permits a repeated refresh.
        let deadline = self
            .owner
            .cleanup_deadline()
            .min(tokio::time::Instant::now() + std::time::Duration::from_secs(30));
        match tokio::time::timeout_at(deadline, progress.wait()).await {
            Ok(result) => result,
            Err(_) => {
                self.owner.fail(ErrorCode::StorageUnavailable);
                Err(ErrorCode::StorageUnavailable)
            }
        }
    }
    fn enqueue(self: &Arc<Self>, operation: Operation) -> Result<Arc<Progress>, ErrorCode> {
        if operation == Operation::Release && self.release_queued.swap(true, Ordering::AcqRel) {
            return Err(ErrorCode::StorageUnavailable);
        }
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err(ErrorCode::CapacityExceeded);
        }
        let progress = Progress::new();
        let job = Job {
            exchange: self.clone(),
            operation,
            progress: progress.clone(),
        };
        if let Err(code) = self.execution.enqueue_oauth(job) {
            self.busy.store(false, Ordering::Release);
            return Err(code);
        }
        Ok(progress)
    }
    pub fn completed(self: &Arc<Self>) {
        self.busy.store(false, Ordering::Release);
        if self.abandoned.load(Ordering::Acquire) {
            self.abandoned();
        }
    }
    /// Drop queues exact cleanup on the existing reserved lane, without spawning.
    pub fn abandoned(self: &Arc<Self>) {
        self.abandoned.store(true, Ordering::Release);
        if self.start.get().is_some()
            && !self.busy.load(Ordering::Acquire)
            && !self.release_queued.load(Ordering::Acquire)
            && self
                .current()
                .is_ok_and(|row| matches!(row.metadata.state, State::RefreshStarted(_)))
            && self.enqueue(Operation::Release).is_err()
        {
            self.owner.fail(ErrorCode::StorageUnavailable);
        }
    }
}
