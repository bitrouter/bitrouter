//! Persistent shared continuation-worker ownership and bounded reconciliation.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, MutexGuard};
use std::task::{Context, Poll};

use anyhow::{Result, ensure};
use tokio::time::Instant;

use super::{ContinuationRegistry, PendingPublicationPhase, ReconcilerTask};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Worker {
    pub started: bool,
    pub joined: bool,
    pub forced: bool,
    pub cancelled: bool,
    pub panicked: bool,
}

impl Worker {
    pub fn is_clean(self) -> bool {
        self.joined && !self.forced && !self.cancelled && !self.panicked
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Markers {
    pub total: usize,
    pub reserved: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Retained {
    Deadline,
    Backend,
    NoProgress,
    Forced,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Drain {
    pub worker: Worker,
    pub markers: Markers,
    pub retained: Option<Retained>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Status {
    pub worker: Worker,
    pub markers: Markers,
    pub producer_closed: bool,
    pub joining: bool,
}

impl Status {
    pub fn is_quiescent(self) -> bool {
        self.worker.joined && !self.joining
    }
}

impl Drain {
    pub fn is_clean(self) -> bool {
        self.retained.is_none() && self.markers.total == 0 && self.worker.is_clean()
    }
}

#[derive(Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Running,
    Draining,
    Stopped,
}

#[derive(Default)]
struct State {
    mode: Mode,
    task: Option<ReconcilerTask>,
    worker: Worker,
    waiting: bool,
}

#[derive(Default)]
pub(super) struct Control {
    state: Mutex<State>,
    forced: tokio::sync::Notify,
}

struct Waiter<'a>(&'a Control);

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        self.0.lock().waiting = false;
    }
}

impl Control {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// At most one worker, with no restart after failure or withdrawal.
    pub fn start(&self, spawn: impl FnOnce() -> ReconcilerTask) {
        let mut state = self.lock();
        if state.mode == Mode::Running && !state.worker.started {
            state.task = Some(spawn());
            state.worker.started = true;
        }
    }

    fn waiter(&self) -> Result<Waiter<'_>> {
        let mut state = self.lock();
        ensure!(
            !state.waiting,
            "shared continuation drain already has a waiter"
        );
        state.waiting = true;
        Ok(Waiter(self))
    }

    fn begin_drain(&self, notify: &tokio::sync::Notify, force: bool) {
        {
            let mut state = self.lock();
            if state.mode == Mode::Running {
                state.mode = Mode::Draining;
            }
            state.worker.forced |= force;
            if let Some(task) = &state.task {
                task.cancelled
                    .store(true, std::sync::atomic::Ordering::Release);
                if force {
                    task.handle.abort();
                }
            }
        }
        notify.notify_one();
        if force {
            self.forced.notify_waiters();
        }
    }

    fn poll_joined(&self, cx: &mut Context<'_>) -> Poll<Worker> {
        let (worker, result) = {
            let mut state = self.lock();
            let result = match &mut state.task {
                Some(task) => match Pin::new(&mut task.handle).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(result) => Some(result),
                },
                None => None,
            };
            // A handle is removed only after its completion has been observed.
            // https://docs.rs/tokio/latest/tokio/task/struct.JoinHandle.html#cancel-safety
            state.task = None;
            state.mode = Mode::Stopped;
            state.worker.joined = true;
            if let Some(Err(error)) = &result {
                state.worker.cancelled |= error.is_cancelled();
                state.worker.panicked |= error.is_panic();
            }
            (state.worker, result)
        };
        // Panic payload destructors may re-enter start; the gate is already shut.
        drop(result);
        Poll::Ready(worker)
    }

    fn worker(&self) -> Worker {
        self.lock().worker
    }

    #[cfg(test)]
    pub fn healthy(&self) -> bool {
        let state = self.lock();
        state.mode == Mode::Running
            && state
                .task
                .as_ref()
                .is_none_or(|task| !task.handle.is_finished())
    }

    #[cfg(test)]
    pub(super) fn task_id(&self) -> Option<tokio::task::Id> {
        self.lock().task.as_ref().map(|task| task.handle.id())
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(task) = &state.task {
            // This is an incomplete owner teardown, never a successful drain.
            // Normal lifecycle owners retain Control until poll_joined completes.
            task.handle.abort();
            tracing::error!("Shared continuation owner dropped before worker join");
        }
    }
}

impl ContinuationRegistry {
    /// Select shared ownership before the registry or any worker is published.
    pub(crate) fn with_owned_lifecycle(mut self) -> Result<Self> {
        let inner = std::sync::Arc::get_mut(&mut self.inner)
            .ok_or_else(|| anyhow::anyhow!("continuation registry is already shared"))?;
        ensure!(
            inner.reconciler.owned.is_none(),
            "continuation lifecycle was already selected"
        );
        ensure!(
            inner
                .reconciler
                .task
                .get_mut()
                .unwrap_or_else(|error| error.into_inner())
                .is_none(),
            "continuation worker was already started"
        );
        inner.reconciler.owned = Some(Control::default());
        Ok(self)
    }

    fn owned(&self) -> Result<&Control> {
        self.inner
            .reconciler
            .owned
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("continuation registry has no shared lifecycle"))
    }

    fn owned_markers(&self) -> Markers {
        if let Some(shared) = &self.inner.shared {
            return shared.markers();
        }
        let pending = self
            .inner
            .pending_publications
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Markers {
            total: pending.len(),
            reserved: pending
                .values()
                .filter(|publication| publication.phase == PendingPublicationPhase::Reserved)
                .count(),
        }
    }

    fn owned_report(&self, retained: Option<Retained>) -> Result<Drain> {
        Ok(Drain {
            worker: self.owned()?.worker(),
            markers: self.owned_markers(),
            retained,
        })
    }

    #[cfg(test)]
    pub(crate) fn owned_worker_healthy(&self) -> bool {
        self.inner
            .reconciler
            .owned
            .as_ref()
            .is_some_and(Control::healthy)
    }

    /// Refresh this after pipeline producers join: their destructors may leave
    /// additional markers even when the native worker has already stopped.
    pub(crate) fn owned_status(&self) -> Result<Status> {
        let (worker, joining) = {
            let state = self.owned()?.lock();
            (state.worker, state.waiting)
        };
        Ok(Status {
            worker,
            markers: self.owned_markers(),
            producer_closed: self
                .inner
                .shared
                .as_ref()
                .is_none_or(|shared| shared.producer_closed()),
            joining,
        })
    }

    /// Borrow the worker handle and use the caller's single absolute deadline.
    /// Cancellation retains the worker, closed restart gate and marker map.
    /// Call only after pipeline producers and tasks have joined.
    pub(crate) async fn drain_owned_until(&self, deadline: Instant) -> Result<Drain> {
        let control = self.owned()?;
        let _waiter = control.waiter()?;
        let forced = control.forced.notified();
        tokio::pin!(forced);
        forced.as_mut().enable();
        if control.worker().forced {
            return self.owned_report(Some(Retained::Forced));
        }
        // Register before observing the latch so force cannot be lost between
        // the check and a blocked worker/database await.
        // https://docs.rs/tokio/latest/tokio/sync/struct.Notify.html#method.notified
        tokio::select! {
            biased;
            () = &mut forced => self.owned_report(Some(Retained::Forced)),
            result = self.drain_owned_grace(control, deadline) => result,
        }
    }

    async fn drain_owned_grace(&self, control: &Control, deadline: Instant) -> Result<Drain> {
        if let Some(shared) = &self.inner.shared {
            shared.stop_accepting();
        }
        control.begin_drain(&self.inner.reconciler.notify, false);
        if tokio::time::timeout_at(deadline, std::future::poll_fn(|cx| control.poll_joined(cx)))
            .await
            .is_err()
        {
            return self.owned_report(Some(Retained::Deadline));
        }
        if control.worker().forced {
            return self.owned_report(Some(Retained::Forced));
        }
        if Instant::now() >= deadline {
            return self.owned_report(Some(Retained::Deadline));
        }
        // Even an empty local map needs the durable producer-close barrier:
        // cancelled SQL may still hold a bind transaction on the primary.
        if let Some(shared) = &self.inner.shared {
            match tokio::time::timeout_at(deadline, shared.close_producer()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(%error, "Shared continuation producer close retained ownership");
                    return self.owned_report(Some(Retained::Backend));
                }
                Err(_) => return self.owned_report(Some(Retained::Deadline)),
            }
        }
        self.mark_pending_for_shutdown_reconciliation();
        loop {
            if control.worker().forced {
                return self.owned_report(Some(Retained::Forced));
            }
            let before = self.owned_markers();
            if before.total == 0 {
                return self.owned_report(None);
            }
            if Instant::now() >= deadline {
                return self.owned_report(Some(Retained::Deadline));
            }
            let had_error = match tokio::time::timeout_at(
                deadline,
                self.reconcile_local_publications(chrono::Utc::now()),
            )
            .await
            {
                Ok(had_error) => had_error,
                Err(_) => return self.owned_report(Some(Retained::Deadline)),
            };
            let after = self.owned_markers();
            if after.total == 0 {
                return self.owned_report(None);
            }
            if after.total >= before.total {
                return self.owned_report(Some(if had_error {
                    Retained::Backend
                } else {
                    Retained::NoProgress
                }));
            }
            tokio::task::yield_now().await;
        }
    }

    /// Broadcast without waiting, including while graceful cleanup is pending.
    /// The force latch also cancels that borrowed cleanup before its next batch.
    pub(crate) fn request_force_owned(&self) -> Result<()> {
        let control = self.owned()?;
        if let Some(shared) = &self.inner.shared {
            shared.stop_accepting();
        }
        control.begin_drain(&self.inner.reconciler.notify, true);
        Ok(())
    }

    /// No database compensation starts here. Cancellation retains the handle.
    pub(crate) async fn join_forced_owned(&self) -> Result<Drain> {
        let control = self.owned()?;
        ensure!(
            control.worker().forced,
            "continuation worker has not been forced"
        );
        let _waiter = control.waiter()?;
        std::future::poll_fn(|cx| control.poll_joined(cx)).await;
        self.owned_report(Some(Retained::Forced))
    }

    #[cfg(test)]
    pub(crate) async fn force_owned(&self) -> Result<Drain> {
        self.request_force_owned()?;
        self.join_forced_owned().await
    }
}
