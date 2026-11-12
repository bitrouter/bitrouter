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
/// Per-Service, one-use caller barrier. The expected identity comes from the
/// actual retained Choice intent; this gate creates no receipt or authority.
#[cfg(all(unix, feature = "core", feature = "harness"))]
pub(in crate::tenancy) struct ReceiptAckGate {
    execution: ExecutionId,
    pub(in crate::tenancy) entered: CancellationToken,
    pub(in crate::tenancy) release: CancellationToken,
}
#[derive(Default)]
pub(in crate::tenancy) struct State {
    #[cfg(all(unix, feature = "core", feature = "harness"))]
    receipt_ack: Mutex<Option<Arc<ReceiptAckGate>>>,
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
            execution: execution.clone(),
            entered: CancellationToken::new(),
            release: CancellationToken::new(),
        });
        *slot = Some(gate.clone());
        Ok(gate)
    }
    #[cfg(all(unix, feature = "core", feature = "harness"))]
    pub(in crate::tenancy) async fn after_receipt_ack(&self, execution: &ExecutionId) {
        let gate = match self.receipt_ack.lock() {
            Ok(mut slot)
                if slot
                    .as_ref()
                    .is_some_and(|gate| &gate.execution == execution) =>
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
            // No mutex/DB/actor guard crosses this await. The task that owns the
            // original response is held; the real optional writer remains free.
            gate.entered.cancel();
            gate.release.cancelled().await;
        }
    }

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
