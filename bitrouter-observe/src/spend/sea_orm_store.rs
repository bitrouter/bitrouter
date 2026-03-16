//! SeaORM-backed spend log store for persistent storage.

use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};

use crate::entity::spend_log;

use super::store::{SpendLog, SpendStore};

/// A [`SpendStore`] backed by a SeaORM database connection.
///
/// On write errors, a warning is logged and the error is swallowed — spend
/// logging must never break request serving.
pub struct SeaOrmSpendStore {
    db: DatabaseConnection,
}

impl SeaOrmSpendStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

impl SpendStore for SeaOrmSpendStore {
    fn write(
        &self,
        log: SpendLog,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let input_tokens = i32::try_from(log.input_tokens).unwrap_or_else(|_| {
                tracing::warn!(
                    value = log.input_tokens,
                    "input_tokens overflows i32, clamping to i32::MAX"
                );
                i32::MAX
            });
            let output_tokens = i32::try_from(log.output_tokens).unwrap_or_else(|_| {
                tracing::warn!(
                    value = log.output_tokens,
                    "output_tokens overflows i32, clamping to i32::MAX"
                );
                i32::MAX
            });
            let latency_ms = i64::try_from(log.latency_ms).unwrap_or_else(|_| {
                tracing::warn!(
                    value = log.latency_ms,
                    "latency_ms overflows i64, clamping to i64::MAX"
                );
                i64::MAX
            });

            let active = spend_log::ActiveModel {
                id: Set(log.id),
                account_id: Set(log.account_id),
                model: Set(log.model),
                provider: Set(log.provider),
                input_tokens: Set(input_tokens),
                output_tokens: Set(output_tokens),
                cost: Set(log.cost),
                latency_ms: Set(latency_ms),
                success: Set(log.success),
                error_type: Set(log.error_type),
                created_at: Set(log.created_at),
            };

            if let Err(e) = active.insert(&self.db).await {
                tracing::warn!(error = %e, "failed to write spend log");
            }
        })
    }
}
