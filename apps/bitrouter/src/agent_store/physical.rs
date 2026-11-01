//! Physical custody for the existing native SQL transaction owners.

use anyhow::Result;
use sea_orm::{DatabaseTransaction, Statement, TransactionTrait};
use serde::Serialize;

use super::DatabaseExecutionStore;
use crate::tenancy::storage::{Budget, Charge, encoded_size, storage_health_error};

pub(super) struct Write<'a> {
    budget: Option<&'a Budget>,
    charge: Option<Charge>,
    prepaid: usize,
    #[cfg(test)]
    gate: Option<std::sync::Arc<Gate>>,
}

impl<'a> Write<'a> {
    #[cfg(test)]
    pub(super) fn unbudgeted() -> Self {
        Self {
            budget: None,
            charge: None,
            prepaid: 0,
            gate: None,
        }
    }

    pub(super) async fn begin(
        store: &'a DatabaseExecutionStore,
        maintenance: bool,
        prepare: impl FnOnce() -> Result<usize>,
    ) -> Result<(Self, DatabaseTransaction)> {
        // The disabled path must not even evaluate the accounting preparation.
        let budget = store.storage_budget.as_ref();
        let prepaid = if budget.is_some() { prepare()? } else { 0 };
        let charge = budget
            .map(|budget| budget.reserve(prepaid, maintenance).map_err(capacity))
            .transpose()?;
        let mut write = Self {
            budget,
            charge,
            prepaid,
            #[cfg(test)]
            gate: store.physical_gate.clone(),
        };
        if let Some(charge) = &mut write.charge {
            charge.start();
        }
        // A failed/cancelled BEGIN has no acknowledged transaction cleanup.
        // Keep the same conservative started-charge custody as other writers.
        let tx = store.db.begin().await?;
        Ok((write, tx))
    }

    pub(super) fn enabled(&self) -> bool {
        self.budget.is_some()
    }

    pub(super) fn account(&mut self, bytes: usize) -> Result<()> {
        if let Some(charge) = &mut self.charge {
            if let Some(remaining) = self.prepaid.checked_sub(bytes) {
                self.prepaid = remaining;
            } else {
                charge.grow(bytes - self.prepaid).map_err(capacity)?;
                self.prepaid = 0;
            }
        }
        Ok(())
    }

    pub(super) fn row(&mut self, value: &impl Serialize) -> Result<()> {
        if self.enabled() {
            self.account(encoded_size(value)?)?;
        }
        Ok(())
    }

    pub(super) fn statement(&mut self, prepare: impl FnOnce() -> Statement) -> Result<()> {
        if self.enabled() {
            self.account(crate::storage::write::statement_bytes(&prepare())?)?;
        }
        Ok(())
    }

    pub(super) async fn finish<T>(
        mut self,
        tx: DatabaseTransaction,
        result: Result<T>,
    ) -> Result<T> {
        if let Err(error) = &result {
            fence(self.budget, error);
        }
        #[cfg(test)]
        if let Some(gate) = &self.gate
            && gate.commit == result.is_ok()
        {
            gate.entered.notify_one();
            gate.release.acquire().await?.forget();
        }
        // Keep typed errors and the charge alive through the consuming ACK.
        // A failed COMMIT/ROLLBACK leaves started Drop to retain/quarantine.
        if result.is_ok() {
            tx.commit().await?;
        } else {
            tx.rollback().await?;
        }
        if let Some(charge) = self.charge.take() {
            charge.complete().map_err(capacity)?;
        }
        result
    }
}

fn capacity(error: bitrouter_ai::tenancy::operation::ErrorCode) -> anyhow::Error {
    anyhow::anyhow!("native storage reservation refused: {error:?}")
}

pub(super) fn fence(budget: Option<&Budget>, error: &anyhow::Error) {
    if let Some(budget) = budget
        && storage_health_error(error)
    {
        budget.fence();
    }
}

#[cfg(test)]
pub(super) struct Gate {
    pub commit: bool,
    pub entered: tokio::sync::Notify,
    pub release: tokio::sync::Semaphore,
}

#[cfg(test)]
impl Gate {
    #[cfg(all(unix, any(feature = "otel-http", feature = "otel-grpc")))]
    pub(super) fn new(commit: bool) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            commit,
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        })
    }
}

#[cfg(all(test, unix, any(feature = "otel-http", feature = "otel-grpc")))]
mod tests;
