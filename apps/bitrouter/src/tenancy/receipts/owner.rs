//! One immutable terminal obligation. Waiters never own its durable transaction.
use super::{
    journal::Journal,
    store::{Candidate, Disposition},
};
use crate::tenancy::{actor::Job, admission::Lease, transaction::Proof};
use bitrouter_ai::tenancy::{
    execution::{ExecutionReceipt, Outcome, ReceiptState},
    identity::{CorrelationId, ExecutionId, InstallationId, PrincipalId, RevisionId, TenantId},
    operation::ErrorCode,
};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

pub(in crate::tenancy) struct Identity {
    pub installation: InstallationId,
    pub tenant: TenantId,
    pub principal: PrincipalId,
    pub execution: ExecutionId,
    pub correlation: CorrelationId,
    pub revision: RevisionId,
}

enum Phase {
    Open,
    Queued(Outcome),
    Prepared(Arc<Candidate>),
    Durable(Arc<Candidate>),
    Failed {
        code: ErrorCode,
        candidate: Option<Arc<Candidate>>,
        retryable: bool,
    },
}

pub(crate) struct Owner {
    identity: Identity,
    pub(in crate::tenancy) journal: Arc<Journal>,
    sender: tokio::sync::mpsc::Sender<Job>,
    phase: Mutex<Phase>,
    proposal: std::sync::OnceLock<Outcome>,
    changed: Notify,
    cancel_actions: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
    admission: Arc<crate::tenancy::admission::Admission>,
    lease: Mutex<Option<Arc<Lease>>>,
    pub(in crate::tenancy) origin: Arc<Proof>,
    pub(crate) deadline: tokio::time::Instant,
    cleanup: Mutex<tokio::time::Instant>,
    pub(crate) cancelled: tokio_util::sync::CancellationToken,
}

impl Owner {
    pub(in crate::tenancy) fn new(
        identity: Identity,
        journal: Arc<Journal>,
        sender: tokio::sync::mpsc::Sender<Job>,
        lease: Arc<Lease>,
        admission: Arc<crate::tenancy::admission::Admission>,
        origin: Arc<Proof>,
        deadline: tokio::time::Instant,
    ) -> Arc<Self> {
        Arc::new(Self {
            identity,
            journal,
            sender,
            phase: Mutex::new(Phase::Open),
            proposal: std::sync::OnceLock::new(),
            changed: Notify::new(),
            cancel_actions: Mutex::new(Vec::new()),
            lease: Mutex::new(Some(lease)),
            admission,
            origin,
            deadline,
            cleanup: Mutex::new(deadline + std::time::Duration::from_secs(30)),
            cancelled: tokio_util::sync::CancellationToken::new(),
        })
    }
    pub(crate) fn on_cancel(
        &self,
        action: Box<dyn FnOnce() + Send>,
    ) -> crate::gateway::error::Result<()> {
        let mut actions = self
            .cancel_actions
            .lock()
            .map_err(|_| crate::gateway::error::BitrouterError::UpstreamUnavailable)?;
        if self.cancelled.is_cancelled() {
            drop(actions);
            action();
            return Err(crate::gateway::error::BitrouterError::UpstreamUnavailable);
        }
        if actions.len() >= 6 {
            return Err(crate::gateway::error::BitrouterError::internal(
                "execution output slot limit",
            ));
        }
        actions.push(action);
        Ok(())
    }
    pub(in crate::tenancy) fn cleanup_deadline(&self) -> tokio::time::Instant {
        self.cleanup
            .lock()
            .map_or_else(|_| tokio::time::Instant::now(), |value| *value)
    }
    pub(crate) fn cancel(&self) {
        let actions = {
            let mut actions = self
                .cancel_actions
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            self.cancelled.cancel();
            std::mem::take(&mut *actions)
        };
        for action in actions {
            action();
        }
    }
    pub(crate) fn id(&self) -> &ExecutionId {
        &self.identity.execution
    }
    pub(in crate::tenancy) fn installation(&self) -> &InstallationId {
        &self.identity.installation
    }

    pub(crate) fn fail(&self, code: ErrorCode) {
        self.admission.fence_receipts();
        if let Ok(mut state) = self.phase.lock()
            && !matches!(*state, Phase::Durable(_))
        {
            let candidate = match &*state {
                Phase::Prepared(value)
                | Phase::Failed {
                    candidate: Some(value),
                    ..
                } => Some(value.clone()),
                _ => None,
            };
            *state = Phase::Failed {
                code,
                candidate,
                retryable: false,
            };
        }
        self.changed.notify_waiters();
    }
    pub(in crate::tenancy) fn storage_failed(&self, retryable: bool) {
        self.admission.fence_receipts();
        if let Ok(mut state) = self.phase.lock()
            && !matches!(*state, Phase::Durable(_))
        {
            let candidate = match &*state {
                Phase::Prepared(value)
                | Phase::Failed {
                    candidate: Some(value),
                    ..
                } => Some(value.clone()),
                _ => None,
            };
            *state = Phase::Failed {
                code: ErrorCode::StorageUnavailable,
                candidate,
                retryable,
            };
        }
        self.changed.notify_waiters();
    }
    /// Explicit cleanup retries immutable bytes only after confirmed rollback.
    pub(in crate::tenancy) fn reconcile(self: &Arc<Self>) -> Result<(), ErrorCode> {
        let queue = {
            let mut phase = self
                .phase
                .lock()
                .map_err(|_| ErrorCode::StorageUnavailable)?;
            match &*phase {
                Phase::Failed {
                    candidate,
                    retryable: true,
                    ..
                } => {
                    *phase = match candidate {
                        Some(candidate) => Phase::Prepared(candidate.clone()),
                        None => Phase::Queued(
                            *self.proposal.get().ok_or(ErrorCode::StorageUnavailable)?,
                        ),
                    };
                    *self
                        .cleanup
                        .lock()
                        .map_err(|_| ErrorCode::StorageUnavailable)? =
                        tokio::time::Instant::now() + std::time::Duration::from_secs(30);
                    true
                }
                _ => false,
            }
        };
        if queue && self.sender.try_send(Job::Receipt(self.clone())).is_err() {
            self.fail(ErrorCode::StorageUnavailable);
            return Err(ErrorCode::StorageUnavailable);
        }
        Ok(())
    }
    pub(crate) fn http_failure(&self) -> Option<crate::tenancy::error::Failure> {
        let mut failure = crate::tenancy::error::Failure::new(self.failure()?, None);
        failure.0.execution_id = Some(self.identity.execution.clone());
        failure.0.dispatch_state = self
            .journal
            .dispatch()
            .unwrap_or(bitrouter_ai::tenancy::operation::DispatchState::Unknown);
        Some(failure)
    }
    pub(crate) fn failure(&self) -> Option<ErrorCode> {
        match self.phase.lock() {
            Ok(state) => match *state {
                Phase::Failed { code, .. } => Some(code),
                _ => None,
            },
            Err(_) => Some(ErrorCode::StorageUnavailable),
        }
    }
    pub(in crate::tenancy) fn release_active(&self) -> Result<(), ErrorCode> {
        if !self.durable() {
            return Err(ErrorCode::StorageUnavailable);
        }
        let lease = self
            .lease
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?
            .take();
        drop(lease);
        Ok(())
    }
    pub(crate) fn durable(&self) -> bool {
        self.phase
            .lock()
            .is_ok_and(|state| matches!(*state, Phase::Durable(_)))
    }

    /// The first terminal proposal owns classification. Later disconnects join it
    /// and cannot rewrite provider facts after commit has begun.
    fn begin(self: &Arc<Self>, outcome: Outcome, join: bool) -> Result<(), ErrorCode> {
        let queue = {
            let mut state = self
                .phase
                .lock()
                .map_err(|_| ErrorCode::StorageUnavailable)?;
            match *state {
                Phase::Open => {
                    self.proposal
                        .set(outcome)
                        .map_err(|_| ErrorCode::StorageUnavailable)?;
                    *state = Phase::Queued(outcome);
                    true
                }
                Phase::Failed { code, .. } => return Err(code),
                _ => {
                    if !join && self.proposal.get() != Some(&outcome) {
                        self.admission.fence_receipts();
                        return Err(ErrorCode::StorageUnavailable);
                    }
                    false
                }
            }
        };
        if queue && self.sender.try_send(Job::Receipt(self.clone())).is_err() {
            self.fail(ErrorCode::StorageUnavailable);
            return Err(ErrorCode::StorageUnavailable);
        }
        Ok(())
    }
    pub(crate) async fn finish(self: &Arc<Self>, outcome: Outcome) -> Result<(), ErrorCode> {
        self.begin(outcome, false)?;
        self.wait_bounded().await
    }
    pub(crate) async fn finish_or_join(
        self: &Arc<Self>,
        fallback: Outcome,
    ) -> Result<(), ErrorCode> {
        self.begin(fallback, true)?;
        self.wait_bounded().await
    }
    async fn wait_bounded(&self) -> Result<(), ErrorCode> {
        match tokio::time::timeout_at(
            self.cleanup_deadline()
                .min(tokio::time::Instant::now() + std::time::Duration::from_secs(30)),
            self.wait(),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                self.fail(ErrorCode::StorageUnavailable);
                Err(ErrorCode::StorageUnavailable)
            }
        }
    }
    async fn wait(&self) -> Result<(), ErrorCode> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self
                    .phase
                    .lock()
                    .map_err(|_| ErrorCode::StorageUnavailable)?;
                match &*state {
                    Phase::Durable(candidate) => {
                        let _ = &candidate.receipt;
                        return Ok(());
                    }
                    Phase::Failed { code, .. } => return Err(*code),
                    _ => {}
                }
            }
            notified.await;
        }
    }
    pub(in crate::tenancy) async fn prepare(&self) -> anyhow::Result<Arc<Candidate>> {
        let proposed = {
            let state = self
                .phase
                .lock()
                .map_err(|_| anyhow::anyhow!("receipt owner unavailable"))?;
            match &*state {
                Phase::Queued(outcome) => *outcome,
                Phase::Prepared(candidate) | Phase::Durable(candidate) => {
                    return Ok(candidate.clone());
                }
                _ => anyhow::bail!("receipt owner not queued"),
            }
        };
        let frozen = self.journal.settled().await?;
        anyhow::ensure!(
            frozen.belongs_to_identity(&self.identity.installation, &self.identity.execution),
            "receipt owner mismatch"
        );
        let unknown = frozen
            .attempts
            .iter()
            .any(|attempt| attempt.outcome == Outcome::Unknown);
        let outcome = if proposed == Outcome::Completed
            && !frozen
                .attempts
                .iter()
                .any(|attempt| attempt.outcome == Outcome::Completed)
        {
            if unknown {
                Outcome::Unknown
            } else {
                Outcome::Failed
            }
        } else {
            proposed
        };
        let state = if unknown || outcome == Outcome::Unknown {
            ReceiptState::Unknown
        } else if frozen.usage.complete {
            ReceiptState::Committed
        } else {
            ReceiptState::Incomplete
        };
        let receipt = ExecutionReceipt {
            installation_id: self.identity.installation.clone(),
            tenant_id: self.identity.tenant.clone(),
            principal_id: self.identity.principal.clone(),
            execution_id: self.identity.execution.clone(),
            correlation_id: self.identity.correlation.clone(),
            execution_revision: self.identity.revision.clone(),
            state,
            outcome,
            attempts: frozen.attempts,
            usage: frozen.usage,
            recorded_at: chrono::Utc::now()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                .try_into()?,
        };
        let candidate = Arc::new(Candidate::new(
            receipt,
            Disposition {
                dispatch: frozen.dispatch,
                statuses: frozen.http_statuses,
            },
        )?);
        let mut phase = self
            .phase
            .lock()
            .map_err(|_| anyhow::anyhow!("receipt owner unavailable"))?;
        anyhow::ensure!(
            matches!(*phase, Phase::Queued(_)),
            "receipt terminal phase conflict"
        );
        *phase = Phase::Prepared(candidate.clone());
        Ok(candidate)
    }
    pub(in crate::tenancy) fn committed(
        &self,
        candidate: Arc<Candidate>,
        inserted: bool,
    ) -> Result<(), ErrorCode> {
        let mut phase = self
            .phase
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        if let Phase::Durable(_) = *phase {
            return Ok(());
        }
        let lease = self
            .lease
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?
            .as_ref()
            .cloned()
            .ok_or(ErrorCode::StorageUnavailable)?;
        if inserted {
            let unknown = if candidate.unknown() {
                Some((
                    candidate.receipt.execution_id.clone(),
                    chrono::DateTime::parse_from_rfc3339(candidate.receipt.recorded_at.as_str())
                        .map_err(|_| ErrorCode::StorageUnavailable)?
                        .with_timezone(&chrono::Utc),
                ))
            } else {
                None
            };
            lease.confirm_receipt(candidate.bytes(), unknown)?;
        } else {
            lease.confirm_receipt(0, None)?;
        }
        *phase = Phase::Durable(candidate);
        drop(phase);
        self.changed.notify_waiters();
        Ok(())
    }
}
