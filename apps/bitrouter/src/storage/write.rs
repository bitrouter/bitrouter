//! Shared physical custody for existing local SQL write consumers.

use std::sync::{Arc, OnceLock};

use anyhow::Result;
use bitrouter_ai::tenancy::operation::ErrorCode;
use sea_orm::{DatabaseConnection, DatabaseTransaction, DbErr, TransactionTrait};
use serde::Serialize;

use crate::{
    gateway::error::BitrouterError,
    storage::{evolution::records, operation_budget::MAX_BYTES},
    tenancy::storage::{Budget, Charge, encoded_size},
};

pub(crate) struct Write {
    budget: Option<Budget>,
    charge: Option<Charge>,
    remaining: usize,
}

impl Write {
    pub(crate) async fn begin(
        db: &DatabaseConnection,
        cell: &Option<Arc<OnceLock<Budget>>>,
        maintenance: bool,
    ) -> Result<(Self, DatabaseTransaction)> {
        let write = Self::start(cell, maintenance)?;
        let tx = db
            .begin()
            .await
            .map_err(|_| BitrouterError::internal("local write transaction unavailable"))?;
        Ok((write, tx))
    }

    /// Reserve before the caller's existing backend-specific BEGIN await.
    pub(crate) fn start(cell: &Option<Arc<OnceLock<Budget>>>, maintenance: bool) -> Result<Self> {
        let budget = cell
            .as_ref()
            .map(|cell| {
                cell.get()
                    .cloned()
                    .ok_or_else(|| BitrouterError::internal("local storage budget not ready"))
            })
            .transpose()?;
        // These programs discover their write set under the existing locks.
        // Reuse the existing operation byte allowance, then count actual old
        // and new rows before each mutation. No directory/history count limit.
        let mut charge = budget
            .as_ref()
            .map(|budget| {
                budget
                    .reserve(MAX_BYTES, maintenance)
                    .map_err(capacity_error)
            })
            .transpose()?;
        if let Some(charge) = &mut charge {
            charge.start();
        }
        Ok(Self {
            budget,
            charge,
            remaining: MAX_BYTES,
        })
    }

    pub(crate) fn enabled(&self) -> bool {
        self.budget.is_some()
    }

    pub(crate) fn account(&mut self, value: &impl Serialize) -> Result<()> {
        if self.enabled() {
            let bytes = encoded_size(value)?;
            self.remaining = self
                .remaining
                .checked_sub(bytes)
                .ok_or_else(|| capacity_error(ErrorCode::CapacityExceeded))?;
        }
        Ok(())
    }

    pub(crate) fn replacement(
        &mut self,
        previous: &records::Model,
        body: &impl Serialize,
    ) -> Result<()> {
        self.account(&(
            &previous.record_id,
            &previous.scope_id,
            &previous.kind,
            &previous.session_key,
            previous.revision,
            &previous.body,
            body,
        ))
    }

    pub(crate) async fn session(&mut self, tx: &DatabaseTransaction, key: &str) -> Result<()> {
        use crate::storage::capture::sessions;
        use sea_orm::{EntityTrait, QueryFilter, QuerySelect};
        if self.enabled()
            && let Some(row) = sessions::Entity::find_by_id(key)
                .filter(crate::storage::legacy::scope("acp_canonical_sessions"))
                .lock_exclusive()
                .one(tx)
                .await?
        {
            self.account(&(&row, &row))?;
        }
        Ok(())
    }

    pub(crate) async fn connection(&mut self, tx: &DatabaseTransaction, id: &str) -> Result<()> {
        use crate::storage::capture::connections;
        use sea_orm::{EntityTrait, QueryFilter, QuerySelect};
        if self.enabled()
            && let Some(row) = connections::Entity::find_by_id(id)
                .filter(crate::storage::legacy::scope("acp_capture_connections"))
                .lock_exclusive()
                .one(tx)
                .await?
        {
            self.account(&(&row, &row))?;
        }
        Ok(())
    }

    /// Validation fixtures retain their read view and explicitly roll back the
    /// lock-only writes. Complete custody only after that rollback is acknowledged.
    #[cfg(all(test, feature = "core"))]
    pub(crate) async fn rollback(mut self, tx: DatabaseTransaction) -> Result<()> {
        tx.rollback()
            .await
            .map_err(|_| BitrouterError::internal("local write rollback unconfirmed"))?;
        if let Some(charge) = self.charge.take() {
            charge.complete().map_err(capacity_error)?;
        }
        Ok(())
    }

    pub(crate) async fn finish<T>(
        mut self,
        tx: DatabaseTransaction,
        result: Result<T>,
    ) -> Result<T> {
        let result = match result {
            Ok(value) => {
                tx.commit()
                    .await
                    .map_err(|_| BitrouterError::internal("local write commit unconfirmed"))?;
                Ok(value)
            }
            Err(error) => {
                let sql_failed = self.enabled() && error.downcast_ref::<DbErr>().is_some();
                if sql_failed && let Some(budget) = &self.budget {
                    budget.fence();
                }
                tx.rollback()
                    .await
                    .map_err(|_| BitrouterError::internal("local write rollback unconfirmed"))?;
                if sql_failed {
                    Err(BitrouterError::internal("local storage SQL failed").into())
                } else {
                    Err(error)
                }
            }
        };
        if let Some(charge) = self.charge.take() {
            charge.complete().map_err(capacity_error)?;
        }
        result
    }
}

/// Actual prepared SQL values, without rendering literals or copying payloads.
#[cfg(any(feature = "core", feature = "harness"))]
pub(crate) fn statement_bytes(statement: &sea_orm::Statement) -> Result<usize> {
    use sea_orm::Value;
    let mut bytes = statement.sql.len();
    if let Some(values) = &statement.values {
        for value in &values.0 {
            let size = match value {
                Value::String(Some(value)) => value.len(),
                Value::Bytes(Some(value)) => value.len(),
                Value::String(None) | Value::Bytes(None) | Value::Bool(_) => 1,
                Value::TinyInt(_)
                | Value::SmallInt(_)
                | Value::Int(_)
                | Value::BigInt(_)
                | Value::TinyUnsigned(_)
                | Value::SmallUnsigned(_)
                | Value::Unsigned(_)
                | Value::BigUnsigned(_)
                | Value::Float(_)
                | Value::Double(_) => 8,
                _ => return Err(BitrouterError::internal("unsupported local SQL value").into()),
            };
            bytes = bytes
                .checked_add(size)
                .ok_or_else(|| capacity_error(ErrorCode::CapacityExceeded))?;
        }
    }
    Ok(bytes)
}

pub(crate) fn read_error(
    cell: &Option<Arc<OnceLock<Budget>>>,
    error: anyhow::Error,
) -> anyhow::Error {
    if error.downcast_ref::<DbErr>().is_some()
        && let Some(budget) = cell.as_ref().and_then(|cell| cell.get())
    {
        budget.fence();
        BitrouterError::internal("local storage SQL failed").into()
    } else {
        error
    }
}

fn capacity_error(code: ErrorCode) -> BitrouterError {
    match code {
        ErrorCode::CapacityExceeded => BitrouterError::RateLimited { retry_after: None },
        _ => BitrouterError::internal("local storage unavailable"),
    }
}

#[cfg(test)]
mod tests;

#[cfg(all(test, unix, any(feature = "otel-http", feature = "otel-grpc")))]
pub(crate) mod measurement;

#[cfg(all(
    test,
    unix,
    feature = "core",
    any(feature = "otel-http", feature = "otel-grpc")
))]
mod calibration;
