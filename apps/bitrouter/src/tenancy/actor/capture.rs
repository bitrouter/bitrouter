//! Capture work uses the same H phase proof, owner and ordinary actor queue.
use super::*;
use crate::storage::operation_budget::{Error as BudgetError, OperationBudget};
use crate::tenancy::capture::queue::{Job, Reply, Request};

impl Worker {
    pub(super) async fn execute_capture(
        &self,
        job: &Job,
        control: &Control,
    ) -> Result<Reply, Failure> {
        let cleanup = job.request.cleanup();
        let check = || {
            if Instant::now() >= job.deadline || control.check_read().is_err() {
                return Err(ErrorCode::CapacityExceeded);
            }
            if !job.binding.origin(&self.proof) || !self.live.0.load(Ordering::Acquire) {
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
                job.binding.check().map_err(|e| e.0.code)?;
            }
            Ok(())
        };
        let map = |error: anyhow::Error| match error.downcast_ref::<BudgetError>() {
            Some(BudgetError::Rejected(code)) => Failure::from(*code),
            Some(BudgetError::Capacity) => Failure::from(ErrorCode::CapacityExceeded),
            _ if error.is::<crate::harness::trajectory::owned::InvalidObservation>() => {
                Failure::from(ErrorCode::InvalidRequest)
            }
            _ => self.unavailable(ErrorCode::StorageUnavailable),
        };
        check().map_err(Failure::from)?;
        let observed = if matches!(&job.request, Request::Append { .. })
            && job.binding.block_profile().is_some()
        {
            Some(job.binding.begin_observation()?)
        } else {
            None
        };
        let generation = if matches!(&job.request, Request::Block(_) | Request::Freeze(_))
            && job.binding.block_profile().is_some()
        {
            Some(job.binding.begin_block_update()?)
        } else {
            None
        };
        let tx = control
            .begin_repeatable_read(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let result = async {
            let budget = if matches!(&job.request, Request::Human(_)) {
                OperationBudget::with_row_limit(&check, 1024)
            } else {
                OperationBudget::new(&check)
            };
            let association = job.binding.association();
            let mut published = None;
            let reply = match &job.request {
                Request::Create(at) => {
                    association.create_in(&tx, at, &budget).await?;
                    crate::tenancy::execution::block::store::initialize(&tx, &job.binding, &budget)
                        .await?;
                    Reply::Created
                }
                Request::Append { value, cleanup } => {
                    if *cleanup {
                        association.check_cleanup_in(&tx, value, &budget).await?;
                    }
                    Reply::Appended(association.append_in(&tx, value, &budget).await?)
                }
                Request::Block(request) => {
                    let value = crate::tenancy::execution::block::store::select(
                        &tx,
                        &job.binding,
                        request,
                        &budget,
                    )
                    .await?;
                    published = Some(value.intent.control_generation);
                    Reply::Block(Box::new(value))
                }
                Request::Human(request) => {
                    let (reply, wrote) = request.execute(&tx, association, &budget).await?;
                    Reply::Human(Box::new(reply), wrote)
                }
                Request::Freeze(native) => {
                    let summary =
                        crate::core::evolution::scheduler::EvolutionScheduler::owned_pass_in(
                            &tx,
                            association,
                            native,
                            &budget,
                        )
                        .await?;
                    if summary.is_some() {
                        published = crate::tenancy::execution::block::store::checkpoint(
                            &tx,
                            &job.binding,
                            &budget,
                        )
                        .await?;
                    }
                    Reply::Frozen(summary)
                }
            };
            budget.check()?;
            Ok::<_, anyhow::Error>((reply, published))
        }
        .await;
        let (reply, published) = match result {
            Ok(reply) => reply,
            Err(error) => {
                control
                    .rollback(tx)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                if let Some(head) = observed {
                    job.binding.observed_head(head);
                }
                if let Some(generation) = generation {
                    job.binding.publish_block_generation(generation);
                }
                return Err(map(error));
            }
        };
        let write = !matches!(&reply, Reply::Appended(value) if value.duplicate)
            && !matches!(&reply, Reply::Frozen(None))
            && !matches!(&reply, Reply::Human(_, false));
        if write {
            control.commit(tx).await
        } else {
            control.rollback(tx).await
        }
        .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        if let Some(generation) = published.or(generation) {
            job.binding.publish_block_generation(generation);
        }
        if let Reply::Appended(value) = &reply {
            job.binding.observed_head(value.head);
        }
        // One sample: a confirmed commit cannot become a capacity rejection
        // if the deadline crosses just before this final checkpoint.
        match check() {
            Err(ErrorCode::CapacityExceeded)
                if self.proof.disposition() == Disposition::Committed =>
            {
                self.proof.fenced();
                Err(ErrorCode::AuthorityUnavailable.into())
            }
            Err(code) => Err(code.into()),
            Ok(()) => Ok(reply),
        }
    }
}
