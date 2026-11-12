//! Per-Service scheduling probes on the actual owned optional SQL consumer.
//! No global hook, replacement receipt, authority setter or provider retry.
use super::*;
use bitrouter_ai::tenancy::identity::ExecutionId;
use std::sync::Mutex;
use tokio::sync::Notify;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tenancy) enum Point {
    BeforeInsert,
    AfterInsert,
    Driver,
    DdlReadOnly,
    PanicAfterInsert,
}
pub(in crate::tenancy) struct Gate {
    point: Point,
    pub(in crate::tenancy) entered: CancellationToken,
    pub(in crate::tenancy) release: CancellationToken,
}
/// Per-Service, one-use caller barrier for an expected or the next actual ACK.
/// The observed identity comes only from the original terminal_receipt caller.
#[cfg(unix)]
pub(in crate::tenancy) struct ReceiptAckGate {
    expected: Option<ExecutionId>,
    actual: Mutex<Option<ExecutionId>>,
    pub(in crate::tenancy) entered: CancellationToken,
    pub(in crate::tenancy) release: CancellationToken,
}
#[cfg(unix)]
impl ReceiptAckGate {
    pub(in crate::tenancy) fn observed_execution(&self) -> anyhow::Result<ExecutionId> {
        self.actual
            .lock()
            .map_err(|_| anyhow::anyhow!("receipt ACK fixture identity unavailable"))?
            .clone()
            .ok_or_else(|| anyhow::anyhow!("receipt ACK fixture has not observed a caller"))
    }
}
#[derive(Default)]
pub(in crate::tenancy) struct State {
    #[cfg(unix)]
    receipt_ack: Mutex<Option<Arc<ReceiptAckGate>>>,
    rotation_overlap: Mutex<Option<Arc<RotationOverlap>>>,
    gate: Mutex<Option<Arc<Gate>>>,
    last: Mutex<Option<(ExecutionId, bool)>>,
    changed: Notify,
    seen: Mutex<
        std::collections::VecDeque<
            bitrouter_ai::tenancy::provider_technical_facts::ProviderTechnicalFactsV1,
        >,
    >,
}
impl State {
    #[cfg(unix)]
    pub(in crate::tenancy) fn arm_rotation_overlap(
        &self,
        probe: Arc<RotationOverlap>,
    ) -> anyhow::Result<()> {
        let mut slot = self
            .rotation_overlap
            .lock()
            .map_err(|_| anyhow::anyhow!("rotation probe unavailable"))?;
        anyhow::ensure!(slot.is_none(), "rotation probe already armed");
        *slot = Some(probe);
        Ok(())
    }

    pub(super) async fn rotation_commit_gate(
        &self,
        db: &mut sea_orm::sqlx::SqliteConnection,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        let probe = self
            .rotation_overlap
            .lock()
            .map_err(|_| anyhow::anyhow!("rotation probe unavailable"))?
            .take();
        let Some(probe) = probe else { return Ok(()) };
        let execution = self
            .seen
            .lock()
            .map_err(|_| anyhow::anyhow!("rotation capture unavailable"))?
            .back()
            .map(|facts| facts.execution_id.clone())
            .ok_or_else(|| anyhow::anyhow!("rotation producer was not observed"))?;
        *probe
            .execution
            .lock()
            .map_err(|_| anyhow::anyhow!("rotation identity unavailable"))? = Some(execution);
        let cancel = cancel.clone();
        // Installed after original schema/absence checks, immediately before
        // the original INSERT. SQLx's safe hook never performs an alternate write.
        db.lock_handle().await?.set_commit_hook(move || {
            if probe.commit_entered.is_cancelled() {
                return false;
            }
            probe.commit_entered.cancel();
            // Stretch the native commit tail without changing the ORIGINAL
            // progress handler/token. Preempt cannot retroactively undo COMMIT.
            // No SQL or connection access is allowed inside a commit callback.
            while !probe.commit_release.is_cancelled() && Instant::now() < probe.deadline {
                std::thread::park_timeout(std::time::Duration::from_millis(1));
            }
            probe
                .preempt_seen
                .store(cancel.is_cancelled(), std::sync::atomic::Ordering::Release);
            let released = probe.commit_release.is_cancelled() && Instant::now() < probe.deadline;
            probe
                .commit_expired
                .store(!released, std::sync::atomic::Ordering::Release);
            released // SQLx true permits COMMIT; false explicitly rolls back.
        });
        Ok(())
    }

    #[cfg(all(unix, feature = "core", feature = "harness"))]
    pub(in crate::tenancy) fn hold_after_receipt_ack(
        &self,
        execution: &ExecutionId,
    ) -> anyhow::Result<Arc<ReceiptAckGate>> {
        let mut slot = self
            .receipt_ack
            .lock()
            .map_err(|_| anyhow::anyhow!("receipt ACK fixture gate unavailable"))?;
        anyhow::ensure!(slot.is_none(), "receipt ACK fixture gate already installed");
        let gate = Arc::new(ReceiptAckGate {
            expected: Some(execution.clone()),
            actual: Mutex::new(None),
            entered: CancellationToken::new(),
            release: CancellationToken::new(),
        });
        *slot = Some(gate.clone());
        Ok(gate)
    }
    #[cfg(unix)]
    pub(in crate::tenancy) fn hold_next_receipt_ack(&self) -> anyhow::Result<Arc<ReceiptAckGate>> {
        let mut slot = self
            .receipt_ack
            .lock()
            .map_err(|_| anyhow::anyhow!("receipt ACK fixture gate unavailable"))?;
        anyhow::ensure!(slot.is_none(), "receipt ACK fixture gate already installed");
        let gate = Arc::new(ReceiptAckGate {
            expected: None,
            actual: Mutex::new(None),
            entered: CancellationToken::new(),
            release: CancellationToken::new(),
        });
        *slot = Some(gate.clone());
        Ok(gate)
    }
    #[cfg(unix)]
    pub(in crate::tenancy) async fn after_receipt_ack(&self, execution: &ExecutionId) {
        let gate = match self.receipt_ack.lock() {
            Ok(mut slot)
                if slot.as_ref().is_some_and(|gate| {
                    gate.expected
                        .as_ref()
                        .is_none_or(|expected| expected == execution)
                }) =>
            {
                slot.take()
            }
            Ok(_) => None,
            Err(_) => {
                tracing::error!("receipt ACK fixture gate unavailable");
                None
            }
        };
        if let Some(gate) = gate {
            let recorded = match gate.actual.lock() {
                Ok(mut actual) => {
                    *actual = Some(execution.clone());
                    true
                }
                Err(_) => {
                    tracing::error!("receipt ACK fixture identity unavailable");
                    false
                }
            };
            if !recorded {
                return;
            }
            // No mutex/DB/actor guard crosses this await. The task that owns the
            // original response is held; the real optional writer remains free.
            gate.entered.cancel();
            gate.release.cancelled().await;
        }
    }

    #[cfg(unix)]
    pub(in crate::tenancy) fn hold(&self, point: Point) -> anyhow::Result<Arc<Gate>> {
        let gate = Arc::new(Gate {
            point,
            entered: CancellationToken::new(),
            release: CancellationToken::new(),
        });
        *self
            .gate
            .lock()
            .map_err(|_| anyhow::anyhow!("optional test gate unavailable"))? = Some(gate.clone());
        Ok(gate)
    }
    #[cfg(unix)]
    pub(in crate::tenancy) fn clear_gate(&self) -> anyhow::Result<()> {
        *self
            .gate
            .lock()
            .map_err(|_| anyhow::anyhow!("optional test gate unavailable"))? = None;
        Ok(())
    }
    pub(super) fn readonly_install(&self) -> bool {
        self.gate(Point::DdlReadOnly).is_some()
    }
    fn gate(&self, point: Point) -> Option<Arc<Gate>> {
        self.gate
            .lock()
            .ok()?
            .as_ref()
            .filter(|gate| gate.point == point)
            .cloned()
    }
    pub(super) async fn at(&self, point: Point, cancel: &CancellationToken) {
        if point == Point::AfterInsert
            && let Some(gate) = self.gate(Point::PanicAfterInsert)
        {
            gate.entered.cancel();
            std::panic::resume_unwind(Box::new("owned optional post-commit fault"));
        }
        if let Some(gate) = self.gate(point) {
            gate.entered.cancel();
            tokio::select! {biased;()=cancel.cancelled()=>{},()=gate.release.cancelled()=>{}}
        }
    }
    pub(super) fn captured(
        &self,
        facts: &bitrouter_ai::tenancy::provider_technical_facts::ProviderTechnicalFactsV1,
    ) {
        if let Ok(mut seen) = self.seen.lock() {
            if seen.len() == 2 {
                seen.pop_front();
            }
            seen.push_back(facts.clone());
        }
        self.changed.notify_waiters();
    }
    pub(in crate::tenancy) async fn observed(
        &self,
        id: &ExecutionId,
    ) -> anyhow::Result<bitrouter_ai::tenancy::provider_technical_facts::ProviderTechnicalFactsV1>
    {
        loop {
            let changed = self.changed.notified();
            if let Some(facts) = self
                .seen
                .lock()
                .map_err(|_| anyhow::anyhow!("optional capture observation unavailable"))?
                .iter()
                .find(|facts| &facts.execution_id == id)
                .cloned()
            {
                return Ok(facts);
            }
            changed.await;
        }
    }
    pub(super) fn completed(&self, id: ExecutionId, closed: bool) {
        if let Ok(mut last) = self.last.lock() {
            *last = Some((id, closed));
        }
        self.changed.notify_waiters();
    }
    #[cfg(unix)]
    pub(in crate::tenancy) async fn wait(&self, id: &ExecutionId) -> anyhow::Result<()> {
        loop {
            let changed = self.changed.notified();
            if let Some((observed, closed)) = self
                .last
                .lock()
                .map_err(|_| anyhow::anyhow!("optional test completion unavailable"))?
                .as_ref()
                && observed == id
            {
                anyhow::ensure!(*closed, "optional driver not positively closed");
                return Ok(());
            }
            changed.await;
        }
    }
    pub(super) async fn driver_gate(
        &self,
        db: &mut sea_orm::sqlx::SqliteConnection,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        if let Some(gate) = self.gate(Point::Driver) {
            let cancel = cancel.clone();
            db.lock_handle().await?.set_progress_handler(1, move || {
                // Existing audit tests use this actual SQLx worker mechanism.
                // Park only the disposable worker, not the runtime/control actor;
                // original work preempts it by cancelling the same owned token.
                gate.entered.cancel();
                while !gate.release.is_cancelled() && !cancel.is_cancelled() {
                    std::thread::park_timeout(std::time::Duration::from_millis(1));
                }
                !cancel.is_cancelled()
            });
        }
        Ok(())
    }
}

/// One Service's optional worker and one exact authority operation, test-only.
pub(in crate::tenancy) struct RotationOverlap {
    installation: bitrouter_ai::tenancy::identity::InstallationId,
    operation: bitrouter_ai::tenancy::identity::OperationId,
    pub(in crate::tenancy) deadline: Instant,
    pub(in crate::tenancy) commit_entered: CancellationToken,
    pub(in crate::tenancy) commit_release: CancellationToken,
    pub(in crate::tenancy) header_entered: CancellationToken,
    pub(in crate::tenancy) target_store_entered: CancellationToken,
    pub(in crate::tenancy) header_release: CancellationToken,
    pub(in crate::tenancy) preempt_seen: std::sync::atomic::AtomicBool,
    pub(in crate::tenancy) commit_expired: std::sync::atomic::AtomicBool,
    execution: Mutex<Option<ExecutionId>>,
    outcome: Mutex<Option<RotationWriteOutcome>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tenancy) struct RotationWriteOutcome {
    pub ok: bool,
    pub sqlite_code: Option<i32>,
}

impl RotationOverlap {
    pub(in crate::tenancy) fn new(
        installation: bitrouter_ai::tenancy::identity::InstallationId,
        operation: bitrouter_ai::tenancy::identity::OperationId,
        deadline: Instant,
    ) -> Arc<Self> {
        Arc::new(Self {
            installation,
            operation,
            deadline,
            commit_entered: CancellationToken::new(),
            commit_release: CancellationToken::new(),
            header_entered: CancellationToken::new(),
            target_store_entered: CancellationToken::new(),
            header_release: CancellationToken::new(),
            preempt_seen: std::sync::atomic::AtomicBool::new(false),
            commit_expired: std::sync::atomic::AtomicBool::new(false),
            execution: Mutex::new(None),
            outcome: Mutex::new(None),
        })
    }

    pub(in crate::tenancy) fn matches(
        &self,
        installation: &bitrouter_ai::tenancy::identity::InstallationId,
        operation: &bitrouter_ai::tenancy::identity::OperationId,
    ) -> bool {
        &self.installation == installation && &self.operation == operation
    }

    #[cfg(unix)]
    pub(in crate::tenancy) fn execution(&self) -> anyhow::Result<ExecutionId> {
        self.execution
            .lock()
            .map_err(|_| anyhow::anyhow!("rotation identity unavailable"))?
            .clone()
            .ok_or_else(|| anyhow::anyhow!("rotation worker identity missing"))
    }

    pub(in crate::tenancy) fn outcome(&self) -> anyhow::Result<RotationWriteOutcome> {
        self.outcome
            .lock()
            .map_err(|_| anyhow::anyhow!("rotation outcome unavailable"))?
            .ok_or_else(|| anyhow::anyhow!("rotation SQL outcome missing"))
    }

    pub(in crate::tenancy) fn record_result<T>(&self, result: &anyhow::Result<T>) {
        use sea_orm::sqlx::error::DatabaseError;
        let code = result.as_ref().err().and_then(|error| {
            error.chain().find_map(|cause| {
                cause
                    .downcast_ref::<sea_orm::sqlx::Error>()
                    .and_then(sea_orm::sqlx::Error::as_database_error)
                    .and_then(|error| {
                        error.try_downcast_ref::<sea_orm::sqlx::sqlite::SqliteError>()
                    })
                    .or_else(|| cause.downcast_ref::<sea_orm::sqlx::sqlite::SqliteError>())
                    .and_then(|error| error.code())
                    .and_then(|code| code.parse().ok())
            })
        });
        if let Ok(mut outcome) = self.outcome.lock() {
            *outcome = Some(RotationWriteOutcome {
                ok: result.is_ok(),
                sqlite_code: code,
            });
        }
    }
}

tokio::task_local! {
    // Scoped by the original Control/Proof, not a global installation registry.
    static AUTHORITY_ROTATION: Arc<RotationOverlap>;
}

pub(in crate::tenancy) async fn observe_rotation_write<F: std::future::Future>(
    probe: Option<Arc<RotationOverlap>>,
    work: F,
) -> F::Output {
    match probe {
        Some(probe) => AUTHORITY_ROTATION.scope(probe, work).await,
        None => work.await,
    }
}

pub(in crate::tenancy) async fn after_authority_header(
    installation: &bitrouter_ai::tenancy::identity::InstallationId,
    operation: &bitrouter_ai::tenancy::identity::OperationId,
) -> anyhow::Result<Option<Arc<RotationOverlap>>> {
    let probe = AUTHORITY_ROTATION
        .try_with(Arc::clone)
        .ok()
        .filter(|probe| probe.matches(installation, operation));
    if let Some(probe) = &probe {
        probe.header_entered.cancel();
        tokio::time::timeout_at(probe.deadline, probe.header_release.cancelled())
            .await
            .map_err(|_| anyhow::anyhow!("rotation snapshot gate deadline"))?;
    }
    Ok(probe)
}

#[tokio::test]
async fn rotation_probe_scopes_identity_and_never_extracts_codes_from_error_text()
-> anyhow::Result<()> {
    let installation = "probe-installation".try_into()?;
    let operation = "probe-operation".try_into()?;
    let probe = RotationOverlap::new(
        installation,
        operation,
        Instant::now() + crate::tenancy::MANAGEMENT_TIMEOUT,
    );
    observe_rotation_write(Some(probe.clone()), async {
        anyhow::ensure!(
            after_authority_header(&probe.installation, &"other-operation".try_into()?)
                .await?
                .is_none()
        );
        anyhow::ensure!(
            after_authority_header(&"other-installation".try_into()?, &probe.operation)
                .await?
                .is_none()
        );
        anyhow::ensure!(!probe.header_entered.is_cancelled());
        probe.header_release.cancel();
        anyhow::ensure!(
            after_authority_header(&probe.installation, &probe.operation)
                .await?
                .is_some()
        );
        anyhow::ensure!(probe.header_entered.is_cancelled());
        Ok::<(), anyhow::Error>(())
    })
    .await?;
    probe.record_result::<()>(&Err(anyhow::anyhow!("SQLITE_BUSY_SNAPSHOT 517")));
    anyhow::ensure!(
        probe.outcome()?
            == RotationWriteOutcome {
                ok: false,
                sqlite_code: None
            }
    );
    Ok(())
}

#[cfg(all(unix, feature = "core", feature = "harness"))]
#[tokio::test]
async fn receipt_ack_fixture_gate_ignores_other_identity_and_is_consumed_once() -> anyhow::Result<()>
{
    let state = Arc::new(State::default());
    let actual = "observed-execution".try_into()?;
    let gate = state.hold_after_receipt_ack(&actual)?;
    let release = gate.release.clone().drop_guard();
    tokio::time::timeout(
        crate::tenancy::MANAGEMENT_TIMEOUT,
        state.after_receipt_ack(&"other-execution".try_into()?),
    )
    .await?;
    anyhow::ensure!(!gate.entered.is_cancelled());
    let actor = state.clone();
    let id = actual.clone();
    let mut waiting = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        actor.after_receipt_ack(&id).await
    }));
    tokio::select! {biased;
        result=&mut waiting=>anyhow::bail!("actual gate did not hold caller: {result:?}"),
        ()=gate.entered.cancelled()=>{},
        ()=tokio::time::sleep(crate::tenancy::MANAGEMENT_TIMEOUT)=>anyhow::bail!("fixture ACK barrier did not observe actual caller"),
    }
    anyhow::ensure!(!waiting.is_finished());
    // The consumed gate does not hold a second response for the same ID.
    tokio::time::timeout(
        crate::tenancy::MANAGEMENT_TIMEOUT,
        state.after_receipt_ack(&actual),
    )
    .await?;
    gate.release.cancel();
    tokio::time::timeout(crate::tenancy::MANAGEMENT_TIMEOUT, waiting).await??;
    drop(release);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn next_receipt_ack_records_actual_identity_and_is_local_and_single_use() -> anyhow::Result<()>
{
    let state = Arc::new(State::default());
    let gate = state.hold_next_receipt_ack()?;
    let release = gate.release.clone().drop_guard();
    anyhow::ensure!(gate.observed_execution().is_err());
    anyhow::ensure!(state.hold_next_receipt_ack().is_err());
    let actual: ExecutionId = "actual-ACK-caller".try_into()?;
    let actor = state.clone();
    let id = actual.clone();
    let waiting = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        actor.after_receipt_ack(&id).await
    }));
    let checked: anyhow::Result<()> = async {
        tokio::time::timeout(crate::tenancy::MANAGEMENT_TIMEOUT, gate.entered.cancelled()).await?;
        anyhow::ensure!(!waiting.is_finished() && gate.observed_execution()? == actual);
        let other: ExecutionId = "other-ACK-caller".try_into()?;
        tokio::time::timeout(
            crate::tenancy::MANAGEMENT_TIMEOUT,
            state.after_receipt_ack(&other),
        )
        .await?;
        tokio::time::timeout(
            crate::tenancy::MANAGEMENT_TIMEOUT,
            State::default().after_receipt_ack(&actual),
        )
        .await?;
        anyhow::ensure!(gate.observed_execution()? == actual && !waiting.is_finished());
        Ok(())
    }
    .await;
    drop(release);
    let joined = waiting.await;
    checked?;
    joined?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn next_receipt_ack_error_release_and_caller_cancel_both_join() -> anyhow::Result<()> {
    for cancel in [false, true] {
        let state = Arc::new(State::default());
        let gate = state.hold_next_receipt_ack()?;
        let release = gate.release.clone().drop_guard();
        let actual: ExecutionId = "actual-error-ACK-caller".try_into()?;
        let actor = state.clone();
        let id = actual.clone();
        let waiting = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
            actor.after_receipt_ack(&id).await
        }));
        let checked: anyhow::Result<()> = async {
            tokio::time::timeout(crate::tenancy::MANAGEMENT_TIMEOUT, gate.entered.cancelled())
                .await?;
            anyhow::ensure!(gate.observed_execution()? == actual && !waiting.is_finished());
            // Inject a fixture Result error only after the actual gate held.
            anyhow::bail!("owned fixture error");
        }
        .await;
        let joined = if cancel {
            // Keep the gate unreleased until cancellation is actually joined;
            // otherwise a successful release could race the abort assertion.
            waiting.abort();
            let joined = waiting.await;
            drop(release);
            joined
        } else {
            drop(release);
            waiting.await
        };
        anyhow::ensure!(checked.is_err() && gate.entered.is_cancelled());
        if cancel {
            anyhow::ensure!(joined.is_err_and(|error| error.is_cancelled()));
        } else {
            joined?;
        }
        anyhow::ensure!(gate.observed_execution()? == actual);
    }
    Ok(())
}
