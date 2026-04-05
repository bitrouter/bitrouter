//! In-memory spend log store for use when no database is configured.

use std::sync::RwLock;

use chrono::NaiveDateTime;

use super::store::{ServiceType, SpendLog, SpendStore};

/// An in-memory [`SpendStore`] backed by a `Vec`.
///
/// Useful for development, testing, or deployments without a database.
pub struct InMemorySpendStore {
    logs: RwLock<Vec<SpendLog>>,
}

impl InMemorySpendStore {
    pub fn new() -> Self {
        Self {
            logs: RwLock::new(Vec::new()),
        }
    }

    /// Returns a snapshot of all stored logs.
    pub fn logs(&self) -> Vec<SpendLog> {
        self.logs.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Default for InMemorySpendStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SpendStore for InMemorySpendStore {
    fn write(
        &self,
        log: SpendLog,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let mut guard = self.logs.write().unwrap_or_else(|e| e.into_inner());
        guard.push(log);
        Box::pin(async {})
    }

    fn query_total_spend(
        &self,
        account_id: &str,
        since: Option<NaiveDateTime>,
        service_type: Option<ServiceType>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = f64> + Send + '_>> {
        let guard = self.logs.read().unwrap_or_else(|e| e.into_inner());
        let total: f64 = guard
            .iter()
            .filter(|log| {
                log.success
                    && log.account_id.as_deref() == Some(account_id)
                    && since.is_none_or(|s| log.created_at >= s)
                    && service_type.is_none_or(|st| log.service_type == st)
            })
            .map(|log| log.cost)
            .sum();
        Box::pin(async move { total })
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use uuid::Uuid;

    use super::*;

    #[tokio::test]
    async fn write_and_read_logs() {
        let store = InMemorySpendStore::new();
        assert!(store.logs().is_empty());

        store
            .write(SpendLog {
                id: Uuid::new_v4(),
                service_type: ServiceType::Model,
                account_id: Some("acct-1".into()),
                key_id: None,
                session_id: None,
                service_name: "fast".into(),
                operation: "openai:gpt-4o".into(),
                input_tokens: 100,
                output_tokens: 50,
                cost: 0.005,
                latency_ms: 300,
                success: true,
                error_info: None,
                created_at: Utc::now().naive_utc(),
            })
            .await;

        let logs = store.logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].operation, "openai:gpt-4o");
        assert!(logs[0].success);
    }

    #[tokio::test]
    async fn query_filters_by_service_type() {
        let store = InMemorySpendStore::new();

        store
            .write(SpendLog {
                id: Uuid::new_v4(),
                service_type: ServiceType::Model,
                account_id: Some("acct-1".into()),
                key_id: None,
                session_id: None,
                service_name: "fast".into(),
                operation: "openai:gpt-4o".into(),
                input_tokens: 100,
                output_tokens: 50,
                cost: 0.005,
                latency_ms: 300,
                success: true,
                error_info: None,
                created_at: Utc::now().naive_utc(),
            })
            .await;

        store
            .write(SpendLog {
                id: Uuid::new_v4(),
                service_type: ServiceType::Tool,
                account_id: Some("acct-1".into()),
                key_id: None,
                session_id: None,
                service_name: "github".into(),
                operation: "search".into(),
                input_tokens: 0,
                output_tokens: 0,
                cost: 0.002,
                latency_ms: 200,
                success: true,
                error_info: None,
                created_at: Utc::now().naive_utc(),
            })
            .await;

        // All types
        let total = store.query_total_spend("acct-1", None, None).await;
        assert!((total - 0.007).abs() < 1e-10);

        // Model only
        let model_total = store
            .query_total_spend("acct-1", None, Some(ServiceType::Model))
            .await;
        assert!((model_total - 0.005).abs() < 1e-10);

        // Tool only
        let tool_total = store
            .query_total_spend("acct-1", None, Some(ServiceType::Tool))
            .await;
        assert!((tool_total - 0.002).abs() < 1e-10);

        // Agent (none)
        let agent_total = store
            .query_total_spend("acct-1", None, Some(ServiceType::Agent))
            .await;
        assert_eq!(agent_total, 0.0);
    }
}
