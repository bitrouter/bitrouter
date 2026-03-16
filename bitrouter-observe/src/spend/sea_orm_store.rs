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
            let active = spend_log::ActiveModel {
                id: Set(log.id),
                account_id: Set(log.account_id),
                model: Set(log.model),
                provider: Set(log.provider),
                input_tokens: Set(log.input_tokens as i32),
                output_tokens: Set(log.output_tokens as i32),
                cost: Set(log.cost),
                latency_ms: Set(log.latency_ms as i64),
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
