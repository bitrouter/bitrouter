//! [`SpendObserver`] — an [`ObserveCallback`] that calculates cost and
//! persists spend logs via a [`SpendStore`].

use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use std::future::Future;
use std::pin::Pin;

use bitrouter_core::observe::{ObserveCallback, RequestFailureEvent, RequestSuccessEvent};
use bitrouter_core::pricing::calculate_cost;
use bitrouter_core::routers::routing_table::ModelPricing;

use crate::spend::store::{ServiceType, SpendLog, SpendStore};

/// A thread-safe closure that maps `(provider, model)` to [`ModelPricing`].
type PricingLookup = dyn Fn(&str, &str) -> ModelPricing + Send + Sync;

/// Observes completed requests, calculates cost, and writes spend logs.
///
/// The `pricing_lookup` closure maps `(provider, model)` to [`Pricing`],
/// decoupling this crate from `bitrouter-config`.
pub struct SpendObserver {
    store: Arc<dyn SpendStore>,
    pricing_lookup: Arc<PricingLookup>,
}

impl SpendObserver {
    pub fn new(store: Arc<dyn SpendStore>, pricing_lookup: Arc<PricingLookup>) -> Self {
        Self {
            store,
            pricing_lookup,
        }
    }
}

impl ObserveCallback for SpendObserver {
    fn on_request_success(
        &self,
        event: RequestSuccessEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let pricing = (self.pricing_lookup)(&event.ctx.provider, &event.ctx.model);
            let cost = calculate_cost(&event.usage, &pricing);

            let log = SpendLog {
                id: Uuid::new_v4(),
                service_type: ServiceType::Model,
                account_id: event.ctx.caller.account_id,
                session_id: None,
                service_name: event.ctx.route,
                operation: format!("{}:{}", event.ctx.provider, event.ctx.model),
                input_tokens: event.usage.input_tokens.total.unwrap_or(0),
                output_tokens: event.usage.output_tokens.total.unwrap_or(0),
                cost,
                latency_ms: event.ctx.latency_ms,
                success: true,
                error_info: None,
                created_at: Utc::now().naive_utc(),
            };

            self.store.write(log).await;
        })
    }

    fn on_request_failure(
        &self,
        event: RequestFailureEvent,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let error_info = error_variant_name(&event.error);

            let log = SpendLog {
                id: Uuid::new_v4(),
                service_type: ServiceType::Model,
                account_id: event.ctx.caller.account_id,
                session_id: None,
                service_name: event.ctx.route,
                operation: format!("{}:{}", event.ctx.provider, event.ctx.model),
                input_tokens: 0,
                output_tokens: 0,
                cost: 0.0,
                latency_ms: event.ctx.latency_ms,
                success: false,
                error_info: Some(error_info),
                created_at: Utc::now().naive_utc(),
            };

            self.store.write(log).await;
        })
    }
}

/// Extracts the enum variant name from a [`BitrouterError`] for logging.
fn error_variant_name(error: &bitrouter_core::errors::BitrouterError) -> String {
    use bitrouter_core::errors::BitrouterError;
    match error {
        BitrouterError::UnsupportedFeature { .. } => "UnsupportedFeature".into(),
        BitrouterError::Cancelled { .. } => "Cancelled".into(),
        BitrouterError::InvalidRequest { .. } => "InvalidRequest".into(),
        BitrouterError::Transport { .. } => "Transport".into(),
        BitrouterError::ResponseDecode { .. } => "ResponseDecode".into(),
        BitrouterError::InvalidResponse { .. } => "InvalidResponse".into(),
        BitrouterError::Provider { .. } => "Provider".into(),
        BitrouterError::StreamProtocol { .. } => "StreamProtocol".into(),
        BitrouterError::AccessDenied { .. } => "AccessDenied".into(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bitrouter_core::errors::BitrouterError;
    use bitrouter_core::models::language::usage::{
        LanguageModelInputTokens, LanguageModelOutputTokens, LanguageModelUsage,
    };
    use bitrouter_core::observe::{
        CallerContext, RequestContext, RequestFailureEvent, RequestSuccessEvent,
    };
    use bitrouter_core::routers::routing_table::{
        InputTokenPricing, ModelPricing, OutputTokenPricing,
    };

    use crate::spend::memory::InMemorySpendStore;

    use super::*;

    fn test_pricing() -> ModelPricing {
        ModelPricing {
            input_tokens: InputTokenPricing {
                no_cache: Some(2.50),
                cache_read: None,
                cache_write: None,
            },
            output_tokens: OutputTokenPricing {
                text: Some(10.00),
                reasoning: None,
            },
        }
    }

    fn test_ctx() -> RequestContext {
        RequestContext {
            route: "fast".into(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
            caller: CallerContext {
                account_id: Some("acct-1".into()),
                ..CallerContext::default()
            },
            latency_ms: 250,
        }
    }

    #[tokio::test]
    async fn success_writes_spend_log_with_cost() {
        let store = Arc::new(InMemorySpendStore::new());
        let observer = SpendObserver::new(store.clone(), Arc::new(|_, _| test_pricing()));

        let event = RequestSuccessEvent {
            ctx: test_ctx(),
            usage: LanguageModelUsage {
                input_tokens: LanguageModelInputTokens {
                    total: Some(1000),
                    no_cache: None,
                    cache_read: None,
                    cache_write: None,
                },
                output_tokens: LanguageModelOutputTokens {
                    total: Some(500),
                    text: None,
                    reasoning: None,
                },
                raw: None,
            },
        };

        observer.on_request_success(event).await;

        let logs = store.logs();
        assert_eq!(logs.len(), 1);
        assert!(logs[0].success);
        assert_eq!(logs[0].service_type, ServiceType::Model);
        assert_eq!(logs[0].service_name, "fast");
        assert_eq!(logs[0].operation, "openai:gpt-4o");
        assert_eq!(logs[0].input_tokens, 1000);
        assert_eq!(logs[0].output_tokens, 500);
        // cost = 1000*2.50/1M + 500*10.0/1M = 0.0025 + 0.005 = 0.0075
        assert!((logs[0].cost - 0.0075).abs() < 1e-10);
    }

    #[tokio::test]
    async fn failure_writes_spend_log_with_error_info() {
        let store = Arc::new(InMemorySpendStore::new());
        let observer = SpendObserver::new(store.clone(), Arc::new(|_, _| test_pricing()));

        let event = RequestFailureEvent {
            ctx: test_ctx(),
            error: BitrouterError::transport(None, "connection refused"),
        };

        observer.on_request_failure(event).await;

        let logs = store.logs();
        assert_eq!(logs.len(), 1);
        assert!(!logs[0].success);
        assert_eq!(logs[0].error_info.as_deref(), Some("Transport"));
        assert_eq!(logs[0].cost, 0.0);
    }
}
