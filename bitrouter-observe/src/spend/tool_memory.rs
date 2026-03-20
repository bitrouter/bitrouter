//! In-memory tool spend log store for use when no database is configured.

use std::sync::RwLock;

use chrono::NaiveDateTime;

use super::tool_store::{ToolSpendLog, ToolSpendStore};

/// An in-memory [`ToolSpendStore`] backed by a `Vec`.
///
/// Useful for development, testing, or deployments without a database.
pub struct InMemoryToolSpendStore {
    logs: RwLock<Vec<ToolSpendLog>>,
}

impl InMemoryToolSpendStore {
    pub fn new() -> Self {
        Self {
            logs: RwLock::new(Vec::new()),
        }
    }

    /// Returns a snapshot of all stored logs.
    pub fn logs(&self) -> Vec<ToolSpendLog> {
        self.logs.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Default for InMemoryToolSpendStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolSpendStore for InMemoryToolSpendStore {
    fn write(
        &self,
        log: ToolSpendLog,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let mut guard = self.logs.write().unwrap_or_else(|e| e.into_inner());
        guard.push(log);
        Box::pin(async {})
    }

    fn query_tool_spend(
        &self,
        account_id: &str,
        since: Option<NaiveDateTime>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = f64> + Send + '_>> {
        let guard = self.logs.read().unwrap_or_else(|e| e.into_inner());
        let total: f64 = guard
            .iter()
            .filter(|log| {
                log.success
                    && log.account_id.as_deref() == Some(account_id)
                    && since.is_none_or(|s| log.created_at >= s)
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
    async fn write_and_query() {
        let store = InMemoryToolSpendStore::new();
        assert!(store.logs().is_empty());

        store
            .write(ToolSpendLog {
                id: Uuid::new_v4(),
                account_id: Some("acct-1".into()),
                server: "github".into(),
                tool: "search".into(),
                cost: 0.005,
                latency_ms: 200,
                success: true,
                error_message: None,
                created_at: Utc::now().naive_utc(),
            })
            .await;

        store
            .write(ToolSpendLog {
                id: Uuid::new_v4(),
                account_id: Some("acct-1".into()),
                server: "github".into(),
                tool: "get_file".into(),
                cost: 0.002,
                latency_ms: 150,
                success: true,
                error_message: None,
                created_at: Utc::now().naive_utc(),
            })
            .await;

        let logs = store.logs();
        assert_eq!(logs.len(), 2);

        let total = store.query_tool_spend("acct-1", None).await;
        assert!((total - 0.007).abs() < 1e-10);
    }

    #[tokio::test]
    async fn query_filters_by_account() {
        let store = InMemoryToolSpendStore::new();

        store
            .write(ToolSpendLog {
                id: Uuid::new_v4(),
                account_id: Some("acct-1".into()),
                server: "github".into(),
                tool: "search".into(),
                cost: 0.005,
                latency_ms: 200,
                success: true,
                error_message: None,
                created_at: Utc::now().naive_utc(),
            })
            .await;

        store
            .write(ToolSpendLog {
                id: Uuid::new_v4(),
                account_id: Some("acct-2".into()),
                server: "github".into(),
                tool: "search".into(),
                cost: 0.010,
                latency_ms: 200,
                success: true,
                error_message: None,
                created_at: Utc::now().naive_utc(),
            })
            .await;

        let total = store.query_tool_spend("acct-1", None).await;
        assert!((total - 0.005).abs() < 1e-10);
    }

    #[tokio::test]
    async fn failed_calls_excluded_from_spend() {
        let store = InMemoryToolSpendStore::new();

        store
            .write(ToolSpendLog {
                id: Uuid::new_v4(),
                account_id: Some("acct-1".into()),
                server: "github".into(),
                tool: "search".into(),
                cost: 0.005,
                latency_ms: 200,
                success: false,
                error_message: Some("upstream error".into()),
                created_at: Utc::now().naive_utc(),
            })
            .await;

        let total = store.query_tool_spend("acct-1", None).await;
        assert_eq!(total, 0.0);
    }
}
