//! Explicit custody of authority transaction consumers.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::{Result, ensure};
use sea_orm::{DatabaseConnection, DatabaseTransaction, DbErr, TransactionTrait};
use tokio::sync::watch;
use tokio::time::Instant;

#[cfg(test)]
pub(in crate::tenancy) mod receipt_observation;
#[cfg(test)]
mod write_intent;

pub(super) const CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Disposition {
    NoTransaction,
    Pending,
    RolledBack,
    Committed,
    Ambiguous,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Evidence {
    pub disposition: Disposition,
    #[cfg(test)]
    pub fenced: bool,
    #[cfg(test)]
    write_intent_entered: usize,
    #[cfg(test)]
    write_intent_acquired: usize,
}

/// Borrowed, read-only preparation connection. Each awaited statement keeps its
/// actual owner; cancellation prevents the next SQL statement after it settles.
pub(super) struct ReadConnection<'a> {
    transaction: &'a DatabaseTransaction,
    control: &'a Control,
}

impl<'a> ReadConnection<'a> {
    pub fn new(transaction: &'a DatabaseTransaction, control: &'a Control) -> Self {
        Self {
            transaction,
            control,
        }
    }
    fn check(&self) -> std::result::Result<(), DbErr> {
        self.control
            .check_read()
            .map_err(|_| DbErr::Custom("preparation stopped".into()))
    }
}

#[async_trait::async_trait]
impl sea_orm::ConnectionTrait for ReadConnection<'_> {
    fn get_database_backend(&self) -> sea_orm::DatabaseBackend {
        self.transaction.get_database_backend()
    }
    async fn execute(
        &self,
        _statement: sea_orm::Statement,
    ) -> std::result::Result<sea_orm::ExecResult, DbErr> {
        Err(DbErr::Custom(
            "write attempted during read preparation".into(),
        ))
    }
    async fn execute_unprepared(
        &self,
        _sql: &str,
    ) -> std::result::Result<sea_orm::ExecResult, DbErr> {
        Err(DbErr::Custom(
            "write attempted during read preparation".into(),
        ))
    }
    async fn query_one(
        &self,
        statement: sea_orm::Statement,
    ) -> std::result::Result<Option<sea_orm::QueryResult>, DbErr> {
        self.check()?;
        let result = self.transaction.query_one(statement).await;
        if result.is_err() {
            self.control.storage_failed();
        }
        let result = result?;
        self.check()?;
        Ok(result)
    }
    async fn query_all(
        &self,
        statement: sea_orm::Statement,
    ) -> std::result::Result<Vec<sea_orm::QueryResult>, DbErr> {
        self.check()?;
        let result = self.transaction.query_all(statement).await;
        if result.is_err() {
            self.control.storage_failed();
        }
        let result = result?;
        self.check()?;
        Ok(result)
    }
}

/// Joining the worker is insufficient without its explicit transaction outcome.
pub(super) struct Proof {
    #[cfg(test)]
    rotation_overlap:
        std::sync::Mutex<Option<Arc<super::actor::provider_technical::tests::RotationOverlap>>>,
    state: watch::Sender<Evidence>,
    live: Arc<AtomicBool>,
    #[cfg(test)]
    gate: std::sync::Mutex<Option<Arc<Gate>>>,
    #[cfg(test)]
    pub read_stopped: tokio::sync::Notify,
    #[cfg(test)]
    begin_calls: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    acknowledged_rollbacks: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    receipt_observation: std::sync::Mutex<Option<receipt_observation::Trace>>,
}

impl Proof {
    pub fn new(live: Arc<AtomicBool>) -> Arc<Self> {
        Arc::new(Self {
            #[cfg(test)]
            rotation_overlap: std::sync::Mutex::new(None),
            live,
            state: watch::channel(Evidence {
                disposition: Disposition::NoTransaction,
                #[cfg(test)]
                fenced: false,
                #[cfg(test)]
                write_intent_entered: 0,
                #[cfg(test)]
                write_intent_acquired: 0,
            })
            .0,
            #[cfg(test)]
            gate: std::sync::Mutex::new(None),
            #[cfg(test)]
            read_stopped: tokio::sync::Notify::new(),
            #[cfg(test)]
            begin_calls: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            acknowledged_rollbacks: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            receipt_observation: std::sync::Mutex::new(None),
        })
    }

    fn record(&self, disposition: Disposition) {
        if disposition == Disposition::Ambiguous {
            self.fenced();
        }
        self.state.send_modify(|state| {
            if state.disposition != Disposition::Ambiguous {
                state.disposition = disposition;
            }
        });
    }

    pub fn disposition(&self) -> Disposition {
        self.state.borrow().disposition
    }

    pub fn fenced(&self) {
        self.live.store(false, Ordering::Release);
        #[cfg(test)]
        self.state.send_modify(|state| state.fenced = true);
    }

    pub fn quarantine(&self) {
        self.record(Disposition::Ambiguous);
    }

    pub async fn quarantined(&self) {
        let mut state = self.state.subscribe();
        // This Proof owns the Sender for the whole wait. Unexpected closure is
        // itself unprovable ownership, never a successful cleanup notification.
        if state
            .wait_for(|value| value.disposition == Disposition::Ambiguous)
            .await
            .is_err()
        {
            tracing::error!("tenant cleanup watch closed; authority fenced");
            self.quarantine();
        }
    }

    pub fn ensure_clean(&self) -> Result<()> {
        ensure!(
            matches!(
                self.disposition(),
                Disposition::NoTransaction | Disposition::RolledBack | Disposition::Committed
            ),
            "tenant transaction cleanup is not confirmed"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Preparing,
    Beginning,
    Active,
    Committing,
    RollingBack,
    Settled,
}

#[derive(Clone, Copy)]
pub(super) struct Progress {
    pub phase: Phase,
    pub cleanup_deadline: Option<Instant>,
}

pub(super) struct Control {
    proof: Arc<Proof>,
    #[cfg(all(test, feature = "core"))]
    test_target: Option<JobTarget>,
    progress: watch::Sender<Progress>,
    stopped: AtomicBool,
    preparing_read: AtomicBool,
    storage: Option<super::storage::Budget>,
    write_charge: std::sync::Mutex<Option<super::storage::Charge>>,
}

#[derive(Debug)]
pub(super) struct ReadExpired;

impl std::fmt::Display for ReadExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("credential read deadline elapsed")
    }
}

impl std::error::Error for ReadExpired {}

impl Control {
    #[cfg(test)]
    pub(in crate::tenancy) fn rotation_overlap_for_test(
        &self,
    ) -> Option<Arc<super::actor::provider_technical::tests::RotationOverlap>> {
        self.proof.rotation_overlap.lock().ok()?.clone()
    }

    pub fn new(proof: Arc<Proof>) -> Self {
        proof.record(Disposition::NoTransaction);
        Self {
            proof,
            #[cfg(all(test, feature = "core"))]
            test_target: None,
            progress: watch::channel(Progress {
                phase: Phase::Preparing,
                cleanup_deadline: None,
            })
            .0,
            stopped: AtomicBool::new(false),
            preparing_read: AtomicBool::new(false),
            storage: None,
            write_charge: std::sync::Mutex::new(None),
        }
    }

    pub fn with_storage(mut self, storage: super::storage::Budget) -> Self {
        self.storage = Some(storage);
        self
    }

    #[cfg(all(test, feature = "core"))]
    pub fn with_test_target(mut self, target: Option<JobTarget>) -> Self {
        self.test_target = target;
        self
    }

    /// Called before a mutating BEGIN, after any read-only preparation settles.
    /// The existing H owner retains this charge through timeout and cancellation.
    pub fn reserve_write(
        &self,
        encoded_bytes: usize,
        maintenance: bool,
    ) -> std::result::Result<(), bitrouter_ai::tenancy::operation::ErrorCode> {
        use bitrouter_ai::tenancy::operation::ErrorCode;
        let Some(storage) = &self.storage else {
            return Ok(());
        };
        let mut charge = self
            .write_charge
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if charge.is_some() {
            self.proof.quarantine();
            return Err(ErrorCode::AuthorityUnavailable);
        }
        let mut reserved = storage.reserve(encoded_bytes, maintenance)?;
        if self.phase() == Phase::Active {
            // Some startup transactions validate before their optional write.
            // No mutation may precede this reservation.
            reserved.start();
        }
        *charge = Some(reserved);
        Ok(())
    }

    pub fn storage_failed(&self) {
        if let Some(storage) = &self.storage {
            storage.fence();
        }
    }

    pub fn progress(&self) -> watch::Receiver<Progress> {
        self.progress.subscribe()
    }

    pub fn phase(&self) -> Phase {
        self.progress.borrow().phase
    }

    /// Management preparation owns read transaction cleanup but no durable write
    /// intent. Its watchdog may stop reads while retaining actual SQL custody.
    pub fn prepare_read(&self) {
        self.preparing_read.store(true, Ordering::Release);
    }

    pub fn preparing_read(&self) -> bool {
        self.preparing_read.load(Ordering::Acquire)
    }

    pub fn finish_preparation(&self) -> Result<()> {
        self.check_read()?;
        self.preparing_read.store(false, Ordering::Release);
        Ok(())
    }

    pub fn stop_read(&self) {
        self.stopped.store(true, Ordering::Release);
        self.start_cleanup();
        #[cfg(test)]
        self.proof.read_stopped.notify_one();
    }

    pub fn check_read(&self) -> Result<()> {
        if self.stopped.load(Ordering::Acquire) {
            Err(ReadExpired.into())
        } else {
            Ok(())
        }
    }

    fn start_cleanup(&self) {
        self.progress.send_modify(|state| {
            state
                .cleanup_deadline
                .get_or_insert_with(|| Instant::now() + CLEANUP_TIMEOUT);
        });
    }

    async fn enter(&self, phase: Phase) {
        self.progress.send_modify(|state| state.phase = phase);
        #[cfg(test)]
        {
            let gate = match self.proof.gate.lock() {
                Ok(gate) => gate.clone(),
                Err(error) => error.into_inner().clone(),
            };
            if let Some(gate) = gate.filter(|gate| {
                let matches = gate.phase == phase;
                #[cfg(feature = "core")]
                let matches = matches
                    && gate
                        .target
                        .is_none_or(|target| self.test_target == Some(target));
                matches
            }) {
                gate.entered.notify_one();
                gate.release.notified().await;
                if gate.unwind.load(Ordering::Acquire) {
                    // Test-only fault injection inside the caught work future.
                    // No panic hook or diagnostic payload is involved.
                    std::panic::resume_unwind(Box::new(()));
                }
            }
        }
    }

    fn complete(&self, disposition: Disposition) {
        if matches!(
            disposition,
            Disposition::NoTransaction | Disposition::RolledBack | Disposition::Committed
        ) {
            match self.write_charge.lock() {
                Ok(mut charge) => {
                    if let Some(charge) = charge.take()
                        && charge.complete().is_err()
                    {
                        self.proof.quarantine();
                    }
                }
                Err(_) => self.proof.quarantine(),
            }
        }
        self.proof.record(disposition);
        self.progress.send_modify(|state| {
            state.phase = Phase::Settled;
            state.cleanup_deadline = None;
        });
    }

    pub async fn begin(&self, db: &DatabaseConnection) -> Result<DatabaseTransaction> {
        self.begin_configured(db, None, false).await
    }

    /// Only a terminal receipt may reconcile a typed failure before BEGIN.
    /// Existing storage/probe fences remain monotonic; SQL ambiguity still fences.
    pub async fn begin_receipt(&self, db: &DatabaseConnection) -> Result<DatabaseTransaction> {
        self.begin_configured(db, None, true).await
    }

    pub async fn begin_repeatable_read(
        &self,
        db: &DatabaseConnection,
    ) -> Result<DatabaseTransaction> {
        use sea_orm::ConnectionTrait;
        let isolation = (db.get_database_backend() != sea_orm::DatabaseBackend::Sqlite)
            .then_some(sea_orm::IsolationLevel::RepeatableRead);
        self.begin_configured(db, isolation, false).await
    }

    /// Required tenancy writers acquire SQLite intent before any application read.
    pub async fn begin_write(&self, db: &DatabaseConnection) -> Result<DatabaseTransaction> {
        self.with_write_intent(self.begin(db).await?).await
    }

    /// Retain the original repeatable-read choice on non-SQLite backends.
    pub async fn begin_write_repeatable_read(
        &self,
        db: &DatabaseConnection,
    ) -> Result<DatabaseTransaction> {
        self.with_write_intent(self.begin_repeatable_read(db).await?)
            .await
    }

    /// Only BEGIN itself can use the receipt's typed pre-acquisition exception.
    pub async fn begin_write_receipt(
        &self,
        db: &DatabaseConnection,
    ) -> Result<DatabaseTransaction> {
        self.with_write_intent(self.begin_receipt(db).await?).await
    }

    async fn with_write_intent(
        &self,
        transaction: DatabaseTransaction,
    ) -> Result<DatabaseTransaction> {
        use sea_orm::ConnectionTrait;
        if transaction.get_database_backend() != sea_orm::DatabaseBackend::Sqlite {
            return Ok(transaction);
        }
        // BEGIN already succeeded: every failure below owns this SAME transaction.
        // The native tenancy schema guarantees the ordinary main target. Do not
        // inspect/fallback/create a schema, or establish a read snapshot first.
        let result: Result<()> = async {
            self.check_read()?;
            #[cfg(test)]
            self.proof.state.send_modify(|state| {
                state.write_intent_entered = state.write_intent_entered.saturating_add(1);
            });
            let applied = transaction
                .execute_unprepared(
                    "UPDATE main.tenancy_installation SET singleton=singleton WHERE 0",
                )
                .await?;
            ensure!(
                applied.rows_affected() == 0,
                "write intent changed installation rows"
            );
            #[cfg(test)]
            self.proof.state.send_modify(|state| {
                state.write_intent_acquired = state.write_intent_acquired.saturating_add(1);
            });
            self.check_read()?;
            Ok(())
        }
        .await;
        match result {
            Ok(()) => Ok(transaction),
            Err(error) => {
                if error.downcast_ref::<ReadExpired>().is_none() {
                    self.storage_failed();
                }
                // No ConnectionAcquire/NoTransaction reclassification after BEGIN.
                // Consuming rollback must ACK; on Err its original Proof remains
                // Ambiguous. Keep the primary typed cause, with cleanup context.
                match self.rollback(transaction).await {
                    Ok(()) => Err(error),
                    Err(cleanup) => {
                        Err(error
                            .context(format!("write-intent rollback is unconfirmed: {cleanup}")))
                    }
                }
            }
        }
    }

    async fn begin_configured(
        &self,
        db: &DatabaseConnection,
        isolation: Option<sea_orm::IsolationLevel>,
        receipt: bool,
    ) -> Result<DatabaseTransaction> {
        match self.write_charge.lock() {
            Ok(mut charge) => {
                if let Some(charge) = charge.as_mut() {
                    charge.start();
                }
            }
            Err(_) => {
                self.proof.quarantine();
                anyhow::bail!("storage estimate ownership unavailable");
            }
        }
        self.proof.record(Disposition::Pending);
        self.enter(Phase::Beginning).await;
        // SeaORM constructs an open transaction before configuring isolation.
        // Keep that entire await under H custody, including configuration errors.
        // https://docs.rs/sea-orm/1.1.20/src/sea_orm/database/transaction.rs.html
        #[cfg(test)]
        self.proof.begin_calls.fetch_add(1, Ordering::SeqCst);
        let beginning = match isolation {
            Some(isolation) => db.begin_with_config(Some(isolation), None).await,
            None => db.begin().await,
        };
        match beginning {
            Ok(transaction) => {
                self.enter(Phase::Active).await;
                Ok(transaction)
            }
            Err(error) => {
                let no_transaction = matches!(&error, DbErr::ConnectionAcquire(_));
                if !receipt || !no_transaction {
                    self.storage_failed();
                }
                // The typed pool-acquire failure precedes transaction construction.
                // Other begin failures may drop an already-open transaction.
                // https://docs.rs/sea-orm/1.1.20/src/sea_orm/driver/sqlx_sqlite.rs.html
                self.complete(if no_transaction {
                    Disposition::NoTransaction
                } else {
                    Disposition::Ambiguous
                });
                Err(error.into())
            }
        }
    }

    pub async fn commit(&self, transaction: DatabaseTransaction) -> Result<()> {
        self.commit_observed(transaction, None).await
    }

    pub(crate) async fn commit_observed(
        &self,
        transaction: DatabaseTransaction,
        observation: Option<&crate::telemetry::receipt_latency::Timing>,
    ) -> Result<()> {
        self.enter(Phase::Committing).await;
        // The phase gate precedes SQL. Time only the original consuming await.
        let span = observation
            .map(|o| o.span(crate::telemetry::receipt_latency::Phase::SqlCommit))
            .unwrap_or_default();
        let result = transaction.commit().await;
        if result.is_err() {
            self.storage_failed();
        }
        let observation = span.stop(if result.is_ok() {
            crate::telemetry::receipt_latency::ResultKind::Acknowledged
        } else {
            crate::telemetry::receipt_latency::ResultKind::Unconfirmed
        });
        // Both consuming methods clear `open` only on Ok. On Err, Drop merely
        // queues rollback; its completion is not acknowledged to this caller.
        // https://docs.rs/sea-orm/1.1.20/src/sea_orm/database/transaction.rs.html
        self.complete(if result.is_ok() {
            Disposition::Committed
        } else {
            Disposition::Ambiguous
        });
        observation.record();
        result.map_err(Into::into)
    }

    pub async fn rollback(&self, transaction: DatabaseTransaction) -> Result<()> {
        self.start_cleanup();
        self.enter(Phase::RollingBack).await;
        let result = transaction.rollback().await;
        if result.is_err() {
            self.storage_failed();
        }
        #[cfg(test)]
        if result.is_ok() {
            self.proof
                .acknowledged_rollbacks
                .fetch_add(1, Ordering::SeqCst);
        }
        self.complete(if result.is_ok() {
            Disposition::RolledBack
        } else {
            Disposition::Ambiguous
        });
        result.map_err(Into::into)
    }

    pub fn conclude(&self) {
        if self.proof.disposition() == Disposition::Pending {
            self.proof.quarantine();
        }
    }
}

#[cfg(all(test, feature = "core"))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum JobTarget {
    Eval,
    Settlement,
    TrajectoryBegin,
    TrajectoryTerminal,
    #[cfg(feature = "harness")]
    NativeLoad,
}

#[cfg(test)]
pub(super) struct Gate {
    phase: Phase,
    #[cfg(feature = "core")]
    target: Option<JobTarget>,
    unwind: AtomicBool,
    pub entered: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}

#[cfg(test)]
impl Proof {
    /// Monotonic actual Control boundaries; entered does not assert a SQL lock.
    pub fn write_intent_counts_for_test(&self) -> (usize, usize) {
        let state = self.state.borrow();
        (state.write_intent_entered, state.write_intent_acquired)
    }

    pub async fn wait_write_intent_entered_for_test(&self, after: usize) -> Result<()> {
        let mut state = self.state.subscribe();
        state
            .wait_for(|state| state.write_intent_entered > after)
            .await
            .map_err(|_| anyhow::anyhow!("write intent entry observation closed"))?;
        Ok(())
    }

    pub async fn wait_write_intent_acquired_for_test(&self, after: usize) -> Result<()> {
        let mut state = self.state.subscribe();
        state
            .wait_for(|state| state.write_intent_acquired > after)
            .await
            .map_err(|_| anyhow::anyhow!("write intent acquisition observation closed"))?;
        Ok(())
    }

    #[cfg(unix)]
    pub(in crate::tenancy) fn install_rotation_overlap_for_test(
        &self,
        probe: Arc<super::actor::provider_technical::tests::RotationOverlap>,
    ) -> Result<()> {
        let mut slot = self
            .rotation_overlap
            .lock()
            .map_err(|_| anyhow::anyhow!("rotation proof probe unavailable"))?;
        ensure!(slot.is_none(), "rotation proof probe already installed");
        *slot = Some(probe);
        Ok(())
    }

    /// Monotonic observations survive unrelated later jobs on the same writer.
    pub fn sql_counts_for_test(&self) -> (usize, usize) {
        (
            self.begin_calls.load(Ordering::SeqCst),
            self.acknowledged_rollbacks.load(Ordering::SeqCst),
        )
    }

    pub fn hold(&self, phase: Phase) -> Arc<Gate> {
        self.hold_matching(
            phase,
            #[cfg(feature = "core")]
            None,
        )
    }

    #[cfg(feature = "core")]
    pub fn hold_for(&self, phase: Phase, target: JobTarget) -> Arc<Gate> {
        self.hold_matching(phase, Some(target))
    }

    fn hold_matching(
        &self,
        phase: Phase,
        #[cfg(feature = "core")] target: Option<JobTarget>,
    ) -> Arc<Gate> {
        let gate = Arc::new(Gate {
            phase,
            #[cfg(feature = "core")]
            target,
            unwind: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        match self.gate.lock() {
            Ok(mut slot) => *slot = Some(gate.clone()),
            Err(error) => *error.into_inner() = Some(gate.clone()),
        }
        gate
    }

    pub fn release_gate(&self) {
        let gate = match self.gate.lock() {
            Ok(mut slot) => slot.take(),
            Err(error) => error.into_inner().take(),
        };
        if let Some(gate) = gate {
            gate.release.notify_one();
        }
    }

    pub fn unwind_gate(&self) -> Result<()> {
        let gate = match self.gate.lock() {
            Ok(mut slot) => slot.take(),
            Err(error) => error.into_inner().take(),
        }
        .ok_or_else(|| anyhow::anyhow!("transaction test gate missing"))?;
        gate.unwind.store(true, Ordering::Release);
        gate.release.notify_one();
        Ok(())
    }

    pub async fn wait_fenced(&self) {
        let mut state = self.state.subscribe();
        // The self-owned Sender should remain live even when no waiter remains.
        if state.wait_for(|value| value.fenced).await.is_err() {
            tracing::error!("tenant fence watch closed; authority fenced");
            self.quarantine();
        }
    }
}
