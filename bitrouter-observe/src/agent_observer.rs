//! [`AgentSpendObserver`] — an [`AgentObserveCallback`] that persists agent
//! call spend logs via a [`SpendStore`].

use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use std::future::Future;
use std::pin::Pin;

use bitrouter_core::observe::{AgentCallEvent, AgentObserveCallback};

use crate::spend::store::{ServiceType, SpendLog, SpendStore};

/// Observes completed A2A agent calls and writes spend logs.
pub struct AgentSpendObserver {
    store: Arc<dyn SpendStore>,
}

impl AgentSpendObserver {
    pub fn new(store: Arc<dyn SpendStore>) -> Self {
        Self { store }
    }
}

impl AgentObserveCallback for AgentSpendObserver {
    fn on_agent_call(
        &self,
        event: AgentCallEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let log = SpendLog {
                id: Uuid::new_v4(),
                service_type: ServiceType::Agent,
                account_id: event.account_id,
                session_id: None,
                service_name: event.agent,
                operation: event.method,
                input_tokens: 0,
                output_tokens: 0,
                cost: event.cost,
                latency_ms: event.latency_ms,
                success: event.success,
                error_info: event.error_message,
                created_at: Utc::now().naive_utc(),
            };

            self.store.write(log).await;
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bitrouter_core::observe::AgentCallEvent;

    use crate::spend::memory::InMemorySpendStore;
    use crate::spend::store::ServiceType;

    use super::*;

    #[tokio::test]
    async fn observer_writes_agent_log() {
        let store = Arc::new(InMemorySpendStore::new());
        let observer = AgentSpendObserver::new(store.clone());

        let event = AgentCallEvent {
            account_id: Some("acct-1".into()),
            agent: "upstream-agent".into(),
            method: "message/send".into(),
            cost: 0.01,
            latency_ms: 500,
            success: true,
            error_message: None,
        };

        observer.on_agent_call(event).await;

        let logs = store.logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].service_type, ServiceType::Agent);
        assert_eq!(logs[0].service_name, "upstream-agent");
        assert_eq!(logs[0].operation, "message/send");
        assert!(logs[0].success);
        assert!((logs[0].cost - 0.01).abs() < 1e-10);
    }

    #[tokio::test]
    async fn observer_records_agent_failure() {
        let store = Arc::new(InMemorySpendStore::new());
        let observer = AgentSpendObserver::new(store.clone());

        let event = AgentCallEvent {
            account_id: None,
            agent: "upstream-agent".into(),
            method: "tasks/get".into(),
            cost: 0.0,
            latency_ms: 100,
            success: false,
            error_message: Some("upstream timeout".into()),
        };

        observer.on_agent_call(event).await;

        let logs = store.logs();
        assert_eq!(logs.len(), 1);
        assert!(!logs[0].success);
        assert_eq!(logs[0].error_info.as_deref(), Some("upstream timeout"));
    }
}
