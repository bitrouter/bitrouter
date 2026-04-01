//! [`ToolSpendObserver`] — a [`ToolObserveCallback`] that persists tool call
//! spend logs via a [`SpendStore`].

use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use std::future::Future;
use std::pin::Pin;

use bitrouter_core::observe::{ToolCallFailureEvent, ToolCallSuccessEvent, ToolObserveCallback};

use crate::spend::store::{ServiceType, SpendLog, SpendStore};

/// Cost lookup function: `(provider_name, operation) -> cost_usd`.
pub type ToolCostFn = Arc<dyn Fn(&str, &str) -> f64 + Send + Sync>;

/// Observes completed tool calls and writes spend logs.
pub struct ToolSpendObserver {
    store: Arc<dyn SpendStore>,
    cost_fn: ToolCostFn,
}

impl ToolSpendObserver {
    pub fn new(store: Arc<dyn SpendStore>, cost_fn: ToolCostFn) -> Self {
        Self { store, cost_fn }
    }
}

impl ToolObserveCallback for ToolSpendObserver {
    fn on_tool_call_success(
        &self,
        event: ToolCallSuccessEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let cost = (self.cost_fn)(&event.ctx.provider, &event.ctx.operation);
            let log = SpendLog {
                id: Uuid::new_v4(),
                service_type: ServiceType::Tool,
                account_id: event.ctx.caller.account_id,
                session_id: None,
                service_name: event.ctx.provider,
                operation: event.ctx.operation,
                input_tokens: 0,
                output_tokens: 0,
                cost,
                latency_ms: event.ctx.latency_ms,
                success: true,
                error_info: None,
                created_at: Utc::now().naive_utc(),
            };

            self.store.write(log).await;
        })
    }

    fn on_tool_call_failure(
        &self,
        event: ToolCallFailureEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let cost = (self.cost_fn)(&event.ctx.provider, &event.ctx.operation);
            let log = SpendLog {
                id: Uuid::new_v4(),
                service_type: ServiceType::Tool,
                account_id: event.ctx.caller.account_id,
                session_id: None,
                service_name: event.ctx.provider,
                operation: event.ctx.operation,
                input_tokens: 0,
                output_tokens: 0,
                cost,
                latency_ms: event.ctx.latency_ms,
                success: false,
                error_info: Some(event.error),
                created_at: Utc::now().naive_utc(),
            };

            self.store.write(log).await;
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bitrouter_core::observe::{
        CallerContext, ToolCallFailureEvent, ToolCallSuccessEvent, ToolRequestContext,
    };

    use crate::spend::memory::InMemorySpendStore;
    use crate::spend::store::ServiceType;

    use super::*;

    fn zero_cost() -> ToolCostFn {
        Arc::new(|_, _| 0.0)
    }

    fn fixed_cost(c: f64) -> ToolCostFn {
        Arc::new(move |_, _| c)
    }

    #[tokio::test]
    async fn observer_writes_log() {
        let store = Arc::new(InMemorySpendStore::new());
        let observer = ToolSpendObserver::new(store.clone(), fixed_cost(0.005));

        let event = ToolCallSuccessEvent {
            ctx: ToolRequestContext {
                provider: "github".into(),
                operation: "search".into(),
                caller: CallerContext {
                    account_id: Some("acct-1".into()),
                    ..CallerContext::default()
                },
                latency_ms: 200,
            },
        };

        observer.on_tool_call_success(event).await;

        let logs = store.logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].service_type, ServiceType::Tool);
        assert_eq!(logs[0].service_name, "github");
        assert_eq!(logs[0].operation, "search");
        assert!(logs[0].success);
        assert!((logs[0].cost - 0.005).abs() < 1e-10);
    }

    #[tokio::test]
    async fn observer_records_failure() {
        let store = Arc::new(InMemorySpendStore::new());
        let observer = ToolSpendObserver::new(store.clone(), zero_cost());

        let event = ToolCallFailureEvent {
            ctx: ToolRequestContext {
                provider: "github".into(),
                operation: "search".into(),
                caller: CallerContext::default(),
                latency_ms: 50,
            },
            error: "timeout".into(),
        };

        observer.on_tool_call_failure(event).await;

        let logs = store.logs();
        assert_eq!(logs.len(), 1);
        assert!(!logs[0].success);
        assert_eq!(logs[0].error_info.as_deref(), Some("timeout"));
    }
}
