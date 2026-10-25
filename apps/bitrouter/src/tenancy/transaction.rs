//! Explicit custody of the two authority transaction consumers.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::{Result, ensure};
use sea_orm::{DatabaseConnection, DatabaseTransaction, DbErr, TransactionTrait};
use tokio::sync::watch;
use tokio::time::Instant;

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
}

/// Joining the worker is insufficient without its explicit transaction outcome.
pub(super) struct Proof {
    state: watch::Sender<Evidence>,
    live: Arc<AtomicBool>,
    #[cfg(test)]
    gate: std::sync::Mutex<Option<Arc<Gate>>>,
    #[cfg(test)]
    pub read_stopped: tokio::sync::Notify,
}

impl Proof {
    pub fn new(live: Arc<AtomicBool>) -> Arc<Self> {
        Arc::new(Self {
            live,
            state: watch::channel(Evidence {
                disposition: Disposition::NoTransaction,
                #[cfg(test)]
                fenced: false,
            })
            .0,
            #[cfg(test)]
            gate: std::sync::Mutex::new(None),
            #[cfg(test)]
            read_stopped: tokio::sync::Notify::new(),
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
    progress: watch::Sender<Progress>,
    stopped: AtomicBool,
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
    pub fn new(proof: Arc<Proof>) -> Self {
        proof.record(Disposition::NoTransaction);
        Self {
            proof,
            progress: watch::channel(Progress {
                phase: Phase::Preparing,
                cleanup_deadline: None,
            })
            .0,
            stopped: AtomicBool::new(false),
        }
    }

    pub fn progress(&self) -> watch::Receiver<Progress> {
        self.progress.subscribe()
    }

    pub fn phase(&self) -> Phase {
        self.progress.borrow().phase
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
            if let Some(gate) = gate.filter(|gate| gate.phase == phase) {
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
        self.proof.record(disposition);
        self.progress.send_modify(|state| {
            state.phase = Phase::Settled;
            state.cleanup_deadline = None;
        });
    }

    pub async fn begin(&self, db: &DatabaseConnection) -> Result<DatabaseTransaction> {
        self.proof.record(Disposition::Pending);
        self.enter(Phase::Beginning).await;
        match db.begin().await {
            Ok(transaction) => {
                self.enter(Phase::Active).await;
                Ok(transaction)
            }
            Err(error) => {
                // The typed pool-acquire failure precedes transaction construction.
                // Other begin failures may drop an already-open transaction.
                // https://docs.rs/sea-orm/1.1.20/src/sea_orm/driver/sqlx_sqlite.rs.html
                self.complete(if matches!(&error, DbErr::ConnectionAcquire(_)) {
                    Disposition::NoTransaction
                } else {
                    Disposition::Ambiguous
                });
                Err(error.into())
            }
        }
    }

    pub async fn commit(&self, transaction: DatabaseTransaction) -> Result<()> {
        self.enter(Phase::Committing).await;
        let result = transaction.commit().await;
        // Both consuming methods clear `open` only on Ok. On Err, Drop merely
        // queues rollback; its completion is not acknowledged to this caller.
        // https://docs.rs/sea-orm/1.1.20/src/sea_orm/database/transaction.rs.html
        self.complete(if result.is_ok() {
            Disposition::Committed
        } else {
            Disposition::Ambiguous
        });
        result.map_err(Into::into)
    }

    pub async fn rollback(&self, transaction: DatabaseTransaction) -> Result<()> {
        self.start_cleanup();
        self.enter(Phase::RollingBack).await;
        let result = transaction.rollback().await;
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

#[cfg(test)]
pub(super) struct Gate {
    phase: Phase,
    unwind: AtomicBool,
    pub entered: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}

#[cfg(test)]
impl Proof {
    pub fn hold(&self, phase: Phase) -> Arc<Gate> {
        let gate = Arc::new(Gate {
            phase,
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
