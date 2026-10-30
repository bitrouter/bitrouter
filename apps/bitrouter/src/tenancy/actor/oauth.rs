//! OAuth slot transitions use the same H transaction supervisor and file owner.
use super::{Control, ErrorCode, Failure, Worker};
use crate::tenancy::{
    oauth::{
        exchange::{Job, Operation},
        storage::{self, State},
    },
    receipts::auxiliary::Outcome,
};
use std::sync::{Arc, atomic::Ordering};

impl Worker {
    pub(super) async fn execute_oauth(&self, job: &Job, control: &Control) -> Result<(), Failure> {
        let exchange = &job.exchange;
        if !Arc::ptr_eq(&exchange.owner.origin, &self.proof)
            || exchange.owner.installation() != &exchange.original.metadata.installation
        {
            return Err(ErrorCode::Forbidden.into());
        }
        let expected = match job.operation {
            Operation::Start => {
                exchange.execution.revalidate().map_err(Failure::from)?;
                exchange.original.clone()
            }
            Operation::Commit | Operation::Release => exchange.current().map_err(Failure::from)?,
        };
        let next = match job.operation {
            Operation::Start => exchange.start.get().cloned().ok_or(ErrorCode::Forbidden)?,
            Operation::Commit => exchange
                .replacement
                .get()
                .cloned()
                .ok_or(ErrorCode::Forbidden)?,
            Operation::Release => {
                if !matches!(expected.metadata.state, State::RefreshStarted(_)) {
                    return Ok(());
                }
                if let Some(replacement) = exchange.replacement.get() {
                    replacement.clone()
                } else {
                    exchange.release_candidate().map_err(Failure::from)?
                }
            }
        };
        let transaction = control
            .begin(&self.db)
            .await
            .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
        let write = async {
            control
                .check_read()
                .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
            let row = storage::load(
                &transaction,
                &expected.metadata.installation,
                &expected.metadata.reference,
            )
            .await
            .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?
            .ok_or(ErrorCode::AuthorityUnavailable)?;
            row.token(
                &self
                    .resources
                    .oauth_crypto()
                    .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?,
            )
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
            if row.same(&next) {
                return Ok(false);
            } // Same private immutable candidate only.
            if !row.same(&expected) {
                return Err(ErrorCode::CapacityExceeded.into());
            }
            if job.operation == Operation::Start {
                exchange.execution.revalidate().map_err(Failure::from)?;
                if !matches!(row.metadata.state, State::Ready) {
                    return Err(ErrorCode::CapacityExceeded.into());
                }
            } else {
                match &row.metadata.state {
                    State::RefreshStarted(work)
                        if work == exchange.work().map_err(Failure::from)? => {}
                    _ => return Err(ErrorCode::Forbidden.into()),
                }
            }
            storage::replace(&transaction, &row, &next)
                .await
                .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
            control
                .check_read()
                .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
            Ok::<_, Failure>(true)
        }
        .await;
        let wrote = match write {
            Ok(wrote) => wrote,
            Err(error) => {
                control
                    .rollback(transaction)
                    .await
                    .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
                return Err(error);
            }
        };
        control
            .commit(transaction)
            .await
            .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
        {
            // No management revision bump. Send gates take snapshot read then
            // stamp read in this same order, making publication indivisible.
            let _authority = self
                .snapshot
                .write()
                .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
            if wrote {
                self.oauth
                    .publish(&expected.metadata, next.metadata.clone())
                    .map_err(Failure::from)?;
            }
            exchange.published(next.clone()).map_err(Failure::from)?;
        }
        match job.operation {
            Operation::Start => {}
            Operation::Commit | Operation::Release => {
                let outcome = if next.metadata.generation > exchange.original.metadata.generation {
                    Outcome::Refreshed
                } else if exchange.armed.load(Ordering::Acquire) {
                    Outcome::Unknown
                } else {
                    Outcome::NotSent
                };
                if outcome == Outcome::Refreshed {
                    exchange
                        .owner
                        .acknowledge_oauth_replacement(&next)
                        .map_err(Failure::from)?;
                }
                exchange.finish(outcome).map_err(Failure::from)?;
            }
        }
        Ok(())
    }
}
