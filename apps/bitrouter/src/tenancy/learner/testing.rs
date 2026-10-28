//! Per-Service synchronization in real learner consumers; absent from production.
use crate::tenancy::error::Failure;
use bitrouter_ai::tenancy::operation::ErrorCode;
use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};
use tokio::{sync::Notify, time::Instant};

#[derive(Default)]
pub(in crate::tenancy) struct Gate {
    pub entered: Notify,
    pub release: Notify,
}
impl Gate {
    async fn wait(&self) {
        self.entered.notify_one();
        self.release.notified().await;
    }
}
#[derive(Default)]
pub(in crate::tenancy) struct Hooks {
    before_publish: Mutex<Option<Arc<Gate>>>,
    precommit: Mutex<Option<Arc<Gate>>>,
    pub queued: Notify,
    crossing: AtomicU8,
    pub crossed: AtomicBool,
}
impl Hooks {
    pub fn hold_publish(&self) -> anyhow::Result<Arc<Gate>> {
        Self::install(&self.before_publish)
    }
    pub fn hold_precommit(&self) -> anyhow::Result<Arc<Gate>> {
        Self::install(&self.precommit)
    }
    fn install(slot: &Mutex<Option<Arc<Gate>>>) -> anyhow::Result<Arc<Gate>> {
        let gate = Arc::new(Gate::default());
        let mut slot = slot
            .lock()
            .map_err(|_| anyhow::anyhow!("learner test hook poisoned"))?;
        anyhow::ensure!(slot.is_none(), "learner test hook already installed");
        *slot = Some(gate.clone());
        Ok(gate)
    }
    fn take(slot: &Mutex<Option<Arc<Gate>>>) -> Option<Arc<Gate>> {
        match slot.lock() {
            Ok(mut value) => value.take(),
            Err(error) => error.into_inner().take(),
        }
    }
    pub async fn before_publish(&self) {
        if let Some(gate) = Self::take(&self.before_publish) {
            gate.wait().await;
        }
    }
    pub async fn precommit_write<T>(
        &self,
        write: impl Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        let result = write.await?;
        // The real SQLite write has completed in the original H transaction;
        // keep that exact awaited participant pending until the test releases it.
        if let Some(gate) = Self::take(&self.precommit) {
            gate.wait().await;
        }
        Ok(result)
    }
    pub fn cross_job_deadline(&self) {
        self.crossing.store(1, Ordering::Release);
    }
    pub fn cross_overall_deadline(&self) {
        self.crossing.store(2, Ordering::Release);
    }
    pub fn cross_final_deadline(
        &self,
        job: Instant,
        overall: Option<Instant>,
    ) -> Result<(), Failure> {
        let deadline = match self.crossing.swap(0, Ordering::AcqRel) {
            0 => return Ok(()),
            1 => job,
            _ => overall.ok_or_else(|| Failure::from(ErrorCode::AuthorityUnavailable))?,
        };
        let delay = deadline.saturating_duration_since(Instant::now());
        if delay > std::time::Duration::from_secs(6) {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        // Deliberately synchronous, after real COMMIT/snapshot publication:
        // the supervisor cannot win an async timer branch and mask this defect.
        std::thread::sleep(delay + std::time::Duration::from_millis(2));
        self.crossed.store(true, Ordering::Release);
        Ok(())
    }
}

/// A failing assertion must still release the real borrowed H/worker owner.
pub(in crate::tenancy) struct Release(pub Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release.notify_one();
    }
}
