//! Receipt completions use the same writer and transaction proof as authority.
use super::{Control, ErrorCode, Failure, Worker};
use crate::telemetry::receipt_latency::{Event, Phase, ResultKind};
use crate::tenancy::receipts::{owner::Owner, store};
use std::sync::Arc;

impl Worker {
    pub(super) async fn execute_receipt(
        &self,
        owner: &Arc<Owner>,
        control: &Control,
    ) -> Result<(), Failure> {
        owner.observation.entered();
        // Origin is a private writer identity, independent of mutable caller grants.
        // A revoke stops sends, never the cleanup of already admitted work.
        if !Arc::ptr_eq(&owner.origin, &self.proof) {
            return Err(ErrorCode::Forbidden.into());
        }
        let installation = self
            .snapshot
            .read()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?
            .installation
            .clone();
        if owner.installation() != &installation {
            return Err(ErrorCode::Forbidden.into());
        }
        let preparation = owner.observation.span(Phase::Prepare);
        let prepared = owner.prepare().await;
        preparation.finish(if prepared.is_ok() {
            ResultKind::Observed
        } else {
            ResultKind::Failed
        });
        let candidate = prepared.map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
        #[cfg(test)]
        let observation = self.proof.receipt_started_for_test(&candidate);
        #[cfg(test)]
        use crate::tenancy::transaction::receipt_observation::Step;
        let checked = control.check_read();
        #[cfg(test)]
        self.proof
            .receipt_result_for_test(observation, Step::Check, &checked);
        checked.map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
        let transaction_span = owner.observation.span(Phase::Transaction);
        let beginning = control.begin(&self.db).await;
        #[cfg(test)]
        self.proof
            .receipt_result_for_test(observation, Step::Begin, &beginning);
        let transaction = match beginning {
            Ok(transaction) => transaction,
            Err(_) => {
                transaction_span.finish(
                    if self.proof.disposition()
                        == crate::tenancy::transaction::Disposition::NoTransaction
                    {
                        ResultKind::Failed
                    } else {
                        ResultKind::Unconfirmed
                    },
                );
                return Err(ErrorCode::StorageUnavailable.into());
            }
        };
        #[cfg(all(feature = "core", feature = "harness"))]
        let checkpoint = || {
            control
                .check_read()
                .map_err(|_| ErrorCode::AuthorityUnavailable)
        };
        #[cfg(all(feature = "core", feature = "harness"))]
        let budget = crate::storage::operation_budget::OperationBudget::new(&checkpoint);
        let write = async {
            #[cfg(all(feature = "core", feature = "harness"))]
            store::reserve_experiment_write(&budget)?;
            let checked = control.check_read();
            #[cfg(test)]
            self.proof
                .receipt_result_for_test(observation, Step::Check, &checked);
            checked?;
            let insertion = store::insert(&transaction, &candidate).await;
            #[cfg(test)]
            {
                self.proof
                    .receipt_result_for_test(observation, Step::Store, &insertion);
                if let Ok(inserted) = &insertion {
                    self.proof.receipt_inserted_for_test(observation, *inserted);
                }
            }
            let inserted = insertion?;
            crate::tenancy::receipts::auxiliary_store::insert(&transaction, &candidate).await?;
            #[cfg(all(feature = "core", feature = "harness"))]
            crate::tenancy::experiment::store::terminal::receipt(
                &transaction,
                owner,
                &candidate,
                &budget,
            )
            .await?;
            let checked = control.check_read();
            #[cfg(test)]
            self.proof
                .receipt_result_for_test(observation, Step::Check, &checked);
            checked?;
            Ok::<_, anyhow::Error>(inserted)
        }
        .await;
        let inserted = match write {
            Ok(inserted) => inserted,
            Err(_) => {
                let rollback = control.rollback(transaction).await;
                #[cfg(test)]
                self.proof
                    .receipt_result_for_test(observation, Step::Rollback, &rollback);
                transaction_span.finish(if rollback.is_ok() {
                    ResultKind::Failed
                } else {
                    ResultKind::Unconfirmed
                });
                if rollback.is_ok() {
                    owner.observation.event(Event::RollbackAck);
                }
                if rollback.is_err() {
                    self.proof.quarantine();
                }
                return Err(ErrorCode::StorageUnavailable.into());
            }
        };
        let commit = control
            .commit_observed(transaction, Some(&owner.observation))
            .await;
        transaction_span.finish(if commit.is_ok() {
            ResultKind::Acknowledged
        } else {
            ResultKind::Unconfirmed
        });
        #[cfg(test)]
        self.proof
            .receipt_result_for_test(observation, Step::Commit, &commit);
        commit.map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
        // Publish only after confirmed commit. A late acknowledgement still
        // releases exactly the original reserved completion, never another send.
        let publication = owner.committed(candidate, inserted);
        if publication.is_err() {
            owner.observation.event(Event::PublicationFailure);
        }
        publication.map_err(Failure::from)
    }
}

impl Worker {
    pub(super) async fn read_receipt(
        &self,
        read: &crate::tenancy::receipts::read::Read,
        control: &Control,
    ) -> Result<Vec<u8>, Failure> {
        use crate::tenancy::receipts::read::authorize;
        let check = || {
            control
                .check_read()
                .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
            let state = self.current()?;
            if state.installation != read.installation {
                return Err(ErrorCode::CapabilityMismatch.into());
            }
            authorize(&state, &read.caller, &read.tenant).map_err(Failure::from)
        };
        check()?;
        let transaction = control
            .begin_repeatable_read(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let body = async {
            check()?;
            let bytes = crate::tenancy::receipts::read::body(
                &transaction,
                &read.installation,
                &read.tenant,
                &read.request,
                || self.unavailable(ErrorCode::StorageUnavailable),
            )
            .await?;
            check()?;
            Ok(bytes)
        }
        .await;
        // Read-only transactions also stay in actor custody through rollback.
        control
            .rollback(transaction)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        check()?;
        body
    }
}
