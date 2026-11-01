//! One historical payload per maintenance turn under the existing writer.

use super::{Control, ErrorCode, Failure, Worker};
use crate::storage::operation_budget::OperationBudget;
use crate::tenancy::receipts::store::{archive, obligations};

#[cfg(test)]
mod tests;

#[cfg(test)]
impl crate::tenancy::Service {
    pub(in crate::tenancy) async fn archive_history_for_test(&self) -> Result<bool, Failure> {
        let (reply, receive) = tokio::sync::oneshot::channel();
        self.commands
            .try_send(super::Job::Archive {
                deadline: None,
                reply,
            })
            .map_err(|_| ErrorCode::CapacityExceeded)?;
        receive.await.map_err(|_| ErrorCode::AuthorityUnavailable)?
    }
}

pub(super) struct Progress {
    pub after: Option<obligations::Key>,
    pub done: bool,
    pub archived: bool,
}

impl Worker {
    pub(super) async fn archive_history(
        &self,
        after: Option<&obligations::Key>,
        control: &Control,
    ) -> Result<Progress, Failure> {
        let installation = self.current()?.installation.clone();
        let check = || {
            control
                .check_read()
                .map_err(|_| ErrorCode::CapacityExceeded)
        };
        let budget = OperationBudget::new(&check);
        let charge = |rows, bytes| budget.charge(rows, bytes).map_err(anyhow::Error::from);
        let tx = control
            .begin(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let selected = archive::next_candidate(&tx, &installation, after, &charge).await;
        control
            .rollback(tx)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let selected = selected.map_err(|error| self.maintenance_failure(error))?;
        let mut progress = Progress {
            after: selected.after,
            done: selected.done,
            archived: false,
        };
        let Some(candidate) = selected.candidate else {
            return Ok(progress);
        };
        let key = obligations::Key {
            tenant: candidate.receipt.tenant_id.clone(),
            execution: candidate.receipt.execution_id.clone(),
        };
        if self.admission.has_live_execution(&key)? {
            return Ok(progress);
        }
        // Candidate decoding and compression coexist and share one working-set
        // allowance. Compression holds no authority publication lock.
        let prepared = archive::Prepared::new(&candidate, &charge)
            .map_err(|error| self.maintenance_failure(error))?;
        drop(candidate);
        drop(budget);
        let encoded = prepared
            .stored_bytes()
            .checked_add(prepared.relocated_bytes())
            .and_then(|n| n.checked_add(16 * 1024))
            .ok_or(ErrorCode::CapacityExceeded)?;
        control.reserve_write(encoded, true)?;
        let tx = control
            .begin(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let budget = OperationBudget::new(&check);
        // The prepared payload remains live while the canonical row is re-read.
        let result = async {
            budget.charge(0, prepared.stored_bytes())?;
            let charge = |rows, bytes| budget.charge(rows, bytes).map_err(anyhow::Error::from);
            if self
                .admission
                .has_live_execution(&key)
                .map_err(|code| anyhow::anyhow!("archive ownership unavailable: {code:?}"))?
                || obligations::lookup(&tx, &installation, &key, &charge)
                    .await?
                    .is_some()
            {
                return Ok(false);
            }
            archive::archive_one(&tx, &prepared, &charge).await
        }
        .await;
        match result {
            Ok(true) => {
                control
                    .commit(tx)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                // No credit for predicted compression, freelist pages or an
                // unacknowledged transaction. This is only the live-class debit.
                self.admission
                    .archive_receipt(prepared.source_bytes(), prepared.history_bytes())?;
                progress.archived = true;
            }
            result => {
                control
                    .rollback(tx)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                result.map_err(|error| self.maintenance_failure(error))?;
            }
        }
        Ok(progress)
    }
}
