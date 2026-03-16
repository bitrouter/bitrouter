//! In-memory spend log store for use when no database is configured.

use std::sync::RwLock;

use super::store::{SpendLog, SpendStore};

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
                account_id: Some("acct-1".into()),
                model: "gpt-4o".into(),
                provider: "openai".into(),
                input_tokens: 100,
                output_tokens: 50,
                cost: 0.005,
                latency_ms: 300,
                success: true,
                error_type: None,
                created_at: Utc::now().naive_utc(),
            })
            .await;

        let logs = store.logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].model, "gpt-4o");
        assert!(logs[0].success);
    }
}
