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
        let header = self.current()?.header();
        let projection = self.admission.unknown_read(true)?;
        let work = exchange.work().map_err(Failure::from)?;
        let key = crate::tenancy::receipts::store::obligations::Key {
            tenant: work.tenant.clone(),
            execution: work.execution.clone(),
        };
        // Both encrypted rows and their metadata/index projections can coexist
        // during replacement. Refresh cleanup remains possible at logical limits.
        control.reserve_write(
            2 * (crate::tenancy::oauth::profile::PAYLOAD_BYTES + 16 * 1024),
            job.operation != Operation::Start,
        )?;
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
            let wrote = !row.same(&next);
            if wrote && !row.same(&expected) {
                return Err(ErrorCode::CapacityExceeded.into());
            }
            if wrote && job.operation == Operation::Start {
                exchange.execution.revalidate().map_err(Failure::from)?;
                if !matches!(row.metadata.state, State::Ready) {
                    return Err(ErrorCode::CapacityExceeded.into());
                }
            } else if wrote {
                match &row.metadata.state {
                    State::RefreshStarted(work)
                        if work == exchange.work().map_err(Failure::from)? => {}
                    _ => return Err(ErrorCode::Forbidden.into()),
                }
            }
            if wrote {
                storage::replace(&transaction, &row, &next)
                    .await
                    .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
            }
            let change = crate::tenancy::receipts::store::obligations::reconcile(
                &transaction,
                &expected.metadata.installation,
                &key,
                &|_, _| control.check_read(),
            )
            .await
            .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
            control
                .check_read()
                .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
            Ok::<_, Failure>((wrote, change))
        }
        .await;
        let (wrote, change) = match write {
            Ok(value) => value,
            Err(error) => {
                control
                    .rollback(transaction)
                    .await
                    .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
                return Err(error);
            }
        };
        let publication = if wrote {
            match self
                .authority
                .prepare(header, &[], Some((&expected.metadata, &next.metadata)))
            {
                Ok(publication) => Some(publication),
                Err(code) => {
                    control
                        .rollback(transaction)
                        .await
                        .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                    return Err(code.into());
                }
            }
        } else {
            None
        };
        let due = change
            .after
            .as_ref()
            .filter(|row| !row.escalated)
            .map(|row| row.due);
        let unknown = match self.admission.prepare_unknown(
            &projection,
            vec![change],
            Vec::new(),
            self.proof.clone(),
        ) {
            Ok(publication) => publication,
            Err(code) => {
                control
                    .rollback(transaction)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                return Err(code.into());
            }
        };
        control
            .commit(transaction)
            .await
            .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
        let published = (|| {
            exchange.published(next.clone())?;
            if let Some(publication) = publication {
                publication.publish()?;
            }
            unknown.publish()?;
            Ok::<_, ErrorCode>(())
        })();
        if published.is_err() {
            self.proof.quarantine();
            return Err(self.unavailable(ErrorCode::AuthorityUnavailable));
        }
        self.schedule_obligation(due)?;
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
