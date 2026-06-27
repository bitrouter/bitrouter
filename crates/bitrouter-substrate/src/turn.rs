//! Per-session single-writer turn queue.
//!
//! [`TurnController`] serialises concurrent client prompts into ordered turns:
//! the spawned worker drains the FIFO channel and awaits each turn to completion
//! before starting the next, guaranteeing single-writer access to the upstream.
//!
//! # `submit` vs `try_submit`
//!
//! - [`TurnController::try_submit`] maps a full channel to `Err`; use this for
//!   backpressure in the engine.
//! - [`TurnController::submit`] never panics; on a full channel it returns a
//!   receiver that immediately resolves to `Err("turn queue full")` so the
//!   caller can still `.await` it uniformly.
//!
//! # Cancellation
//!
//! [`TurnController::cancel`] sets an `AtomicBool` flag that the worker checks
//! between turns: any turns still in the queue are drained and their reply
//! channels are resolved with `Err("cancelled")`.  The flag is also exposed via
//! [`TurnController::cancel_flag`] so the engine can pass it into the
//! `run_turn` closure for in-flight upstream cancellation via `session/cancel`.

use anyhow::{Result, anyhow};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{mpsc, oneshot};

/// A job sent from `submit`/`try_submit` to the worker.
struct Job {
    label: String,
    reply: oneshot::Sender<Result<()>>,
}

/// Per-session single-writer turn queue.
///
/// Generic over the turn-runner closure so it is unit-testable without a live
/// upstream pipeline.
pub struct TurnController {
    tx: mpsc::Sender<Job>,
    /// Shared cancellation flag.  `cancel()` sets this; the worker checks it
    /// between turns and drains remaining queued jobs with `Err("cancelled")`.
    /// The engine can pass `Arc::clone(&cancel_flag)` into `run_turn` to
    /// propagate in-flight upstream cancellation via `session/cancel`.
    cancel_flag: Arc<AtomicBool>,
}

impl TurnController {
    /// Create a new controller.
    ///
    /// - `bound`: maximum number of turns that may be queued at once.
    /// - `run_turn`: closure that executes one turn given its label; the engine
    ///   passes a closure that calls `acp::Pipeline::execute` for the session.
    pub fn new<F, Fut>(bound: usize, run_turn: F) -> Self
    where
        F: Fn(String) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        let (tx, mut rx) = mpsc::channel::<Job>(bound);
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let flag = cancel_flag.clone();

        tokio::spawn(async move {
            while let Some(job) = rx.recv().await {
                if flag.load(Ordering::Acquire) {
                    // Cancellation was requested: reject this job and drain
                    // the rest of the queue.
                    let _ = job.reply.send(Err(anyhow!("cancelled")));
                    while let Ok(pending) = rx.try_recv() {
                        let _ = pending.reply.send(Err(anyhow!("cancelled")));
                    }
                    break;
                }

                let result = run_turn(job.label).await;
                // Ignore send error: the caller may have dropped the receiver.
                let _ = job.reply.send(result);
            }
        });

        Self { tx, cancel_flag }
    }

    /// Enqueue a turn.
    ///
    /// On a full channel this does **not** panic; it returns a receiver that
    /// immediately resolves to `Err("turn queue full")` so the caller can still
    /// `.await` the handle uniformly.
    pub fn submit(&self, label: String) -> oneshot::Receiver<Result<()>> {
        match self.enqueue(label) {
            Ok(rx) => rx,
            Err(_full) => {
                let (tx, rx) = oneshot::channel();
                let _ = tx.send(Err(anyhow!("turn queue full")));
                rx
            }
        }
    }

    /// Enqueue a turn, returning `Err` when the bounded channel is full.
    ///
    /// Use this for backpressure: the engine can report the rejection to the
    /// client rather than silently queuing without bound.
    pub fn try_submit(&self, label: String) -> Result<oneshot::Receiver<Result<()>>> {
        self.enqueue(label)
    }

    /// Signal cancellation.
    ///
    /// Sets the shared `AtomicBool` flag.  The worker checks this flag between
    /// turns and will drain remaining queued jobs with `Err("cancelled")`.
    ///
    /// The engine should also pass `Arc::clone` of the flag (obtained via
    /// [`TurnController::cancel_flag`]) into `run_turn` so the in-flight turn's
    /// upstream `session/cancel` call can be triggered while it is running.
    pub fn cancel(&self) {
        self.cancel_flag.store(true, Ordering::Release);
    }

    /// Expose the cancellation flag so the engine can wire in-flight upstream
    /// cancellation (`session/cancel`) inside `run_turn`.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel_flag.clone()
    }

    // --- internal ---

    fn enqueue(&self, label: String) -> Result<oneshot::Receiver<Result<()>>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let job = Job {
            label,
            reply: reply_tx,
        };
        self.tx
            .try_send(job)
            .map_err(|e| anyhow!("turn queue full: {e}"))?;
        Ok(reply_rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn second_prompt_waits_for_first() {
        use std::sync::{Arc, Mutex};
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let o = order.clone();
        let c = TurnController::new(4, move |label: String| {
            let o = o.clone();
            async move {
                o.lock().unwrap().push(format!("start {label}"));
                tokio::task::yield_now().await;
                o.lock().unwrap().push(format!("end {label}"));
                Ok::<(), anyhow::Error>(())
            }
        });
        let h1 = c.submit("A".into());
        let h2 = c.submit("B".into());
        h1.await.unwrap().unwrap();
        h2.await.unwrap().unwrap();
        assert_eq!(
            *order.lock().unwrap(),
            vec!["start A", "end A", "start B", "end B"]
        );
    }

    #[tokio::test]
    async fn queue_rejects_past_bound() {
        let c = TurnController::new(1, |_l: String| async {
            tokio::task::yield_now().await;
            Ok::<(), anyhow::Error>(())
        });
        let _r = c.submit("A".into());
        let _q = c.submit("B".into());
        assert!(c.try_submit("C".into()).is_err());
    }

    #[tokio::test]
    async fn cancel_drains_pending_turns() {
        use std::sync::{Arc, Mutex};
        // Slow runner: yields many times so that A is still in flight when we cancel.
        let started = Arc::new(Mutex::new(false));
        let started2 = started.clone();
        let c = TurnController::new(4, move |_label: String| {
            let s = started2.clone();
            async move {
                *s.lock().unwrap() = true;
                // Yield so the test can cancel while A is running.
                for _ in 0..10 {
                    tokio::task::yield_now().await;
                }
                Ok::<(), anyhow::Error>(())
            }
        });
        let _h_a = c.submit("A".into());
        // Let the worker pick up A.
        tokio::task::yield_now().await;
        let h_b = c.submit("B".into());
        c.cancel();
        // B should resolve to an error (cancelled or queue-full — either means rejected).
        let result = h_b.await.unwrap();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn submit_on_full_queue_returns_err_receiver() {
        // bound=1; A occupies worker, B fills the single queue slot,
        // C submitted via submit() (not try_submit) must return an Err receiver.
        let c = TurnController::new(1, |_l: String| async {
            tokio::task::yield_now().await;
            Ok::<(), anyhow::Error>(())
        });
        let _a = c.submit("A".into());
        let _b = c.submit("B".into());
        let c_rx = c.submit("C".into()); // full — must not panic
        let result = c_rx.await.unwrap();
        assert!(result.is_err());
    }
}
