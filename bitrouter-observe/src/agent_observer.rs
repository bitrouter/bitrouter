//! [`AgentSpendObserver`] — an [`AgentObserveCallback`] that persists agent
//! call spend logs via a [`SpendStore`].

use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use std::future::Future;
use std::pin::Pin;

use bitrouter_core::observe::{AgentCallFailureEvent, AgentCallSuccessEvent, AgentObserveCallback};

use crate::spend::store::{ServiceType, SpendLog, SpendStore};

/// Cost lookup function: `(agent_name, method) -> cost_usd`.
pub type AgentCostFn = Arc<dyn Fn(&str, &str) -> f64 + Send + Sync>;

/// Observes completed A2A agent calls and writes spend logs.
pub struct AgentSpendObserver {
    store: Arc<dyn SpendStore>,
    cost_fn: AgentCostFn,
}

impl AgentSpendObserver {
    pub fn new(store: Arc<dyn SpendStore>, cost_fn: AgentCostFn) -> Self {
        Self { store, cost_fn }
    }
}

impl AgentObserveCallback for AgentSpendObserver {
    fn on_agent_call_success(
        &self,
        event: AgentCallSuccessEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let cost = (self.cost_fn)(&event.ctx.agent, &event.ctx.method);
            let log = SpendLog {
                id: Uuid::new_v4(),
                service_type: ServiceType::Agent,
                account_id: event.ctx.caller.account_id,
                session_id: None,
                service_name: event.ctx.agent,
                operation: event.ctx.method,
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

    fn on_agent_call_failure(
        &self,
        event: AgentCallFailureEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let cost = (self.cost_fn)(&event.ctx.agent, &event.ctx.method);
            let log = SpendLog {
                id: Uuid::new_v4(),
                service_type: ServiceType::Agent,
                account_id: event.ctx.caller.account_id,
                session_id: None,
                service_name: event.ctx.agent,
                operation: event.ctx.method,
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
        AgentCallFailureEvent, AgentCallSuccessEvent, AgentRequestContext, CallerContext,
    };

    use crate::spend::memory::InMemorySpendStore;
    use crate::spend::store::ServiceType;

    use super::*;

    fn zero_cost() -> AgentCostFn {
        Arc::new(|_, _| 0.0)
    }

    fn fixed_cost(c: f64) -> AgentCostFn {
        Arc::new(move |_, _| c)
    }

    #[tokio::test]
    async fn observer_writes_agent_log() {
        let store = Arc::new(InMemorySpendStore::new());
        let observer = AgentSpendObserver::new(store.clone(), fixed_cost(0.01));

        let event = AgentCallSuccessEvent {
            ctx: AgentRequestContext {
                agent: "upstream-agent".into(),
                method: "message/send".into(),
                caller: CallerContext {
                    account_id: Some("acct-1".into()),
                    ..CallerContext::default()
                },
                latency_ms: 500,
            },
        };

        observer.on_agent_call_success(event).await;

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
        let observer = AgentSpendObserver::new(store.clone(), zero_cost());

        let event = AgentCallFailureEvent {
            ctx: AgentRequestContext {
                agent: "upstream-agent".into(),
                method: "tasks/get".into(),
                caller: CallerContext::default(),
                latency_ms: 100,
            },
            error: "upstream timeout".into(),
        };

        observer.on_agent_call_failure(event).await;

        let logs = store.logs();
        assert_eq!(logs.len(), 1);
        assert!(!logs[0].success);
        assert_eq!(logs[0].error_info.as_deref(), Some("upstream timeout"));
    }
}
