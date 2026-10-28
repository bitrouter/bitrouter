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
    #[cfg(all(feature = "core", feature = "harness"))]
    learning: std::sync::OnceLock<Arc<crate::tenancy::learning::owner::Owner>>,
    identity: Identity,
    pub(in crate::tenancy) observation: crate::telemetry::receipt_latency::Timing,
    // Retain the exact encrypted replacement independently of adapter/future
    // lifetime, including a confirmed rollback whose bounded reconciliation
    // also fails. No token bytes or second lifecycle registry escape this owner.
    oauth_replacement: std::sync::OnceLock<Arc<crate::tenancy::oauth::storage::Row>>,
    oauth_acknowledged: std::sync::atomic::AtomicBool,
    pub(in crate::tenancy) auxiliary: Arc<super::auxiliary::Journal>,
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
    #[cfg(all(feature = "core", feature = "harness"))]
    pub(in crate::tenancy) fn recovery_seed(
        &self,
        service: &crate::tenancy::Service,
    ) -> anyhow::Result<Option<super::recovery::Seed>> {
        anyhow::ensure!(
            Arc::ptr_eq(&self.origin, &service.cleanup),
            "receipt origin mismatch"
        );
        service
            .startup
            .as_ref()
            .map(|startup| {
                startup
                    .stamp(&self.origin)
                    .map(|stamp| super::recovery::Seed::from_owner(&self.identity, stamp))
            })
            .transpose()
    }

    #[cfg(all(feature = "core", feature = "harness"))]
    pub(crate) fn storage_identity(&self) -> (&InstallationId, &TenantId, &PrincipalId) {
        (
            &self.identity.installation,
            &self.identity.tenant,
            &self.identity.principal,
        )
    }
    #[cfg(all(feature = "core", feature = "harness"))]
    pub(in crate::tenancy) fn attach_learning(
        &self,
        owner: Arc<crate::tenancy::learning::owner::Owner>,
    ) -> crate::gateway::error::Result<()> {
        if owner.execution != self.identity.execution || !owner.binding.origin(&self.origin) {
            return Err(crate::gateway::error::BitrouterError::internal(
                "assessment receipt mismatch",
            ));
        }
        self.learning.set(owner).map_err(|_| {
            crate::gateway::error::BitrouterError::internal("assessment receipt already bound")
        })
    }
    #[cfg(all(feature = "core", feature = "harness"))]
    pub(in crate::tenancy) async fn drain_learning(&self) -> Result<(), ErrorCode> {
        if let Some(owner) = self.learning.get() {
            owner.drain().await?;
        }
        Ok(())
    }

    #[cfg(all(feature = "core", feature = "harness"))]
    pub(in crate::tenancy) fn learning_seed(
        &self,
        binding: &crate::tenancy::learning::source::Binding,
        prepared: &crate::tenancy::learning::store::Prepared,
    ) -> anyhow::Result<Option<crate::tenancy::learning::recovery::Seed>> {
        anyhow::ensure!(binding.origin(&self.origin), "paid owner origin mismatch");
        binding
            .assessment_check()
            .map_err(|_| anyhow::anyhow!("paid owner authority rejected"))?;
        let Some(startup) = &binding.service().startup else {
            return Ok(None);
        };
        let origin = startup.stamp(&self.origin)?;
        Ok(Some(crate::tenancy::learning::recovery::Seed::from_owner(
            &self.identity,
            origin,
            binding,
            prepared,
        )?))
    }
    #[cfg(all(feature = "core", feature = "harness"))]
    pub(in crate::tenancy) fn learning_terminal(&self) -> Result<Arc<Candidate>, ErrorCode> {
        let phase = self
            .phase
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        match &*phase {
            Phase::Durable(candidate) => Ok(candidate.clone()),
            _ => Err(ErrorCode::AuthorityUnavailable),
        }
    }

    #[cfg(all(feature = "core", feature = "harness"))]
    pub(in crate::tenancy) fn stopped_learning(
        &self,
    ) -> Result<Option<Arc<crate::tenancy::learning::owner::Owner>>, ErrorCode> {
        // Missing aux evidence is not permission to discard an auxiliary effect.
        if !self.auxiliary.snapshot()?.is_empty() {
            return Err(ErrorCode::CapabilityMismatch);
        }
        Ok(self.learning.get().cloned())
    }

    pub(in crate::tenancy) fn new(
        identity: Identity,
        journal: Arc<Journal>,
        sender: tokio::sync::mpsc::Sender<Job>,
        lease: Arc<Lease>,
        admission: Arc<crate::tenancy::admission::Admission>,
        origin: Arc<Proof>,
        observed_deadline: (
            tokio::time::Instant,
            crate::telemetry::receipt_latency::Timing,
        ),
    ) -> Arc<Self> {
        let (deadline, observation) = observed_deadline;
        Arc::new(Self {
            #[cfg(all(feature = "core", feature = "harness"))]
            learning: std::sync::OnceLock::new(),
            identity,
            observation,
            oauth_replacement: std::sync::OnceLock::new(),
            oauth_acknowledged: std::sync::atomic::AtomicBool::new(false),
            auxiliary: super::auxiliary::Journal::new(),
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
    pub(in crate::tenancy) fn retain_oauth_replacement(
        &self,
        row: Arc<crate::tenancy::oauth::storage::Row>,
    ) -> Result<(), ErrorCode> {
        self.oauth_replacement
            .set(row)
            .map_err(|_| ErrorCode::Forbidden)
    }
    pub(in crate::tenancy) fn acknowledge_oauth_replacement(
        &self,
        row: &crate::tenancy::oauth::storage::Row,
    ) -> Result<(), ErrorCode> {
        if !self
            .oauth_replacement
            .get()
            .is_some_and(|retained| retained.same(row))
        {
            return Err(ErrorCode::Forbidden);
        }
        self.oauth_acknowledged
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
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
        if queue {
            self.observation
                .event(crate::telemetry::receipt_latency::Event::Retry);
        }
        if queue && self.sender.try_send(Job::Receipt(self.clone())).is_err() {
            self.observation
                .event(crate::telemetry::receipt_latency::Event::QueueFailure);
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
                    self.observation.ready();
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
        if queue {
            self.observation.queued();
        }
        if queue && self.sender.try_send(Job::Receipt(self.clone())).is_err() {
            self.observation
                .event(crate::telemetry::receipt_latency::Event::QueueFailure);
            self.fail(ErrorCode::StorageUnavailable);
            return Err(ErrorCode::StorageUnavailable);
        }
        Ok(())
    }
    pub(crate) async fn finish(self: &Arc<Self>, outcome: Outcome) -> Result<(), ErrorCode> {
        self.close_auxiliary().await?;
        self.begin(outcome, false)?;
        self.wait_bounded(true).await
    }
    /// Join durable settlement without turning a refused success ACK into a
    /// storage failure. This cannot acknowledge delivery: callers retain their
    /// error/cancellation, and uncertain storage still fails in every mode.
    pub(crate) async fn finish_or_join(
        self: &Arc<Self>,
        fallback: Outcome,
    ) -> Result<(), ErrorCode> {
        self.close_auxiliary().await?;
        self.begin(fallback, true)?;
        self.wait_bounded(false).await
    }
    async fn close_auxiliary(&self) -> Result<(), ErrorCode> {
        let close = async {
            tokio::select! {
                result = self.auxiliary.close() => result,
                code = self.wait_failure() => Err(code),
            }
        };
        let deadline = self
            .cleanup_deadline()
            .min(tokio::time::Instant::now() + std::time::Duration::from_secs(30));
        match tokio::time::timeout_at(deadline, close).await {
            Ok(result) => result,
            Err(_) => {
                self.fail(ErrorCode::StorageUnavailable);
                Err(ErrorCode::StorageUnavailable)
            }
        }
    }
    async fn wait_failure(&self) -> ErrorCode {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(code) = self.failure() {
                return code;
            }
            notified.await;
        }
    }
    async fn wait_bounded(&self, delivery: bool) -> Result<(), ErrorCode> {
        match tokio::time::timeout_at(
            self.cleanup_deadline()
                .min(tokio::time::Instant::now() + std::time::Duration::from_secs(30)),
            self.wait(delivery),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                self.observation
                    .event(crate::telemetry::receipt_latency::Event::CallerWaitFailure);
                self.fail(ErrorCode::StorageUnavailable);
                Err(ErrorCode::StorageUnavailable)
            }
        }
    }
    async fn wait(&self, delivery: bool) -> Result<(), ErrorCode> {
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
                        if delivery
                            && candidate.auxiliary_unclean()
                            && self.proposal.get() == Some(&Outcome::Completed)
                        {
                            return Err(ErrorCode::StorageUnavailable);
                        }
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
        anyhow::ensure!(
            self.oauth_replacement.get().is_none()
                || self
                    .oauth_acknowledged
                    .load(std::sync::atomic::Ordering::Acquire),
            "OAuth replacement remains in owner custody without commit acknowledgement"
        );
        let auxiliary = self
            .auxiliary
            .snapshot()
            .map_err(|_| anyhow::anyhow!("auxiliary producers remain live"))?;
        let auxiliary_unclean = auxiliary.unclean();
        let frozen = self.journal.settled().await?;
        anyhow::ensure!(
            frozen.belongs_to_identity(&self.identity.installation, &self.identity.execution),
            "receipt owner mismatch"
        );
        let unknown = frozen
            .attempts
            .iter()
            .any(|attempt| attempt.outcome == Outcome::Unknown);
        let outcome = if proposed == Outcome::Completed && auxiliary_unclean {
            Outcome::Failed
        } else if proposed == Outcome::Completed
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
        } else if auxiliary_unclean {
            ReceiptState::Incomplete
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
        let candidate = Arc::new(
            Candidate::new(
                receipt,
                Disposition {
                    dispatch: frozen.dispatch,
                    statuses: frozen.http_statuses,
                    auxiliary_digest: None,
                },
            )?
            .with_auxiliary(auxiliary)?,
        );
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
            let unknown = if candidate.unknown() || candidate.auxiliary_unknown() {
                Some((
                    candidate.receipt.execution_id.clone(),
                    chrono::DateTime::parse_from_rfc3339(candidate.receipt.recorded_at.as_str())
                        .map_err(|_| ErrorCode::StorageUnavailable)?
                        .with_timezone(&chrono::Utc),
                ))
            } else {
                None
            };
            lease.confirm_receipt(
                candidate
                    .bytes()
                    .checked_add(candidate.auxiliary_bytes())
                    .ok_or(ErrorCode::CapacityExceeded)?,
                unknown,
            )?;
        } else {
            lease.confirm_receipt(0, None)?;
        }
        *phase = Phase::Durable(candidate.clone());
        let publication = self.observation.publication(&candidate.receipt);
        drop(phase);
        publication.record();
        self.changed.notify_waiters();
        Ok(())
    }
}
