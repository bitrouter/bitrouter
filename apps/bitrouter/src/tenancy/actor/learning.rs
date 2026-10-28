use super::*;
use crate::{
    storage::operation_budget::{Error as BudgetError, OperationBudget},
    tenancy::learning::{
        queue::{Job, Reply, Work},
        store as learning_store,
    },
};

impl Worker {
    // Keep the bounded paid transaction continuation off the dispatcher stack.
    // The original H actor still polls it under the same custody and deadlines.
    pub(super) fn execute_learning<'a>(
        &'a self,
        job: &'a Job,
        control: &'a Control,
    ) -> futures::future::BoxFuture<'a, Result<Reply, Failure>> {
        Box::pin(async move {
            let binding = job.work.binding();
            let cleanup = job.work.cleanup();
            let check = || {
                if Instant::now() >= job.deadline || control.check_read().is_err() {
                    return Err(ErrorCode::CapacityExceeded);
                }
                if !binding.origin(&self.proof) || !self.live.0.load(Ordering::Acquire) {
                    return Err(ErrorCode::AuthorityUnavailable);
                }
                if !cleanup {
                    if !*self
                        .accepting
                        .lock()
                        .map_err(|_| ErrorCode::AuthorityUnavailable)?
                    {
                        return Err(ErrorCode::AuthorityUnavailable);
                    }
                    binding.assessment_check()?;
                }
                Ok(())
            };
            let map = |error: anyhow::Error| match error.downcast_ref::<BudgetError>() {
                Some(BudgetError::Rejected(code)) => Failure::from(*code),
                Some(BudgetError::Capacity) => Failure::from(ErrorCode::CapacityExceeded),
                _ => self.unavailable(ErrorCode::StorageUnavailable),
            };
            check().map_err(Failure::from)?;
            // Expiry can race awaited SQL. Retry selection once as owner-bound
            // retention; never retry a model or infer that a provider was unsent.
            let mut select =
                matches!(&job.work, Work::Finish(_)) && binding.assessment_check().is_ok();
            for _ in 0..2 {
                let tx = control
                    .begin_repeatable_read(&self.db)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                let result = async {
                    let budget = OperationBudget::with_row_limit(&check, 1024);
                    let reply = match &job.work {
                        Work::NativePrepare(request) => Reply::Prepared(Arc::new(
                            crate::tenancy::learning::native::prepare(&tx, request, &budget)
                                .await?,
                        )),
                        Work::NativeStopped(proof) => {
                            crate::tenancy::learning::native::stopped(&tx, proof, &budget).await?;
                            Reply::NativeStopped
                        }
                        Work::Prepare {
                            binding,
                            native,
                            summary,
                        } => Reply::Prepared(Arc::new(
                            learning_store::prepare(&tx, binding, native, summary, &budget).await?,
                        )),
                        Work::Accept(owner) => Reply::Accepted(
                            learning_store::accept(
                                &tx,
                                &binding,
                                &owner.prepared,
                                &owner.execution,
                                owner.recovery.as_ref(),
                                &budget,
                            )
                            .await?
                            .is_none(),
                        ),
                        Work::CaptureStopped(proof) => {
                            learning_store::capture_stopped(&tx, proof, &budget).await?;
                            Reply::CaptureStopped
                        }
                        Work::StopSet(proof) => {
                            learning_store::stopped::retain(&tx, proof, &self.proof, &budget)
                                .await?;
                            Reply::Stopped
                        }
                        Work::Observe(owner) => {
                            learning_store::observe(
                                &tx,
                                &binding,
                                &owner.prepared,
                                &owner.execution,
                                owner.observation().map_err(BudgetError::Rejected)?,
                                &budget,
                            )
                            .await?;
                            Reply::Observed
                        }
                        Work::Finish(owner) => {
                            let output = owner.output().map_err(BudgetError::Rejected)?;
                            learning_store::finish(
                                &tx,
                                &binding,
                                &owner.prepared,
                                &owner.execution,
                                output.as_ref(),
                                select,
                                &budget,
                            )
                            .await?;
                            Reply::Finished
                        }
                    };
                    budget.check()?;
                    Ok::<_, anyhow::Error>(reply)
                }
                .await;
                let reply = match result {
                    Ok(reply) => reply,
                    Err(error) => {
                        control
                            .rollback(tx)
                            .await
                            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                        return Err(map(error));
                    }
                };
                if select && binding.assessment_check().is_err() {
                    control
                        .rollback(tx)
                        .await
                        .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                    select = false;
                    continue;
                }
                if matches!(&reply, Reply::Prepared(_) | Reply::Accepted(false)) {
                    control.rollback(tx).await
                } else {
                    control.commit(tx).await
                }
                .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                // Durable markers precede any late deadline check or response send.
                match (&job.work, &reply) {
                    (Work::Accept(owner), Reply::Accepted(fresh)) => owner.accepted(*fresh),
                    (Work::Finish(owner), Reply::Finished) => owner.finished(),
                    (Work::Observe(owner), Reply::Observed) => owner.observed(),
                    _ => {}
                }
                return match check() {
                    Ok(()) => Ok(reply),
                    Err(_) if self.proof.disposition() == Disposition::Committed => {
                        self.proof.fenced();
                        Err(ErrorCode::AuthorityUnavailable.into())
                    }
                    Err(code) => Err(code.into()),
                };
            }
            Err(self.unavailable(ErrorCode::AuthorityUnavailable))
        })
    }
}
