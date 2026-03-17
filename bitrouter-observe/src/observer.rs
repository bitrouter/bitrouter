//! [`SpendObserver`] — an [`ObserveCallback`] that calculates cost and
//! persists spend logs via a [`SpendStore`].

use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use std::future::Future;
use std::pin::Pin;

use bitrouter_core::observe::{ObserveCallback, RequestFailureEvent, RequestSuccessEvent};

use crate::cost::{Pricing, calculate_cost};
use crate::spend::store::{SpendLog, SpendStore};

/// A thread-safe closure that maps `(provider, model)` to [`Pricing`].
type PricingLookup = dyn Fn(&str, &str) -> Pricing + Send + Sync;

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
                account_id: event.ctx.account_id,
                model: event.ctx.model,
                provider: event.ctx.provider,
                input_tokens: event.usage.input_tokens.total.unwrap_or(0),
                output_tokens: event.usage.output_tokens.total.unwrap_or(0),
                cost,
                latency_ms: event.ctx.latency_ms,
                success: true,
                error_type: None,
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
            let error_type = error_variant_name(&event.error);

            let log = SpendLog {
                id: Uuid::new_v4(),
                account_id: event.ctx.account_id,
                model: event.ctx.model,
                provider: event.ctx.provider,
                input_tokens: 0,
                output_tokens: 0,
                cost: 0.0,
                latency_ms: event.ctx.latency_ms,
                success: false,
                error_type: Some(error_type),
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
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bitrouter_core::errors::BitrouterError;
    use bitrouter_core::models::language::usage::{
        LanguageModelInputTokens, LanguageModelOutputTokens, LanguageModelUsage,
    };
    use bitrouter_core::observe::{RequestContext, RequestFailureEvent, RequestSuccessEvent};

    use crate::cost::Pricing;
    use crate::spend::memory::InMemorySpendStore;

    use super::*;

    fn test_pricing() -> Pricing {
        Pricing {
            input_no_cache: 2.50,
            input_cache_read: 0.0,
            input_cache_write: 0.0,
            output_text: 10.00,
            output_reasoning: 0.0,
        }
    }

    fn test_ctx() -> RequestContext {
        RequestContext {
            route: "fast".into(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
            account_id: Some("acct-1".into()),
            agent_name: None,
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
        assert_eq!(logs[0].input_tokens, 1000);
        assert_eq!(logs[0].output_tokens, 500);
        // cost = 1000*2.50/1M + 500*10.0/1M = 0.0025 + 0.005 = 0.0075
        assert!((logs[0].cost - 0.0075).abs() < 1e-10);
    }

    #[tokio::test]
    async fn failure_writes_spend_log_with_error_type() {
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
        assert_eq!(logs[0].error_type.as_deref(), Some("Transport"));
        assert_eq!(logs[0].cost, 0.0);
    }
}
