//! Receipt-owned shared publication state, separate from local lease handling.

use super::cleanup::{Authority, Finished, Observation};
use super::*;
use crate::gateway::pipeline::settlement::terminal;
use crate::gateway::shared::continuation::ProducerClosure;

pub(in crate::continuation) struct Registry {
    native: SharedNative,
    state: Mutex<State>,
}

struct State {
    accepting: bool,
    pending: HashMap<u64, Pending>,
    cursor: Option<u64>,
    closure: Option<Arc<ProducerClosure>>,
}

#[derive(Clone)]
enum Disposition {
    Reserved,
    Owned { lease: Option<String> },
    Existing(NativeIntent),
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Reason {
    Retiring,
    Uncertain,
    RollbackRequired,
}

#[derive(Clone)]
struct Pending {
    intent: NativeIntent,
    receipt: RequiredFinalizationReceipt,
    disposition: Disposition,
    reason: Option<Reason>,
    terminal: Option<terminal::Witness>,
}

impl Pending {
    fn matches(&self, other: &Self) -> bool {
        self.intent.identity == other.intent.identity
            && self.intent.generation == other.intent.generation
            && self.intent.producer == other.intent.producer
            && self.receipt.same_invocation(&other.receipt)
    }

    fn terminal_outcome(&self) -> Option<bool> {
        self.terminal.as_ref().and_then(terminal::Witness::outcome)
    }

    fn retire(&mut self, reason: Reason) {
        self.reason = Some(self.reason.map_or(reason, |existing| existing.max(reason)));
    }

    /// Adapt only confirmed, live forward ownership to the backend's fencing
    /// token. Neither a reservation nor a cleanup intent can enter this path.
    fn forward(&self) -> Result<PendingPublication> {
        ensure!(self.reason.is_none(), "shared publication is retiring");
        let Disposition::Owned { lease: Some(lease) } = &self.disposition else {
            anyhow::bail!("shared publication has no observed forward lease");
        };
        Ok(PendingPublication {
            continuation_identity: self.intent.identity.clone(),
            generation: self.intent.generation.clone(),
            instance_id: self.intent.producer.clone(),
            lease_until: lease.clone(),
            receipt: Some(self.receipt.clone()),
            phase: PendingPublicationPhase::Prepared,
        })
    }
}

impl Registry {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn get(&self, attempt: u64, receipt: Option<&RequiredFinalizationReceipt>) -> Option<Pending> {
        self.lock()
            .pending
            .get(&attempt)
            .filter(|pending| receipt.is_some_and(|r| pending.receipt.same_invocation(r)))
            .cloned()
    }

    fn update(&self, attempt: u64, expected: &Pending, change: impl FnOnce(&mut Pending)) -> bool {
        let mut state = self.lock();
        let Some(pending) = state
            .pending
            .get_mut(&attempt)
            .filter(|p| p.matches(expected))
        else {
            return false;
        };
        change(pending);
        true
    }

    fn clear(&self, attempt: u64, expected: &Pending) {
        let mut state = self.lock();
        if state
            .pending
            .get(&attempt)
            .is_some_and(|p| p.matches(expected))
        {
            state.pending.remove(&attempt);
        }
    }

    pub(in crate::continuation) fn markers(&self) -> owned::Markers {
        let state = self.lock();
        owned::Markers {
            total: state.pending.len(),
            reserved: state
                .pending
                .values()
                .filter(|p| matches!(p.disposition, Disposition::Reserved))
                .count(),
        }
    }

    pub(in crate::continuation) fn stop_accepting(&self) {
        let mut state = self.lock();
        state.accepting = false;
        for pending in state.pending.values_mut() {
            pending.retire(Reason::Retiring);
        }
    }

    /// Only the lifecycle owner calls this after every producer and worker has
    /// joined. A cancelled/unknown commit retains the gate and all intentions.
    pub(in crate::continuation) async fn close_producer(&self) -> Result<()> {
        if self.lock().closure.is_some() {
            return Ok(());
        }
        let closure = match self.native.authority.confirm_closed().await? {
            Some(closure) => closure,
            None => self.native.authority.close_binds().await?,
        };
        self.lock().closure = Some(Arc::new(closure));
        Ok(())
    }

    fn visible(&self, row: &continuation_entity::Model) -> bool {
        !self.lock().pending.values().any(|p| {
            !matches!(p.disposition, Disposition::Existing(_))
                && p.intent.owns(row)
                && p.terminal_outcome() != Some(true)
        })
    }
}

impl ContinuationRegistry {
    /// Construct an explicitly namespace-scoped registry over an initialized
    /// authority. No local epoch, key file or migration is consulted.
    pub(crate) fn new_shared(
        authority: SharedContinuationDb,
        retention_days: u32,
        prune_batch_size: usize,
    ) -> Result<Self> {
        ensure!(
            retention_days > 0,
            "shared continuation retention must be positive"
        );
        ensure!(
            (1..=10_000).contains(&prune_batch_size),
            "invalid shared continuation batch size"
        );
        let mut registry = Self::new(
            authority.database().clone(),
            ContinuationKeySource::fixed(authority.key()),
            retention_days,
            prune_batch_size,
        )?
        .with_owned_lifecycle()?;
        let inner =
            Arc::get_mut(&mut registry.inner).context("shared registry was already published")?;
        inner.shared = Some(Registry {
            native: SharedNative::new(authority),
            state: Mutex::new(State {
                accepting: true,
                pending: HashMap::new(),
                cursor: None,
                closure: None,
            }),
        });
        Ok(registry)
    }

    fn shared(&self) -> Result<&Registry> {
        self.inner
            .shared
            .as_ref()
            .context("shared continuation backend is absent")
    }

    pub(in crate::continuation) async fn shared_bind(
        &self,
        owner: &str,
        request: &str,
        binding: ContinuationBinding<'_>,
    ) -> Result<()> {
        let shared = self.shared()?;
        let intent = shared.native.intent(owner, request)?;
        let lock = self.pending_identity_lock(&intent.identity);
        let _guard = lock.lock().await;
        ensure!(
            shared.lock().accepting,
            "shared continuation producer is retiring"
        );
        let bound = shared
            .native
            .bind(
                owner,
                request,
                &binding,
                &intent.generation,
                PUBLICATION_ACTIVE,
                self.inner.retention,
            )
            .await?;
        ensure!(
            bound.outcome != BindOutcome::ExistingProvisional,
            "continuation has a pending publication"
        );
        Ok(())
    }

    pub(in crate::continuation) async fn shared_bind_pending(
        &self,
        owner: &str,
        request: &str,
        binding: ContinuationBinding<'_>,
        attempt: PendingBindAttempt,
    ) -> Result<bool> {
        let shared = self.shared()?;
        let pending = Pending {
            intent: shared.native.intent(owner, request)?,
            receipt: attempt
                .receipt
                .context("shared publication requires a finalization receipt")?,
            disposition: Disposition::Reserved,
            reason: None,
            terminal: None,
        };
        let id = attempt.delivery_attempt_id;
        let lock = self.pending_identity_lock(&pending.intent.identity);
        let _guard = lock.lock().await;
        {
            let mut state = shared.lock();
            ensure!(state.accepting, "shared continuation producer is retiring");
            ensure!(
                !state.pending.contains_key(&id),
                "duplicate shared delivery attempt"
            );
            ensure!(
                state.pending.len() < self.inner.pending_capacity,
                "shared continuation capacity exhausted"
            );
            state.pending.insert(id, pending.clone());
        }
        // Cancellation from this point leaves the original random intent. A
        // later absence observation cannot discard it before producer closure.
        let result = shared
            .native
            .bind(
                owner,
                request,
                &binding,
                &pending.intent.generation,
                PUBLICATION_PROVISIONAL,
                self.inner.retention,
            )
            .await;
        match result {
            Ok(bound) => match bound.outcome {
                BindOutcome::Inserted => {
                    shared.update(id, &pending, |p| {
                        p.disposition = Disposition::Owned {
                            lease: Some(bound.row.publication_lease_until),
                        }
                    });
                    self.start_reconciler();
                    Ok(true)
                }
                BindOutcome::ExistingActive => {
                    shared.update(id, &pending, |p| {
                        p.disposition = Disposition::Existing(NativeIntent {
                            identity: bound.row.continuation_identity,
                            generation: bound.row.publication_generation,
                            producer: bound.row.publication_instance_id,
                        })
                    });
                    self.start_reconciler();
                    Ok(false)
                }
                BindOutcome::ExistingProvisional => {
                    shared.clear(id, &pending);
                    anyhow::bail!("continuation has another pending publication");
                }
            },
            Err(error) => {
                if matches!(
                    error.downcast_ref::<Rejection>(),
                    Some(Rejection::PayloadTooLarge)
                ) {
                    shared.clear(id, &pending);
                } else {
                    shared.update(id, &pending, |p| p.retire(Reason::Uncertain));
                    self.start_reconciler();
                }
                Err(error)
            }
        }
    }

    /// Caller holds the process-local identity lock. The observed lease is
    /// invalidated before awaiting SQL, so cancellation cannot reuse stale CAS.
    async fn shared_renew(&self, id: u64, pending: &Pending) -> Result<PendingPublication> {
        let shared = self.shared()?;
        let mut current = shared
            .get(id, Some(&pending.receipt))
            .context("shared publication disappeared")?;
        ensure!(
            current.matches(pending) && current.reason.is_none(),
            "shared publication is not forward-owned"
        );
        if matches!(current.disposition, Disposition::Owned { lease: None }) {
            let row = match shared.native.refresh_forward(&current.intent).await {
                Ok(row) => row,
                Err(error) => {
                    shared.update(id, &current, |p| p.retire(Reason::Uncertain));
                    return Err(error);
                }
            };
            shared.update(id, &current, |p| {
                p.disposition = Disposition::Owned {
                    lease: Some(row.publication_lease_until),
                }
            });
            current = shared
                .get(id, Some(&pending.receipt))
                .context("shared publication disappeared")?;
        }
        let fence = current.forward()?;
        shared.update(id, &current, |p| {
            p.disposition = Disposition::Owned { lease: None }
        });
        match shared.native.renew(&fence).await {
            Ok(row) => {
                shared.update(id, &current, |p| {
                    p.disposition = Disposition::Owned {
                        lease: Some(row.publication_lease_until),
                    }
                });
                shared
                    .get(id, Some(&pending.receipt))
                    .context("shared publication disappeared")?
                    .forward()
            }
            Err(error) => {
                shared.update(id, &current, |p| p.retire(Reason::Uncertain));
                Err(error)
            }
        }
    }

    async fn shared_forward(
        &self,
        id: u64,
        pending: &Pending,
        next: ForwardTransition,
    ) -> Result<()> {
        let shared = self.shared()?;
        let lock = self.pending_identity_lock(&pending.intent.identity);
        let _guard = lock.lock().await;
        let current = shared
            .get(id, Some(&pending.receipt))
            .context("shared preparation is missing")?;
        ensure!(
            current.matches(pending) && current.reason.is_none(),
            "shared preparation is retiring"
        );
        if let Disposition::Existing(intent) = &current.disposition {
            return shared.native.verify_existing(intent).await;
        }
        let result = async {
            let fence = self.shared_renew(id, &current).await?;
            shared.native.transition(&fence, next).await?;
            Ok(())
        }
        .await;
        if result.is_err() {
            shared.update(id, &current, |p| p.retire(Reason::Uncertain));
        }
        result
    }

    pub(in crate::continuation) async fn shared_activate(
        &self,
        id: u64,
        receipt: Option<&RequiredFinalizationReceipt>,
        delivery: &RequiredDeliveryHandshake,
    ) -> Result<bool> {
        let shared = self.shared()?;
        let pending = shared
            .get(id, receipt)
            .context("eligible shared publication has no preparation")?;
        let witness = delivery
            .terminal_witness()
            .context("shared delivery requires retained terminal evidence")?;
        {
            let mut state = shared.lock();
            let current = state
                .pending
                .get_mut(&id)
                .filter(|p| p.matches(&pending))
                .context("shared publication disappeared")?;
            ensure!(
                current.terminal.is_none(),
                "shared publication commit already started"
            );
            current.terminal = Some(witness.clone());
        }
        self.shared_forward(id, &pending, ForwardTransition::Delivering)
            .await?;
        let delivered = delivery.wait_for_delivery_acknowledgement().await;
        if !delivered.as_ref().is_ok_and(|ok| *ok) {
            self.shared_rollback(id, receipt).await?;
            if delivery.complete_activation(Ok(())) {
                delivery.wait_for_terminal_commit().await;
            }
            return delivered.map_err(anyhow::Error::from);
        }
        if let Err(error) = self
            .shared_forward(id, &pending, ForwardTransition::Active)
            .await
        {
            delivery.reject(BitrouterError::internal(format!(
                "shared continuation activation: {error}"
            )));
            self.shared_rollback(id, receipt).await?;
            return Err(error);
        }
        if delivery.complete_activation(Ok(()))
            && delivery.wait_for_terminal_commit().await
            && witness.wait().await
        {
            shared.clear(id, &pending);
            return Ok(true);
        }
        self.shared_rollback(id, receipt).await?;
        Ok(false)
    }

    pub(in crate::continuation) async fn shared_rollback(
        &self,
        id: u64,
        receipt: Option<&RequiredFinalizationReceipt>,
    ) -> Result<()> {
        let shared = self.shared()?;
        let Some(pending) = shared.get(id, receipt) else {
            return Ok(());
        };
        ensure!(
            pending.terminal_outcome() != Some(true),
            "cannot roll back a successful terminal publication"
        );
        // Persist cancellation evidence before the first await, including a
        // cancelled waiter on the process-local identity lock.
        shared.update(id, &pending, |p| p.retire(Reason::RollbackRequired));
        let lock = self.pending_identity_lock(&pending.intent.identity);
        let _guard = lock.lock().await;
        self.shared_reconcile_one(id, &pending).await?;
        ensure!(
            shared.get(id, Some(&pending.receipt)).is_none(),
            "shared continuation rollback remains unresolved"
        );
        Ok(())
    }

    /// Caller holds the identity lock. Reserved absence needs a confirmed
    /// closure; normal owned cleanup never relies on an old observed lease.
    async fn shared_reconcile_one(&self, id: u64, expected: &Pending) -> Result<()> {
        let shared = self.shared()?;
        let Some(mut pending) = shared
            .get(id, Some(&expected.receipt))
            .filter(|p| p.matches(expected))
        else {
            return Ok(());
        };
        match pending.terminal_outcome() {
            Some(true) => {
                ensure!(
                    pending.reason != Some(Reason::RollbackRequired),
                    "contradictory shared terminal evidence"
                );
                shared.clear(id, &pending);
                return Ok(());
            }
            Some(false) => {
                shared.update(id, &pending, |p| p.retire(Reason::RollbackRequired));
                pending.retire(Reason::RollbackRequired);
            }
            None => {}
        }
        if matches!(pending.disposition, Disposition::Existing(_)) {
            if pending.reason.is_some() {
                shared.clear(id, &pending);
            }
            return Ok(());
        }
        let closure = shared.lock().closure.clone();
        let reserved = matches!(pending.disposition, Disposition::Reserved);
        if reserved && closure.is_none() {
            return Ok(());
        }
        if pending.reason.is_none() {
            self.shared_renew(id, &pending).await?;
            return Ok(());
        }
        // Reserved must use READ COMMITTED after the producer-close barrier,
        // even if cancellation is known. A pre-close snapshot cannot prove
        // that a delayed bind will not appear.
        let authority = if reserved {
            Authority::Closed {
                intent: &pending.intent,
                closure: closure.as_deref().context("producer closure missing")?,
            }
        } else if pending.reason == Some(Reason::RollbackRequired) {
            Authority::Cancelled(&pending.intent)
        } else if let Some(closure) = closure.as_deref() {
            Authority::Closed {
                intent: &pending.intent,
                closure,
            }
        } else {
            Authority::Abandoned(&pending.intent)
        };
        match shared.native.claim_cleanup(authority).await? {
            Observation::Missing | Observation::Foreign | Observation::Expired => {
                shared.clear(id, &pending)
            }
            Observation::Retained => {}
            Observation::Claimed(claim) => {
                if matches!(
                    shared.native.finish_cleanup(&claim).await?,
                    Finished::Removed | Finished::Missing
                ) {
                    shared.clear(id, &pending);
                }
            }
        }
        Ok(())
    }

    pub(in crate::continuation) async fn shared_reconcile_pending(&self) -> bool {
        let Ok(shared) = self.shared() else {
            return true;
        };
        let selected = {
            let state = shared.lock();
            let mut ids = state
                .pending
                .iter()
                .map(|(id, p)| (*id, p.clone()))
                .collect::<Vec<_>>();
            ids.sort_unstable_by_key(|(id, _)| *id);
            let start = state
                .cursor
                .map_or(0, |cursor| ids.partition_point(|(id, _)| *id <= cursor));
            let start = if start < ids.len() { start } else { 0 };
            ids.rotate_left(start);
            ids.truncate(RECONCILIATION_BATCH_SIZE);
            ids
        };
        let mut failed = false;
        for (id, pending) in selected {
            shared.lock().cursor = Some(id);
            let lock = self.pending_identity_lock(&pending.intent.identity);
            let _guard = lock.lock().await;
            if let Err(error) = self.shared_reconcile_one(id, &pending).await {
                failed = true;
                tracing::warn!(%error, "Shared continuation reconciliation retained an intent");
            }
        }
        failed
    }

    pub(in crate::continuation) async fn shared_reconcile(&self) -> bool {
        let mut failed = self.shared_reconcile_pending().await;
        let Ok(shared) = self.shared() else {
            return true;
        };
        match shared.native.recover_stale().await {
            Ok(report) => failed |= report.failed > 0,
            Err(error) => {
                failed = true;
                tracing::warn!(%error, "Shared stale continuation recovery failed");
            }
        }
        if let Err(error) = self.shared_prune().await {
            failed = true;
            tracing::warn!(%error, "Shared continuation retention failed");
        }
        failed
    }

    pub(in crate::continuation) async fn shared_resolve(
        &self,
        owner: &str,
        request: &str,
    ) -> Result<ContinuationResolution> {
        let shared = self.shared()?;
        let identity = shared.native.identity(owner, request)?;
        let lock = self.pending_identity_lock(&identity);
        let _guard = lock.lock().await;
        shared
            .native
            .resolve_visible(owner, request, |row| shared.visible(row))
            .await
    }

    pub(in crate::continuation) async fn shared_prune(&self) -> Result<u64> {
        let report = self
            .shared()?
            .native
            .maintain(usize::try_from(self.inner.prune_batch_size)?)
            .await?;
        ensure!(
            report.failed == 0,
            "shared continuation maintenance retained failed rows"
        );
        Ok(report.purged)
    }
}

#[cfg(test)]
#[path = "tests_shared_registry.rs"]
mod tests;
