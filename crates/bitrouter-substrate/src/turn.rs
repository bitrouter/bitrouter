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
//! # Cancellation (flush semantics)
//!
//! Cancellation has two halves, matching ACP's session-scoped `session/cancel`:
//!
//! - the **active** turn is cancelled at the upstream level — the engine's
//!   `Session::cancel` sends ACP `session/cancel`, which makes the in-flight
//!   turn complete cooperatively (`StopReason::Cancelled`);
//! - the **queued backlog** is flushed by [`TurnController::flush`]: it bumps a
//!   generation counter, and the worker resolves every job submitted before the
//!   bump with the controller's `flushed` value (the engine supplies a
//!   synthesized `StopReason::Cancelled` response) instead of running it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow};
use tokio::sync::{mpsc, oneshot};

/// A job sent from `submit`/`try_submit` to the worker.
struct Job<I, T> {
    input: I,
    /// Generation at submit time; the worker skips jobs older than the current
    /// generation (they were flushed by a cancel).
    generation: u64,
    reply: oneshot::Sender<Result<T>>,
}

/// Per-session single-writer turn queue.
///
/// Generic over the turn input `I` and output `T` (and over the turn-runner
/// closure) so it is unit-testable without a live upstream pipeline. The engine
/// instantiates it as `TurnController<Vec<ContentBlock>, PromptResponse>` so
/// each turn carries the prompt's content blocks verbatim (multi-modal, not
/// text-flattened) and yields the upstream's typed prompt result.
pub struct TurnController<I, T> {
    tx: mpsc::Sender<Job<I, T>>,
    /// Current generation; bumped by [`flush`](Self::flush) to invalidate every
    /// job queued before the bump.
    generation: Arc<AtomicU64>,
}

impl<I: Send + 'static, T: Send + 'static> TurnController<I, T> {
    /// Create a new controller.
    ///
    /// - `bound`: maximum number of turns that may be queued at once.
    /// - `run_turn`: closure that executes one turn given its input; the engine
    ///   passes a closure that calls `acp::Pipeline::execute` for the session.
    /// - `flushed`: value resolved for a queued job that was flushed by
    ///   [`flush`](Self::flush) before it started; the engine passes a
    ///   synthesized `StopReason::Cancelled` prompt response.
    pub fn new<F, Fut, G>(bound: usize, run_turn: F, flushed: G) -> Self
    where
        F: Fn(I) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T>> + Send + 'static,
        G: Fn() -> Result<T> + Send + 'static,
    {
        let (tx, mut rx) = mpsc::channel::<Job<I, T>>(bound);
        let generation = Arc::new(AtomicU64::new(0));
        let worker_generation = Arc::clone(&generation);

        tokio::spawn(async move {
            while let Some(job) = rx.recv().await {
                // A job from a flushed generation resolves with the `flushed`
                // value instead of running.
                let result = if job.generation < worker_generation.load(Ordering::Acquire) {
                    flushed()
                } else {
                    run_turn(job.input).await
                };
                // Ignore send error: the caller may have dropped the receiver.
                let _ = job.reply.send(result);
            }
        });

        Self { tx, generation }
    }

    /// Flush the queued backlog: every job submitted before this call resolves
    /// with the controller's `flushed` value instead of running. Does **not**
    /// affect the active turn — cancel that at the upstream level
    /// (`session/cancel`), which is what the engine's `Session::cancel` does
    /// alongside this.
    pub fn flush(&self) {
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Enqueue a turn.
    ///
    /// On a full channel this does **not** panic; it returns a receiver that
    /// immediately resolves to `Err("turn queue full")` so the caller can still
    /// `.await` the handle uniformly.
    pub fn submit(&self, input: I) -> oneshot::Receiver<Result<T>> {
        match self.enqueue(input) {
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
    pub fn try_submit(&self, input: I) -> Result<oneshot::Receiver<Result<T>>> {
        self.enqueue(input)
    }

    // --- internal ---

    fn enqueue(&self, input: I) -> Result<oneshot::Receiver<Result<T>>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let job = Job {
            input,
            generation: self.generation.load(Ordering::Acquire),
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
        let c = TurnController::new(
            4,
            move |label: String| {
                let o = o.clone();
                async move {
                    o.lock().unwrap().push(format!("start {label}"));
                    tokio::task::yield_now().await;
                    o.lock().unwrap().push(format!("end {label}"));
                    Ok::<(), anyhow::Error>(())
                }
            },
            || Ok(()),
        );
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
    async fn flush_resolves_queued_jobs_without_running_them() {
        use std::sync::{Arc, Mutex};
        // The first job parks on a oneshot so the backlog stays queued while we
        // flush; after flush it is released and completes normally (it was
        // active, not queued). The queued jobs must resolve with the `flushed`
        // marker and never run.
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let release_rx = Arc::new(Mutex::new(Some(release_rx)));
        let ran = Arc::new(Mutex::new(Vec::<String>::new()));
        let ran_in_turn = ran.clone();
        let c = TurnController::new(
            8,
            move |label: String| {
                let ran = ran_in_turn.clone();
                let release_rx = release_rx.clone();
                async move {
                    ran.lock().unwrap().push(label.clone());
                    if label == "active" {
                        let rx = release_rx.lock().unwrap().take();
                        if let Some(rx) = rx {
                            let _ = rx.await;
                        }
                    }
                    Ok::<String, anyhow::Error>(label)
                }
            },
            || Ok("flushed".to_string()),
        );

        let active = c.submit("active".into());
        let queued_a = c.submit("qa".into());
        let queued_b = c.submit("qb".into());
        // Let the worker pick up the active job before flushing.
        tokio::task::yield_now().await;

        c.flush();
        // A job submitted AFTER the flush runs normally.
        let fresh = c.submit("fresh".into());
        release_tx.send(()).expect("release active");

        assert_eq!(active.await.unwrap().unwrap(), "active");
        assert_eq!(queued_a.await.unwrap().unwrap(), "flushed");
        assert_eq!(queued_b.await.unwrap().unwrap(), "flushed");
        assert_eq!(fresh.await.unwrap().unwrap(), "fresh");
        // The flushed jobs never executed.
        assert_eq!(*ran.lock().unwrap(), vec!["active", "fresh"]);
    }

    #[tokio::test]
    async fn queue_rejects_past_bound() {
        let c = TurnController::new(
            1,
            |_l: String| async {
                tokio::task::yield_now().await;
                Ok::<(), anyhow::Error>(())
            },
            || Ok(()),
        );
        let _r = c.submit("A".into());
        let _q = c.submit("B".into());
        assert!(c.try_submit("C".into()).is_err());
    }

    #[tokio::test]
    async fn submit_on_full_queue_returns_err_receiver() {
        // bound=1; A occupies worker, B fills the single queue slot,
        // C submitted via submit() (not try_submit) must return an Err receiver.
        let c = TurnController::new(
            1,
            |_l: String| async {
                tokio::task::yield_now().await;
                Ok::<(), anyhow::Error>(())
            },
            || Ok(()),
        );
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
        let c = TurnController::new(
            4,
            move |label: String| {
                let o = o.clone();
                async move {
                    o.lock().unwrap().push(label);
                    Ok::<(), anyhow::Error>(())
                }
            },
            || Ok(()),
        );
        c.submit("A".into()).await.unwrap().unwrap();
        c.submit("B".into()).await.unwrap().unwrap();
        assert_eq!(*order.lock().unwrap(), vec!["A", "B"]);
    }
}
