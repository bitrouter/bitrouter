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
//! The controller is pure FIFO serialization and does **not** itself cancel.
//! Turn cancellation is handled at the upstream level: the engine's
//! `Session::cancel` calls ACP `session/cancel` on the upstream connection,
//! which makes the in-flight turn complete cooperatively (`StopReason::Cancelled`).
//! v1 semantic: cancel affects the *active* turn (via upstream), not the queued
//! backlog — queued turns proceed normally once the active one finishes.

use anyhow::{Result, anyhow};
use tokio::sync::{mpsc, oneshot};

/// A job sent from `submit`/`try_submit` to the worker.
struct Job<T> {
    label: String,
    reply: oneshot::Sender<Result<T>>,
}

/// Per-session single-writer turn queue.
///
/// Generic over the turn output `T` (and over the turn-runner closure) so it is
/// unit-testable without a live upstream pipeline. The engine instantiates it as
/// `TurnController<PromptResponse>` so each turn yields the upstream's typed
/// prompt result.
pub struct TurnController<T> {
    tx: mpsc::Sender<Job<T>>,
}

impl<T: Send + 'static> TurnController<T> {
    /// Create a new controller.
    ///
    /// - `bound`: maximum number of turns that may be queued at once.
    /// - `run_turn`: closure that executes one turn given its label; the engine
    ///   passes a closure that calls `acp::Pipeline::execute` for the session.
    pub fn new<F, Fut>(bound: usize, run_turn: F) -> Self
    where
        F: Fn(String) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T>> + Send + 'static,
    {
        let (tx, mut rx) = mpsc::channel::<Job<T>>(bound);

        tokio::spawn(async move {
            while let Some(job) = rx.recv().await {
                let result = run_turn(job.label).await;
                // Ignore send error: the caller may have dropped the receiver.
                let _ = job.reply.send(result);
            }
        });

        Self { tx }
    }

    /// Enqueue a turn.
    ///
    /// On a full channel this does **not** panic; it returns a receiver that
    /// immediately resolves to `Err("turn queue full")` so the caller can still
    /// `.await` the handle uniformly.
    pub fn submit(&self, label: String) -> oneshot::Receiver<Result<T>> {
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
    pub fn try_submit(&self, label: String) -> Result<oneshot::Receiver<Result<T>>> {
        self.enqueue(label)
    }

    // --- internal ---

    fn enqueue(&self, label: String) -> Result<oneshot::Receiver<Result<T>>> {
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

    #[tokio::test]
    async fn controller_accepts_turns_after_one_completes() {
        // Guards that the worker survives across turns: submit A, await it,
        // then submit B, await it. The old `break`-on-cancel worker would have
        // been fine here, but a worker that exits on any signal would not be.
        use std::sync::{Arc, Mutex};
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let o = order.clone();
        let c = TurnController::new(4, move |label: String| {
            let o = o.clone();
            async move {
                o.lock().unwrap().push(label);
                Ok::<(), anyhow::Error>(())
            }
        });
        c.submit("A".into()).await.unwrap().unwrap();
        c.submit("B".into()).await.unwrap().unwrap();
        assert_eq!(*order.lock().unwrap(), vec!["A", "B"]);
    }
}
