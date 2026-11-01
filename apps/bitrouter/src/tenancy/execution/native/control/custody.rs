//! A callback's bounded reply is independent of its retained checkpoint work.
use super::*;
use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use tokio::{sync::oneshot, time::Instant};

fn admission(execution: &TenantExecution) -> Result<()> {
    execution
        .revalidate()
        .map_err(|code| execution.reject(code))?;
    if execution.receipt_owner()?.cancelled.is_cancelled() {
        return Err(crate::gateway::error::BitrouterError::UpstreamUnavailable);
    }
    Ok(())
}

impl Bounded {
    pub(super) async fn admit<T, F>(
        &self,
        start: impl FnOnce(Arc<dyn NativeExecutionControl>) -> F + Send + 'static,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        admission(&self.execution)?;
        let value = self
            .retained(self.execution.submission.deadline.instant(), true, start)
            .await?;
        admission(&self.execution)?;
        Ok(value)
    }

    pub(super) async fn complete<T, F>(
        &self,
        start: impl FnOnce(Arc<dyn NativeExecutionControl>) -> F + Send + 'static,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        // Bound only the waiter. An already owed outcome must still finish if
        // this deadline has passed, authority is revoked, or the caller leaves.
        // This is the original callback deadline, not a renewed cleanup period.
        let deadline = self.execution.receipt_owner()?.cleanup_deadline();
        self.retained(Some(deadline), false, start).await
    }

    async fn retained<T, F>(
        &self,
        deadline: Option<Instant>,
        admitting: bool,
        start: impl FnOnce(Arc<dyn NativeExecutionControl>) -> F + Send + 'static,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        let (reply, receiver) = oneshot::channel();
        let (execution, inner, tasks) = (
            self.execution.clone(),
            self.inner.clone(),
            self.tasks.clone(),
        );
        // The actual pipeline is registered and its external-source gate is
        // closed. Register this descendant synchronously before the first poll;
        // never acquire another Source or reopen the serving generation.
        self.tasks.spawn_settlement(async move {
            if admitting {
                // A queued callback may first poll after cancellation/deadline.
                // In particular, it must not start late preparation work.
                if reply.is_closed() {
                    return;
                }
                if let Err(error) = admission(&execution) {
                    let _ = reply.send(Err(error));
                    return;
                }
            }
            // Keep the control and execution in this outer frame through an
            // unwind. They may own a pending ACK or retained external effects.
            let outcome = AssertUnwindSafe(async { start(inner.clone()).await })
                .catch_unwind()
                .await;
            match outcome {
                Ok(result) => {
                    let result = if admitting {
                        admission(&execution).and(result)
                    } else {
                        result
                    };
                    let _ = reply.send(result);
                }
                Err(payload) => {
                    tasks.record_finalizer_failure();
                    if let Ok(receipt) = execution.receipt_owner() {
                        receipt.fail(ErrorCode::StorageUnavailable);
                    }
                    let _ = reply.send(Err(execution.reject(ErrorCode::StorageUnavailable)));
                    // Unwind is not an ACK or proof of cleanup. The existing
                    // execution registry retains this same task and file owner.
                    std::future::pending::<()>().await;
                    drop((payload, inner, execution));
                }
            }
        });
        // Caller cancellation or timeout drops only this receiver. The original
        // callback remains in the pipeline's settlement set until it resolves.
        // A forcing/sealed owner discards it before polling and cannot report a
        // clean drain; a closed reply must never be interpreted as success.
        let receive = async {
            receiver
                .await
                .map_err(|_| self.execution.reject(ErrorCode::StorageUnavailable))?
        };
        let waiting = async {
            match deadline {
                Some(deadline) => self.until(deadline, receive).await,
                None => receive.await,
            }
        };
        if admitting {
            tokio::select! {
                biased;
                () = self.execution.submission.cancelled.cancelled() => Err(self.execution.reject(ErrorCode::CapacityExceeded)),
                result = waiting => result,
            }
        } else {
            waiting.await
        }
    }
}
