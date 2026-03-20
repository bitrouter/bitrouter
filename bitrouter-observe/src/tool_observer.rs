//! [`ToolSpendObserver`] — a [`ToolObserveCallback`] that persists tool call
//! spend logs via a [`ToolSpendStore`].

use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use std::future::Future;
use std::pin::Pin;

use bitrouter_core::observe::{ToolCallEvent, ToolObserveCallback};

use crate::spend::tool_store::{ToolSpendLog, ToolSpendStore};

/// Observes completed tool calls and writes spend logs.
pub struct ToolSpendObserver {
    store: Arc<dyn ToolSpendStore>,
}

impl ToolSpendObserver {
    pub fn new(store: Arc<dyn ToolSpendStore>) -> Self {
        Self { store }
    }
}

impl ToolObserveCallback for ToolSpendObserver {
    fn on_tool_call(&self, event: ToolCallEvent) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let log = ToolSpendLog {
                id: Uuid::new_v4(),
                account_id: event.account_id,
                server: event.server,
                tool: event.tool,
                cost: event.cost,
                latency_ms: event.latency_ms,
                success: event.success,
                error_message: event.error_message,
                created_at: Utc::now().naive_utc(),
            };

            self.store.write(log).await;
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bitrouter_core::observe::ToolCallEvent;

    use crate::spend::tool_memory::InMemoryToolSpendStore;

    use super::*;

    #[tokio::test]
    async fn observer_writes_log() {
        let store = Arc::new(InMemoryToolSpendStore::new());
        let observer = ToolSpendObserver::new(store.clone());

        let event = ToolCallEvent {
            account_id: Some("acct-1".into()),
            server: "github".into(),
            tool: "search".into(),
            cost: 0.005,
            latency_ms: 200,
            success: true,
            error_message: None,
        };

        observer.on_tool_call(event).await;

        let logs = store.logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].server, "github");
        assert_eq!(logs[0].tool, "search");
        assert!(logs[0].success);
        assert!((logs[0].cost - 0.005).abs() < 1e-10);
    }

    #[tokio::test]
    async fn observer_records_failure() {
        let store = Arc::new(InMemoryToolSpendStore::new());
        let observer = ToolSpendObserver::new(store.clone());

        let event = ToolCallEvent {
            account_id: None,
            server: "github".into(),
            tool: "search".into(),
            cost: 0.0,
            latency_ms: 50,
            success: false,
            error_message: Some("timeout".into()),
        };

        observer.on_tool_call(event).await;

        let logs = store.logs();
        assert_eq!(logs.len(), 1);
        assert!(!logs[0].success);
        assert_eq!(logs[0].error_message.as_deref(), Some("timeout"));
    }
}
