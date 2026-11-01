//! Indexed cold counts and bounded due work share the existing authority actor.

use super::{Control, ErrorCode, Failure, UnknownCount, Worker};
use crate::tenancy::{admission::unknown::CountPin, receipts::store::obligations};

#[cfg(test)]
mod tests;

impl Worker {
    pub(super) fn maintenance_failure(&self, error: anyhow::Error) -> Failure {
        use crate::storage::operation_budget::Error;
        // Classify the operation's cause, not the current watchdog flag: a
        // concurrent timeout must not hide a real SQL, codec or budget failure.
        if error.is::<crate::tenancy::transaction::ReadExpired>()
            || matches!(
                error.downcast_ref::<Error>(),
                Some(Error::Capacity | Error::Rejected(ErrorCode::CapacityExceeded))
            )
        {
            ErrorCode::CapacityExceeded.into()
        } else {
            self.unavailable(ErrorCode::StorageUnavailable)
        }
    }

    pub(super) fn obligation_deadline(&self) -> Result<Option<tokio::time::Instant>, Failure> {
        let due = *self
            .obligation_due
            .lock()
            .map_err(|_| self.unavailable(ErrorCode::AuthorityUnavailable))?;
        due.map(|due| {
            tokio::time::Instant::now()
                .checked_add((due - chrono::Utc::now()).to_std().unwrap_or_default())
                .ok_or_else(|| self.unavailable(ErrorCode::StorageUnavailable))
        })
        .transpose()
    }

    pub(super) fn schedule_obligation(
        &self,
        due: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<(), Failure> {
        if let Some(due) = due {
            let mut next = self
                .obligation_due
                .lock()
                .map_err(|_| self.unavailable(ErrorCode::AuthorityUnavailable))?;
            *next = Some(next.map_or(due, |previous| previous.min(due)));
        }
        Ok(())
    }

    pub(super) async fn initialize_obligation_schedule(
        &self,
        control: &Control,
    ) -> Result<(), Failure> {
        let installation = self.current()?.installation.clone();
        let tx = control
            .begin(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let due = obligations::next_due(&tx, &installation, &|_, _| control.check_read()).await;
        control
            .rollback(tx)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        *self
            .obligation_due
            .lock()
            .map_err(|_| self.unavailable(ErrorCode::AuthorityUnavailable))? =
            due.map_err(|error| self.maintenance_failure(error))?;
        Ok(())
    }

    pub(super) async fn escalate_obligations(
        &self,
        now: chrono::DateTime<chrono::Utc>,
        control: &Control,
    ) -> Result<usize, Failure> {
        let installation = self.current()?.installation.clone();
        control.reserve_write(32 * 4096, true)?;
        let tx = control
            .begin(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let result = async {
            let rows = obligations::due_page(&tx, &installation, now, &|_, _| control.check_read())
                .await?;
            for row in &rows {
                obligations::mark_escalated(&tx, &installation, row, &|_, _| control.check_read())
                    .await?;
            }
            let next =
                obligations::next_due(&tx, &installation, &|_, _| control.check_read()).await?;
            Ok::<_, anyhow::Error>((rows, next))
        }
        .await;
        let (rows, next) = match result {
            Ok(value) => value,
            Err(error) => {
                control
                    .rollback(tx)
                    .await
                    .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
                return Err(self.maintenance_failure(error));
            }
        };
        if rows.is_empty() {
            control.rollback(tx).await
        } else {
            control.commit(tx).await
        }
        .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        *self
            .obligation_due
            .lock()
            .map_err(|_| self.unavailable(ErrorCode::AuthorityUnavailable))? = next;
        for row in &rows {
            tracing::warn!(tenant = %row.key.tenant, execution = %row.key.execution,
                "unresolved execution obligation requires operator attention");
        }
        Ok(rows.len())
    }

    pub(super) async fn read_unknown_count(
        &self,
        job: &UnknownCount,
        control: &Control,
    ) -> Result<CountPin, Failure> {
        // Refuse global unknown pressure before COUNT, including on a restart
        // with retained obligations above newly lowered operator budgets.
        let read = self.admission.unknown_read(false)?;
        let installation = self.current()?.installation.clone();
        let tx = control
            .begin(&self.db)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let count = obligations::tenant_count(&tx, &installation, &job.tenant, &|_, _| {
            control.check_read()
        })
        .await;
        control
            .rollback(tx)
            .await
            .map_err(|_| self.unavailable(ErrorCode::StorageUnavailable))?;
        let count = count.map_err(|error| self.maintenance_failure(error))?;
        control
            .check_read()
            .map_err(|_| ErrorCode::CapacityExceeded)?;
        self.admission
            .seed_unknown(&read, &job.tenant, count)
            .map_err(Failure::from)
    }
}

impl crate::tenancy::Service {
    #[cfg(test)]
    pub(in crate::tenancy) fn next_obligation_for_test(
        &self,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>, Failure> {
        self.obligation_due
            .lock()
            .map(|next| *next)
            .map_err(|_| ErrorCode::AuthorityUnavailable.into())
    }

    #[cfg(test)]
    pub(in crate::tenancy) async fn escalate_obligations_for_test(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<usize, Failure> {
        let (reply, receive) = tokio::sync::oneshot::channel();
        self.commands
            .try_send(super::Job::Escalate {
                now,
                deadline: None,
                reply,
            })
            .map_err(|_| ErrorCode::CapacityExceeded)?;
        receive.await.map_err(|_| ErrorCode::AuthorityUnavailable)?
    }

    pub(in crate::tenancy) async fn pin_unknown_count(
        &self,
        tenant: &bitrouter_ai::tenancy::identity::TenantId,
        deadline: tokio::time::Instant,
    ) -> Result<CountPin, Failure> {
        if let Some(pin) = self.admission.pin_unknown(tenant, false)? {
            return Ok(pin);
        }
        let permit = self
            .reads
            .clone()
            .try_acquire_owned()
            .map_err(|_| ErrorCode::CapacityExceeded)?;
        let (reply, receive) = tokio::sync::oneshot::channel();
        self.commands
            .try_send(super::Job::UnknownCount(UnknownCount {
                tenant: tenant.clone(),
                deadline,
                permit,
                reply,
            }))
            .map_err(|_| ErrorCode::CapacityExceeded)?;
        tokio::time::timeout_at(deadline, receive)
            .await
            .map_err(|_| ErrorCode::CapacityExceeded)?
            .map_err(|_| ErrorCode::AuthorityUnavailable)?
    }
}
