//! SeaORM-backed spend log store for persistent storage.

use chrono::NaiveDateTime;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QuerySelect, Set,
};

use crate::entity::spend_log;

use super::store::{ServiceType, SpendLog, SpendStore};

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
                service_type: Set(log.service_type.to_string()),
                account_id: Set(log.account_id),
                key_id: Set(log.key_id),
                session_id: Set(log.session_id),
                service_name: Set(log.service_name),
                operation: Set(log.operation),
                input_tokens: Set(input_tokens),
                output_tokens: Set(output_tokens),
                cost: Set(log.cost),
                latency_ms: Set(latency_ms),
                success: Set(log.success),
                error_info: Set(log.error_info),
                created_at: Set(log.created_at),
            };

            if let Err(e) = active.insert(&self.db).await {
                tracing::warn!(error = %e, "failed to write spend log");
            }
        })
    }

    fn query_total_spend(
        &self,
        account_id: &str,
        since: Option<NaiveDateTime>,
        service_type: Option<ServiceType>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = f64> + Send + '_>> {
        let account_id = account_id.to_owned();
        Box::pin(async move {
            let mut query = spend_log::Entity::find()
                .filter(spend_log::Column::AccountId.eq(&account_id))
                .filter(spend_log::Column::Success.eq(true));

            if let Some(since) = since {
                query = query.filter(spend_log::Column::CreatedAt.gte(since));
            }

            if let Some(st) = service_type {
                query = query.filter(spend_log::Column::ServiceType.eq(st.to_string()));
            }

            let result = query
                .select_only()
                .column_as(spend_log::Column::Cost.sum(), "total")
                .into_tuple::<Option<f64>>()
                .one(&self.db)
                .await;

            match result {
                Ok(Some(total)) => total.unwrap_or(0.0),
                Ok(None) => 0.0,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to query total spend");
                    0.0
                }
            }
        })
    }
}
