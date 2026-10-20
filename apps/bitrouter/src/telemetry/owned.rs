//! Process-owned exporter tasks and cancellation-safe shutdown observation.
//!
//! Generation retirement borrows the exporter; only the process owner closes
//! it after every pipeline has stopped producing observations.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

use anyhow::{Result, ensure};
use tokio::runtime::Handle;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;

use super::config::OtelConfig;
use super::exporter::{OtelExporter, ShutdownReport};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WorkerReport {
    pub joined: u64,
    pub cancelled: u64,
    pub panicked: u64,
    pub discarded: u64,
}

impl WorkerReport {
    fn is_clean(self) -> bool {
        self.cancelled == 0 && self.panicked == 0 && self.discarded == 0
    }
}

#[derive(Default)]
struct WorkerState {
    closed: bool,
    forced: bool,
    tasks: JoinSet<bool>,
    report: WorkerReport,
}

pub(super) struct Workers {
    runtime: Handle,
    state: Mutex<WorkerState>,
    lifetime: Arc<Mutex<Option<Arc<OtelExporter>>>>,
}

/// Field order matters: drop the entire SDK future (including its command
/// receivers) before releasing the exporter keepalive. SDK provider Drop can
/// synchronously request shutdown and must never wait on a still-live receiver
/// whose cancellation needs this same runtime thread.
struct Retained<F> {
    work: Pin<Box<F>>,
    _lifetime: Arc<Mutex<Option<Arc<OtelExporter>>>>,
}

impl<F: Future> Future for Retained<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.work.as_mut().poll(cx)
    }
}

impl std::fmt::Debug for Workers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExporterWorkers")
            .finish_non_exhaustive()
    }
}

impl Workers {
    fn new() -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            runtime: Handle::try_current()?,
            state: Mutex::new(WorkerState::default()),
            lifetime: Arc::new(Mutex::new(None)),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, WorkerState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub(super) fn spawn(&self, work: impl Future<Output = ()> + Send + 'static) {
        let mut state = self.lock();
        let discard = state.closed;
        // Spawn never polls synchronously. In particular, future and panic
        // payload destruction must not re-enter this ownership mutex.
        // https://docs.rs/tokio/latest/tokio/task/fn.spawn.html
        let work = async move {
            if discard {
                drop(work);
                false
            } else {
                work.await;
                true
            }
        };
        state.tasks.spawn_on(
            Retained {
                work: Box::pin(work),
                _lifetime: self.lifetime.clone(),
            },
            &self.runtime,
        );
    }

    fn close(&self, force: bool) {
        let mut state = self.lock();
        state.closed = true;
        if force && !state.forced {
            state.forced = true;
            state.tasks.abort_all();
        }
    }

    fn report(&self) -> WorkerReport {
        self.lock().report
    }

    fn poll_joined(&self, cx: &mut Context<'_>) -> Poll<WorkerReport> {
        let mut completed = Vec::new();
        {
            let mut state = self.lock();
            for _ in 0..32 {
                let Poll::Ready(Some(result)) = state.tasks.poll_join_next(cx) else {
                    break;
                };
                state.report.joined = state.report.joined.saturating_add(1);
                match &result {
                    Ok(true) => {}
                    Ok(false) => state.report.discarded = state.report.discarded.saturating_add(1),
                    Err(error) if error.is_cancelled() => {
                        state.report.cancelled = state.report.cancelled.saturating_add(1)
                    }
                    Err(_) => state.report.panicked = state.report.panicked.saturating_add(1),
                }
                completed.push(result);
            }
        }
        if completed.len() == 32 {
            cx.waker().wake_by_ref();
        }
        drop(completed);
        let state = self.lock();
        if state.closed && state.tasks.is_empty() {
            Poll::Ready(state.report)
        } else {
            Poll::Pending
        }
    }
}

#[derive(Default)]
struct Shutdown {
    task: Option<JoinHandle<ShutdownReport>>,
    outcome: Option<ShutdownReport>,
    started: bool,
    panicked: bool,
    cancelled: bool,
    forced: bool,
    waiting: bool,
    completed: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Drain {
    pub quiescent: bool,
    pub started: bool,
    pub forced: bool,
    pub blocking_panicked: bool,
    pub blocking_cancelled: bool,
    pub exporter: Option<ShutdownReport>,
    pub workers: WorkerReport,
}

impl Drain {
    pub fn is_clean(self) -> bool {
        self.quiescent
            && !self.forced
            && !self.blocking_panicked
            && !self.blocking_cancelled
            && self.exporter.is_some_and(ShutdownReport::is_clean)
            && self.workers.is_clean()
    }
}

pub(crate) struct ProcessExporter {
    exporter: Arc<OtelExporter>,
    workers: Arc<Workers>,
    shutdown: Mutex<Shutdown>,
}

struct Waiter<'a>(&'a ProcessExporter);

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        self.0.lock().waiting = false;
    }
}

impl ProcessExporter {
    /// The process supplies its telemetry configuration. This path adds no
    /// application-level environment overrides or global propagator/subscriber.
    /// The OTLP transport's standard environment is read only at this process
    /// construction, never when a routing generation borrows the exporter.
    pub fn new(config: OtelConfig) -> Result<Self> {
        let workers = Workers::new()?;
        let exporter = Arc::new(
            OtelExporter::new_owned(config, workers.clone())
                .map_err(|_| anyhow::anyhow!("process telemetry initialization failed"))?,
        );
        *workers
            .lifetime
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(exporter.clone());
        Ok(Self {
            exporter,
            workers,
            shutdown: Mutex::new(Shutdown::default()),
        })
    }

    pub fn exporter(&self) -> Arc<OtelExporter> {
        self.exporter.clone()
    }

    fn lock(&self) -> MutexGuard<'_, Shutdown> {
        self.shutdown
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn waiter(&self) -> Result<Waiter<'_>> {
        let mut state = self.lock();
        ensure!(
            !state.waiting,
            "process telemetry shutdown already has a waiter"
        );
        state.waiting = true;
        Ok(Waiter(self))
    }

    fn start_shutdown(&self, flush: bool) {
        let mut state = self.lock();
        if !state.started {
            state.started = true;
            let exporter = self.exporter.clone();
            // Blocking work cannot reliably be aborted after it starts. Its
            // handle stays in this process owner until completion is observed.
            // https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html
            state.task = Some(tokio::task::spawn_blocking(move || {
                exporter.shutdown_recorded(flush)
            }));
        }
    }

    fn poll_blocking(&self, cx: &mut Context<'_>) -> Poll<()> {
        let result = {
            let mut state = self.lock();
            let started = state.started;
            let result = match state.task.as_mut() {
                Some(task) => match Pin::new(task).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(result) => Some(result),
                },
                None if started => None,
                None => return Poll::Pending,
            };
            state.task = None;
            if let Some(result) = &result {
                match result {
                    Ok(report) => state.outcome = Some(*report),
                    Err(error) => {
                        state.panicked |= error.is_panic();
                        state.cancelled |= error.is_cancelled();
                    }
                }
            }
            result
        };
        drop(result);
        Poll::Ready(())
    }

    fn report(&self, quiescent: bool) -> Drain {
        let state = self.lock();
        Drain {
            quiescent,
            started: state.started,
            forced: state.forced,
            blocking_panicked: state.panicked,
            blocking_cancelled: state.cancelled,
            exporter: state.outcome,
            workers: self.workers.report(),
        }
    }

    fn poll_joined(&self, cx: &mut Context<'_>) -> Poll<Drain> {
        // Poll both owners, including while the SDK synchronously waits for its
        // asynchronous processor. Neither handle moves into the waiter.
        let blocking = self.poll_blocking(cx);
        let workers = self.workers.poll_joined(cx);
        if blocking.is_ready() && workers.is_ready() {
            self.lock().completed = true;
            Poll::Ready(self.report(true))
        } else {
            Poll::Pending
        }
    }

    /// Call after all generation producers/tasks have joined. Every retry uses
    /// the process owner's original absolute deadline unless explicitly extended.
    pub async fn drain_until(&self, deadline: Instant) -> Result<Drain> {
        let _waiter = self.waiter()?;
        if self.lock().completed {
            return Ok(self.report(true));
        }
        if Instant::now() >= deadline {
            return Ok(self.report(false));
        }
        self.workers.close(false);
        self.start_shutdown(true);
        Ok(
            match tokio::time::timeout_at(deadline, std::future::poll_fn(|cx| self.poll_joined(cx)))
                .await
            {
                Ok(report) => report,
                Err(_) => self.report(false),
            },
        )
    }

    /// Abort and join actual processor futures before requesting provider
    /// teardown. No new flush is started after forcing; already running blocking
    /// work remains owned and must really finish before reporting quiescence.
    pub async fn force_join(&self) -> Result<Drain> {
        let _waiter = self.waiter()?;
        if self.lock().completed {
            return Ok(self.report(true));
        }
        self.lock().forced = true;
        self.workers.close(true);
        std::future::poll_fn(|cx| self.workers.poll_joined(cx)).await;
        self.start_shutdown(false);
        Ok(std::future::poll_fn(|cx| self.poll_joined(cx)).await)
    }
}

impl Drop for ProcessExporter {
    fn drop(&mut self) {
        let state = self
            .shutdown
            .get_mut()
            .unwrap_or_else(|error| error.into_inner());
        let workers = self.workers.lock();
        let incomplete = state.task.is_some() || !workers.closed || !workers.tasks.is_empty();
        drop(workers);
        if incomplete {
            self.workers.close(true);
            // A blocking task already owns its exporter clone. Dropping this
            // owner cannot cancel that work and is never a successful drain.
            tracing::error!("Process telemetry owner dropped before its joins completed");
        }
    }
}

#[cfg(test)]
mod tests;
