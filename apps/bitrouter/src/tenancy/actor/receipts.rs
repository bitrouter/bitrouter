//! Receipt completions use the same writer and transaction proof as authority.
use super::{Control, ErrorCode, Failure, Worker};
use crate::tenancy::receipts::{owner::Owner, store};
use std::sync::Arc;

impl Worker {
    pub(super) async fn execute_receipt(
        &self,
        owner: &Arc<Owner>,
        control: &Control,
    ) -> Result<(), Failure> {
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
        let candidate = owner
            .prepare()
            .await
            .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
        #[cfg(test)]
        let observation = self.proof.receipt_started_for_test(&candidate);
        #[cfg(test)]
        use crate::tenancy::transaction::receipt_observation::Step;
        let checked = control.check_read();
        #[cfg(test)]
        self.proof
            .receipt_result_for_test(observation, Step::Check, &checked);
        checked.map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
        let beginning = control.begin(&self.db).await;
        #[cfg(test)]
        self.proof
            .receipt_result_for_test(observation, Step::Begin, &beginning);
        let transaction = beginning.map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
        let write = async {
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
                if rollback.is_err() {
                    self.proof.quarantine();
                }
                return Err(ErrorCode::StorageUnavailable.into());
            }
        };
        let commit = control.commit(transaction).await;
        #[cfg(test)]
        self.proof
            .receipt_result_for_test(observation, Step::Commit, &commit);
        commit.map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
        // Publish only after confirmed commit. A late acknowledgement still
        // releases exactly the original reserved completion, never another send.
        owner.committed(candidate, inserted).map_err(Failure::from)
    }
}

impl Worker {
    pub(super) async fn read_receipt(
        &self,
        read: &crate::tenancy::receipts::read::Read,
        control: &Control,
    ) -> Result<Vec<u8>, Failure> {
        use crate::tenancy::receipts::read::{Request, authorize};
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
            let bytes = match &read.request {
                Request::One(id) => {
                    let receipt = store::lookup(&transaction, &read.installation, &read.tenant, id)
                        .await
                        .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?
                        .ok_or_else(|| Failure::from(ErrorCode::NotFound))?;
                    serde_json::to_vec(&receipt)
                }
                Request::Page {
                    correlation,
                    after,
                    limit,
                } => {
                    let page = store::page(
                        &transaction,
                        &read.installation,
                        &read.tenant,
                        correlation,
                        after.as_ref(),
                        *limit,
                    )
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                    serde_json::to_vec(&page)
                }
            }
            .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
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
