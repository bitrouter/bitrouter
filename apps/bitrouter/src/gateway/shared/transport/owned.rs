//! Retained accept and connection roots, independent of shutdown waiters.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::task::{Context, Poll};

use anyhow::{Result, ensure};
use tokio::net::TcpStream;
use tokio::task::{JoinError, JoinHandle, JoinSet};

use super::*;

#[derive(Clone, Copy, Debug, Default)]
pub(in crate::gateway::shared) struct DriverReport {
    pub joined: bool,
    pub failed: bool,
    pub cancelled: bool,
    pub panicked: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub(in crate::gateway::shared) struct ConnectionsReport {
    pub started: u64,
    pub joined: u64,
    pub cancelled: u64,
    pub panicked: u64,
    pub pending: usize,
}

#[derive(Clone, Copy, Debug)]
pub(in crate::gateway::shared) struct Report {
    pub driver: DriverReport,
    pub connections: ConnectionsReport,
    pub forced: bool,
    pub joining: bool,
}

impl Report {
    pub fn is_quiescent(self) -> bool {
        self.driver.joined && self.connections.pending == 0 && !self.joining
    }

    pub fn is_clean(self) -> bool {
        self.is_quiescent()
            && !self.forced
            && !self.driver.failed
            && !self.driver.cancelled
            && !self.driver.panicked
            && self.connections.cancelled == 0
            && self.connections.panicked == 0
    }
}

struct State {
    accepting: bool,
    forced: bool,
    deadline: Option<Instant>,
    tasks: JoinSet<()>,
    report: ConnectionsReport,
}

struct Connections {
    state: Mutex<State>,
    slots: Arc<Semaphore>,
    stopping: CancellationToken,
}

impl Connections {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn register(&self, socket: TcpStream, router: Router, limits: Limits) {
        let mut state = self.lock();
        // This is the same gate that closes registration and aborts all tasks.
        // A socket accepted before force cannot escape by registering afterward.
        if !state.accepting || state.tasks.len() >= limits.connections {
            return;
        }
        let Ok(permit) = self.slots.clone().try_acquire_owned() else {
            return;
        };
        let stopping = self.stopping.clone();
        state.tasks.spawn(async move {
            let _permit = permit;
            connection(socket, router, limits, stopping).await;
        });
        state.report.started += 1;
    }

    fn stop(&self, deadline: Instant) -> Result<()> {
        {
            let mut state = self.lock();
            ensure!(
                !state.forced,
                "forced transport cannot restart graceful shutdown"
            );
            ensure!(
                state.deadline.is_none_or(|original| original == deadline),
                "transport shutdown deadline cannot change"
            );
            state.deadline = Some(deadline);
            state.accepting = false;
        }
        self.stopping.cancel();
        Ok(())
    }

    fn force(&self) {
        {
            let mut state = self.lock();
            state.accepting = false;
            state.forced = true;
            state.tasks.abort_all();
        }
        self.stopping.cancel();
    }

    fn poll_next(&self, cx: &mut Context<'_>) -> Poll<Option<Result<(), JoinError>>> {
        self.lock().tasks.poll_join_next(cx)
    }

    fn record(&self, result: Result<(), JoinError>) {
        {
            let mut state = self.lock();
            state.report.joined += 1;
            if let Err(error) = &result {
                state.report.cancelled += u64::from(error.is_cancelled());
                state.report.panicked += u64::from(error.is_panic());
            }
        }
        // Returned panic payloads may have arbitrary/reentrant destructors.
        drop(result);
    }

    fn pending(&self) -> usize {
        self.lock().tasks.len()
    }

    async fn drive(
        self: Arc<Self>,
        listener: TcpListener,
        router: Router,
        limits: Limits,
    ) -> std::io::Result<()> {
        loop {
            tokio::select! {
                biased;
                () = self.stopping.cancelled() => return Ok(()),
                result = std::future::poll_fn(|cx| self.poll_next(cx)), if self.pending() > 0 => {
                    if let Some(result) = result { self.record(result); }
                },
                accepted = listener.accept() => {
                    let (socket, _) = accepted?;
                    self.register(socket, router.clone(), limits);
                }
            }
        }
    }
}

enum Driver {
    Running(JoinHandle<std::io::Result<()>>),
    Joined(DriverReport),
}

pub(in crate::gateway::shared) struct Transport {
    connections: Arc<Connections>,
    driver: Mutex<Driver>,
    waiting: AtomicBool,
}

struct Waiter<'a>(&'a Transport);

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        self.0.waiting.store(false, Ordering::Release);
    }
}

impl Transport {
    pub fn start(listener: TcpListener, router: Router, limits: Limits) -> Result<Self> {
        ensure!(
            (1..=65_536).contains(&limits.connections),
            "invalid transport connection limit"
        );
        let runtime = tokio::runtime::Handle::try_current()?;
        let connections = Arc::new(Connections {
            state: Mutex::new(State {
                accepting: true,
                forced: false,
                deadline: None,
                tasks: JoinSet::new(),
                report: ConnectionsReport::default(),
            }),
            slots: Arc::new(Semaphore::new(limits.connections)),
            stopping: CancellationToken::new(),
        });
        // No await occurs before both roots are retained in this owner. The
        // connection set is outside the accept future and survives its failure.
        let task = runtime.spawn(connections.clone().drive(listener, router, limits));
        Ok(Self {
            connections,
            driver: Mutex::new(Driver::Running(task)),
            waiting: AtomicBool::new(false),
        })
    }

    fn waiter(&self) -> Result<Waiter<'_>> {
        ensure!(
            self.waiting
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "transport already has an active waiter"
        );
        Ok(Waiter(self))
    }

    fn poll_driver(&self, cx: &mut Context<'_>) -> Poll<DriverReport> {
        let (report, result) = {
            let mut driver = self
                .driver
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let result = match &mut *driver {
                Driver::Joined(report) => return Poll::Ready(*report),
                Driver::Running(task) => match Pin::new(task).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(result) => result,
                },
            };
            let mut report = DriverReport {
                joined: true,
                ..DriverReport::default()
            };
            match &result {
                Ok(Ok(())) => {}
                Ok(Err(_)) => report.failed = true,
                Err(error) => {
                    report.cancelled = error.is_cancelled();
                    report.panicked = error.is_panic();
                }
            }
            *driver = Driver::Joined(report);
            (report, result)
        };
        drop(result);
        Poll::Ready(report)
    }

    pub fn report(&self) -> Report {
        let driver = match &*self
            .driver
            .lock()
            .unwrap_or_else(|error| error.into_inner())
        {
            Driver::Running(_) => DriverReport::default(),
            Driver::Joined(report) => *report,
        };
        let state = self.connections.lock();
        Report {
            driver,
            connections: ConnectionsReport {
                pending: state.tasks.len(),
                ..state.report
            },
            forced: state.forced,
            // Sample after the root/set state. A join observer still owns any
            // returned panic payload until its destructor has finished.
            joining: self.waiting.load(Ordering::Acquire),
        }
    }

    pub fn stop(&self, deadline: Instant) -> Result<()> {
        self.connections.stop(deadline)
    }

    /// Observe accept failure without transferring its handle into the caller.
    pub async fn wait_driver(&self) -> Result<DriverReport> {
        let _waiter = self.waiter()?;
        Ok(std::future::poll_fn(|cx| self.poll_driver(cx)).await)
    }

    async fn join_all(&self) {
        // Only the accept driver reaps children while it is alive. Joining it
        // first prevents competing JoinSet waiters from replacing their wakers.
        std::future::poll_fn(|cx| self.poll_driver(cx)).await;
        while let Some(result) = std::future::poll_fn(|cx| self.connections.poll_next(cx)).await {
            self.connections.record(result);
        }
    }

    pub async fn drain_until(&self, deadline: Instant) -> Result<Report> {
        self.stop(deadline)?;
        let waiter = self.waiter()?;
        let _ = tokio::time::timeout_at(deadline, self.join_all()).await;
        drop(waiter);
        Ok(self.report())
    }

    /// Signal every external root before awaiting any. This is available while
    /// a graceful waiter is pending; that waiter retains all joins on cancellation.
    pub fn request_force(&self) {
        self.connections.force();
        if let Driver::Running(task) = &*self
            .driver
            .lock()
            .unwrap_or_else(|error| error.into_inner())
        {
            task.abort();
        }
    }

    pub async fn join_forced(&self) -> Result<Report> {
        ensure!(
            self.connections.lock().forced,
            "transport has not been forced"
        );
        let waiter = self.waiter()?;
        self.join_all().await;
        drop(waiter);
        Ok(self.report())
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        if !self.report().is_quiescent() {
            self.request_force();
            tracing::error!("Shared transport dropped before its roots joined");
        }
    }
}

#[cfg(test)]
mod tests;
