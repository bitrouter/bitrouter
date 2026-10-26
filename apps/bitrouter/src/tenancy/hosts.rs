//! Service-owned capacity and task custody shared by authenticated local hosts.
use super::{Service, error::Failure, execution::host::Authority};
use crate::gateway::pipeline::owned_tasks::{OwnedTasks, Source};
use bitrouter_ai::tenancy::operation::ErrorCode;
use futures::FutureExt;
use std::{future::Future, sync::Arc};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

pub(super) struct Hosts {
    slots: Arc<Semaphore>,
    tasks: Arc<OwnedTasks>,
    stop: CancellationToken,
    #[cfg(test)]
    resources_gate: std::sync::Mutex<Option<(HostBoundary, Arc<HostGate>)>>,
    #[cfg(all(test, unix))]
    first_cleanup_unknown:
        std::sync::OnceLock<(&'static std::panic::Location<'static>, tokio::time::Instant)>,
}

pub(crate) struct HostSlot {
    permit: Option<OwnedSemaphorePermit>,
    source: Option<Source>,
    finished: tokio::sync::watch::Sender<bool>,
}

impl HostSlot {
    pub(crate) fn finished(&self) -> tokio::sync::watch::Receiver<bool> {
        self.finished.subscribe()
    }
}
impl Drop for HostSlot {
    fn drop(&mut self) {
        drop(self.permit.take());
        drop(self.source.take());
        self.finished.send_replace(true);
    }
}

impl Hosts {
    pub(super) fn new() -> Self {
        Self {
            slots: Arc::new(Semaphore::new(1)),
            tasks: Arc::new(OwnedTasks::default()),
            stop: CancellationToken::new(),
            #[cfg(test)]
            resources_gate: Default::default(),
            #[cfg(all(test, unix))]
            first_cleanup_unknown: Default::default(),
        }
    }
    pub(super) fn close(&self) {
        self.slots.close();
        self.tasks.close_sources();
        self.stop.cancel();
    }
    pub(super) async fn drain(&self) -> anyhow::Result<()> {
        let summary = self.tasks.drain().await?;
        anyhow::ensure!(
            summary.is_clean(),
            "authenticated host cleanup is unconfirmed"
        );
        Ok(())
    }
}

// A single test-owned barrier in the actual native opener, after resource
// ownership is registered. It adds no production switch or owner registry.
#[cfg(test)]
pub(crate) struct HostGate {
    pub entered: tokio::sync::Notify,
    pub mcp: std::sync::OnceLock<
        std::sync::Weak<tokio::sync::Mutex<crate::harness::resources::mcp::McpConnections>>,
    >,
    #[cfg(unix)]
    pub http: std::sync::OnceLock<crate::gateway::mcp::client::HttpProbe>,
    release: tokio::sync::Notify,
    unwind: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostBoundary {
    BeforeTransport,
    Transport,
    Resources,
    Cleanup,
}

#[cfg(test)]
impl HostGate {
    #[cfg(unix)]
    pub(crate) fn release(&self) {
        self.release.notify_one();
    }
    #[cfg(unix)]
    pub(crate) fn unwind(&self) {
        self.unwind
            .store(true, std::sync::atomic::Ordering::Release);
        self.release();
    }
    pub(crate) async fn reached(&self) {
        self.entered.notify_one();
        self.release.notified().await;
        if self.unwind.load(std::sync::atomic::Ordering::Acquire) {
            std::panic::resume_unwind(Box::new(()));
        }
    }
}

#[cfg(test)]
impl Service {
    #[cfg(unix)]
    pub(crate) fn hold_host(&self, boundary: HostBoundary) -> anyhow::Result<Arc<HostGate>> {
        let gate = Arc::new(HostGate {
            entered: tokio::sync::Notify::new(),
            mcp: std::sync::OnceLock::new(),
            http: std::sync::OnceLock::new(),
            release: tokio::sync::Notify::new(),
            unwind: std::sync::atomic::AtomicBool::new(false),
        });
        let mut slot = self
            .hosts
            .resources_gate
            .lock()
            .map_err(|_| anyhow::anyhow!("host test gate poisoned"))?;
        anyhow::ensure!(slot.is_none(), "host resource gate already installed");
        *slot = Some((boundary, gate.clone()));
        Ok(gate)
    }
    #[cfg(unix)]
    pub(crate) fn host_cleanup_unknown_site_for_test(
        &self,
    ) -> Option<(&'static std::panic::Location<'static>, tokio::time::Instant)> {
        self.hosts.first_cleanup_unknown.get().copied()
    }
    #[cfg(unix)]
    pub(crate) fn host_slots_for_test(&self) -> usize {
        self.hosts.slots.available_permits()
    }
    pub(crate) async fn host_resources_ready(
        &self,
    ) -> Result<(), bitrouter_ai::core::protocol::CoreError> {
        let gate = self.take_host_gate(HostBoundary::Resources)?;
        if let Some(gate) = gate {
            gate.reached().await;
        }
        Ok(())
    }
    pub(crate) fn take_host_gate(
        &self,
        boundary: HostBoundary,
    ) -> Result<Option<Arc<HostGate>>, bitrouter_ai::core::protocol::CoreError> {
        let mut slot = self.hosts.resources_gate.lock().map_err(|_| {
            super::execution::session::core_error(ErrorCode::AuthorityUnavailable.into())
        })?;
        Ok(if slot.as_ref().is_some_and(|(at, _)| *at == boundary) {
            slot.take().map(|(_, gate)| gate)
        } else {
            None
        })
    }
}

impl Service {
    pub(crate) fn host_slot(&self, authority: &Authority) -> Result<HostSlot, Failure> {
        let accepting = self
            .accepting
            .lock()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        if !*accepting || !authority.origin(&self.cleanup) {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        authority.check()?;
        let permit = self
            .hosts
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
        let source = self
            .hosts
            .tasks
            .acquire()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        let (finished, _) = tokio::sync::watch::channel(false);
        Ok(HostSlot {
            permit: Some(permit),
            source: Some(source),
            finished,
        })
    }
    pub(crate) fn host_stop(&self) -> CancellationToken {
        self.hosts.stop.clone()
    }
    pub(crate) fn own_host(&self, slot: HostSlot, work: impl Future<Output = ()> + Send + 'static) {
        let service = self.clone();
        self.hosts.tasks.spawn_execution(async move {
            if std::panic::AssertUnwindSafe(work)
                .catch_unwind()
                .await
                .is_err()
            {
                tracing::error!("authenticated host supervisor unwound; owner retained");
                service.host_cleanup_unknown();
                std::future::pending::<()>().await;
            }
            drop(slot);
        });
    }
    #[cfg_attr(all(test, unix), track_caller)]
    pub(crate) fn host_cleanup_unknown(&self) {
        // One fixed observation preserves the first cause across later cleanup.
        // It never replaces quarantine or changes a cleanup acknowledgement.
        #[cfg(all(test, unix))]
        {
            let caller = std::panic::Location::caller();
            self.hosts
                .first_cleanup_unknown
                .get_or_init(|| (caller, tokio::time::Instant::now()));
        }
        self.cleanup.quarantine();
    }
}
