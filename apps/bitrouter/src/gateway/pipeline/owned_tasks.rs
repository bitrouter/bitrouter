//! Shared-generation task ownership. Waiters borrow the task sets; cancellation
//! never transfers, drops or detaches their handles.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

use tokio::task::{JoinError, JoinSet};
use tokio::time::Instant;

use crate::gateway::error::{BitrouterError, Result};

const REAP_BATCH: usize = 64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Summary {
    pub forced: bool,
    pub joined: u64,
    pub cancelled: u64,
    pub panicked: u64,
    pub discarded: u64,
    pub finalizer_failures: u64,
    pub late_registrations: u64,
}

impl Summary {
    pub fn is_clean(self) -> bool {
        !self.forced
            && self.cancelled == 0
            && self.panicked == 0
            && self.discarded == 0
            && self.finalizer_failures == 0
            && self.late_registrations == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Pending {
    pub sources: usize,
    pub executions: usize,
    pub settlements: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Drain {
    Joined(Summary),
    Retained(Pending),
}

enum Completion {
    Finished,
    Discarded,
}

#[derive(Default)]
struct State {
    bound: bool,
    closed: bool,
    forcing: bool,
    sealed: bool,
    sources: usize,
    executions: JoinSet<Completion>,
    settlements: JoinSet<Completion>,
    summary: Summary,
    waiting: bool,
    wake: Option<Waker>,
}

impl State {
    fn record(&mut self, result: &std::result::Result<Completion, JoinError>) {
        self.summary.joined = self.summary.joined.saturating_add(1);
        match result {
            Ok(Completion::Finished) => {}
            Ok(Completion::Discarded) => {
                self.summary.discarded = self.summary.discarded.saturating_add(1);
            }
            Err(error) if error.is_cancelled() => {
                self.summary.cancelled = self.summary.cancelled.saturating_add(1);
            }
            Err(_) => {
                self.summary.panicked = self.summary.panicked.saturating_add(1);
            }
        }
    }

    fn pending(&self) -> Pending {
        Pending {
            sources: self.sources,
            executions: self.executions.len(),
            settlements: self.settlements.len(),
        }
    }
}

#[derive(Default)]
pub(crate) struct OwnedTasks {
    state: Mutex<State>,
}

/// An external producer, retained through destruction of its response body.
/// Only the serving acquisition gate creates these non-clonable tickets.
pub(crate) struct Source {
    owner: Arc<OwnedTasks>,
}

impl Drop for Source {
    fn drop(&mut self) {
        let wake = {
            let mut state = self.owner.lock();
            state.sources -= 1;
            state.wake.clone()
        };
        if let Some(wake) = wake {
            wake.wake();
        }
    }
}

struct Waiter<'a>(&'a OwnedTasks);

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        let wake = {
            let mut state = self.0.lock();
            state.waiting = false;
            state.wake.take()
        };
        drop(wake);
    }
}

impl OwnedTasks {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// One pipeline owns exactly one serving generation, including candidates.
    pub fn bind_snapshot(&self) -> Result<()> {
        let mut state = self.lock();
        if state.bound || state.closed {
            return Err(BitrouterError::internal(
                "shared pipeline already belongs to a serving generation",
            ));
        }
        state.bound = true;
        Ok(())
    }

    pub fn acquire(self: &Arc<Self>) -> Result<Source> {
        let mut state = self.lock();
        if state.closed {
            return Err(BitrouterError::internal("shared generation is withdrawn"));
        }
        state.sources = state
            .sources
            .checked_add(1)
            .ok_or_else(|| BitrouterError::internal("shared producer count exhausted"))?;
        Ok(Source {
            owner: self.clone(),
        })
    }

    /// Called under the serving publication gate. Existing tickets may finish.
    pub fn close_sources(&self) {
        let wake = {
            let mut state = self.lock();
            state.closed = true;
            state.wake.clone()
        };
        if let Some(wake) = wake {
            wake.wake();
        }
    }

    pub fn pending(&self) -> Pending {
        self.lock().pending()
    }

    pub fn record_finalizer_failure(&self) {
        let mut state = self.lock();
        state.summary.finalizer_failures = state.summary.finalizer_failures.saturating_add(1);
    }

    pub fn spawn_execution(&self, work: impl Future<Output = ()> + Send + 'static) {
        self.spawn(work, true);
    }

    pub fn spawn_settlement(&self, work: impl Future<Output = ()> + Send + 'static) {
        self.spawn(work, false);
    }

    fn spawn(&self, work: impl Future<Output = ()> + Send + 'static, execution: bool) {
        let mut completed = Vec::new();
        let wake = {
            let mut state = self.lock();
            // Do not replace a suspended drain's JoinSet waker.
            if !state.waiting {
                for _ in 0..REAP_BATCH {
                    let next = state
                        .executions
                        .try_join_next()
                        .or_else(|| state.settlements.try_join_next());
                    let Some(result) = next else { break };
                    state.record(&result);
                    completed.push(result);
                }
            }
            if state.sealed {
                state.summary.late_registrations =
                    state.summary.late_registrations.saturating_add(1);
            }
            let forcing = state.forcing || state.sealed;
            // Tokio never synchronously polls a spawned future. In particular,
            // dropping work (which can register compensation) happens outside
            // this mutex. A post-deadline child must NEVER poll before abort.
            // https://docs.rs/tokio/latest/tokio/task/fn.spawn.html#using-send-values-from-a-task
            let task = async move {
                if forcing {
                    drop(work);
                    Completion::Discarded
                } else {
                    work.await;
                    Completion::Finished
                }
            };
            if execution {
                state.executions.spawn(task);
            } else {
                state.settlements.spawn(task);
            }
            state.wake.clone()
        };
        // A panic payload may have a user-defined, reentrant destructor.
        drop(completed);
        if let Some(wake) = wake {
            wake.wake();
        }
    }

    fn waiter(&self) -> Result<Waiter<'_>> {
        let mut state = self.lock();
        if !state.closed || state.waiting {
            return Err(BitrouterError::internal(
                "shared drain requires closed sources and one waiter",
            ));
        }
        state.waiting = true;
        Ok(Waiter(self))
    }

    fn poll_joined(&self, cx: &mut Context<'_>) -> Poll<Summary> {
        let mut completed = Vec::new();
        {
            let mut state = self.lock();
            state.wake = Some(cx.waker().clone());
            for _ in 0..REAP_BATCH {
                // Both groups progress while producers remain. A parent is
                // joined only after its destructor has registered descendants.
                // https://docs.rs/tokio/latest/tokio/task/struct.JoinSet.html#method.poll_join_next
                let next = match state.executions.poll_join_next(cx) {
                    Poll::Ready(Some(result)) => Some(result),
                    _ => match state.settlements.poll_join_next(cx) {
                        Poll::Ready(Some(result)) => Some(result),
                        _ => None,
                    },
                };
                let Some(result) = next else { break };
                state.record(&result);
                completed.push(result);
            }
        }
        if completed.len() == REAP_BATCH {
            cx.waker().wake_by_ref();
        }
        drop(completed);
        // Dropping a joined panic payload can itself register compensation.
        // Check the fixed point only after all returned values are destroyed.
        let mut state = self.lock();
        if state.sources == 0 && state.executions.is_empty() && state.settlements.is_empty() {
            state.sealed = true;
            Poll::Ready(state.summary)
        } else {
            Poll::Pending
        }
    }

    /// An elapsed grace deadline retains every handle and producer ticket.
    /// Callers keep the generation and may retry with the same deadline.
    pub async fn drain_until(&self, deadline: Instant) -> Result<Drain> {
        let _waiter = self.waiter()?;
        match tokio::time::timeout_at(deadline, std::future::poll_fn(|cx| self.poll_joined(cx)))
            .await
        {
            Ok(summary) => Ok(Drain::Joined(summary)),
            Err(_) => Ok(Drain::Retained(self.pending())),
        }
    }

    /// Transport roots must already be destroyed and joined. Force is permanent:
    /// descendants registered by cancellation are owned but never polled.
    pub fn force(&self) -> Result<()> {
        let mut state = self.lock();
        if !state.closed || state.sources != 0 {
            return Err(BitrouterError::internal(
                "shared force requires joined external producers",
            ));
        }
        if !state.forcing {
            state.forcing = true;
            state.summary.forced = true;
            state.executions.abort_all();
            state.settlements.abort_all();
        }
        Ok(())
    }

    /// No fresh grace period is granted after force. Cancellation retains the
    /// joins; a noncooperative task must be reported, never claimed as drained.
    pub async fn join_forced(&self) -> Result<Summary> {
        let _waiter = self.waiter()?;
        if !self.lock().forcing {
            return Err(BitrouterError::internal(
                "shared tasks have not been forced",
            ));
        }
        Ok(std::future::poll_fn(|cx| self.poll_joined(cx)).await)
    }
}

#[cfg(test)]
mod tests;
