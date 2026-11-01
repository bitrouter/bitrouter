//! Shared physical write estimates around the existing autocommit consumers.

use bitrouter_ai::tenancy::operation::ErrorCode;
use sea_orm::{ActiveModelTrait, ActiveValue, Iterable, Value};

use super::{MeteringStore, requests};
use crate::{
    gateway::error::{BitrouterError, Result},
    tenancy::storage::{Budget, Charge},
};

impl MeteringStore {
    fn write_budget(&self) -> Result<Option<&Budget>> {
        self.storage_budget
            .as_ref()
            .map(|cell| {
                cell.get()
                    .ok_or_else(|| BitrouterError::internal("metering storage budget not ready"))
            })
            .transpose()
    }

    pub(super) fn stored_bytes(&self, row: &requests::ActiveModel) -> Result<usize> {
        if self.write_budget()?.is_some() {
            model_bytes(row)
        } else {
            Ok(0)
        }
    }

    pub(super) fn reserve_write(
        &self,
        row: &requests::ActiveModel,
        previous_bytes: usize,
    ) -> Result<Option<Charge>> {
        let Some(budget) = self.write_budget()? else {
            return Ok(None);
        };
        let bytes = model_bytes(row)?
            .checked_add(previous_bytes)
            .ok_or_else(|| storage_error(ErrorCode::CapacityExceeded))?;
        // Metering and reconciliation do not own a transferred completion or
        // deny/revoke reservation, so they use ordinary storage admission.
        budget
            .reserve(bytes, false)
            .map(Some)
            .map_err(storage_error)
    }

    pub(super) async fn update_row(
        &self,
        row: requests::ActiveModel,
        previous_bytes: usize,
        context: &'static str,
    ) -> Result<()> {
        let mut charge = self.reserve_write(&row, previous_bytes)?;
        if let Some(charge) = &mut charge {
            charge.start();
        }
        // Keep the existing ORM update/affected-row semantics. Cancellation or
        // error drops the started charge, retaining accounting and fencing.
        row.update(&self.db)
            .await
            .map_err(|_| BitrouterError::internal(context))?;
        complete(charge)
    }
}

pub(super) fn complete(charge: Option<Charge>) -> Result<()> {
    if let Some(charge) = charge {
        charge.complete().map_err(storage_error)?;
    }
    Ok(())
}

fn storage_error(code: ErrorCode) -> BitrouterError {
    match code {
        ErrorCode::CapacityExceeded => BitrouterError::RateLimited { retry_after: None },
        _ => BitrouterError::internal("metering storage unavailable"),
    }
}

/// Count the actual prepared SQL field values, including serialized JSON and
/// unchanged fields that SQLite may rewrite with the row. Iterate the schema
/// without collecting another row; SeaORM materializes one field Value at a
/// time. No SQL literals, debug rendering, or payload diagnostics are produced.
/// The shared Budget supplies amplification and fixed page/index overhead.
fn model_bytes(row: &requests::ActiveModel) -> Result<usize> {
    requests::Column::iter().try_fold(0usize, |total, column| {
        let bytes = match row.get(column) {
            ActiveValue::NotSet => 0,
            ActiveValue::Set(value) | ActiveValue::Unchanged(value) => match value {
                Value::String(Some(value)) => value.len(),
                Value::String(None) => 1,
                // Use the wide integer width even for narrower persisted fields.
                Value::Int(_) | Value::BigInt(_) => 8,
                // A new column type must receive an explicit encoding estimate.
                _ => return Err(BitrouterError::internal("unsupported metering SQL value")),
            },
        };
        total
            .checked_add(bytes)
            .ok_or_else(|| storage_error(ErrorCode::CapacityExceeded))
    })
}

#[cfg(test)]
mod tests;
