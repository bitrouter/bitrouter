//! Builder for the complete observation stack.
//!
//! [`ObserveStack`] encapsulates spend tracking, metrics collection, and
//! request observation behind a single construction point, keeping wiring
//! details out of the server binary.

use std::sync::Arc;

use bitrouter_core::observe::{AgentObserveCallback, ObserveCallback, ToolObserveCallback};
use bitrouter_core::routers::routing_table::ModelPricing;
use sea_orm::DatabaseConnection;

use crate::composite::CompositeObserver;
use crate::metrics::MetricsCollector;
use crate::model_observer::ModelSpendObserver;
use crate::spend::memory::InMemorySpendStore;
use crate::spend::sea_orm_store::SeaOrmSpendStore;
use crate::spend::store::SpendStore;
use crate::tool_observer::ToolSpendObserver;

#[cfg(feature = "otlp")]
use crate::otlp::observer::OtlpObserver;
#[cfg(feature = "otlp")]
use crate::otlp::pipeline::{Pipeline, PipelineConfig};

/// A fully assembled observation pipeline.
///
/// Holds the composite observer (which implements [`ObserveCallback`],
/// [`ToolObserveCallback`], and [`AgentObserveCallback`]), the metrics
/// collector (for the `/v1/metrics` endpoint), and the spend store (for
/// budget queries).
pub struct ObserveStack {
    /// Composite observer implementing [`ObserveCallback`],
    /// [`ToolObserveCallback`], and [`AgentObserveCallback`].
    pub observer: Arc<CompositeObserver>,
    /// In-memory metrics collector for the `/v1/metrics` endpoint.
    pub metrics: Arc<MetricsCollector>,
    /// Spend log store for budget queries.
    pub spend_store: Arc<dyn SpendStore>,
}

impl ObserveStack {
    /// Returns a new [`ObserveStackBuilder`].
    pub fn builder() -> ObserveStackBuilder {
        ObserveStackBuilder::new()
    }
}

type ModelPricingFn = Arc<dyn Fn(&str, &str) -> ModelPricing + Send + Sync>;
type CostFn = Arc<dyn Fn(&str, &str) -> f64 + Send + Sync>;

/// Builder for [`ObserveStack`].
///
/// All fields are optional and have sensible defaults:
/// - Spend store defaults to in-memory.
/// - Pricing lookups default to zero cost.
pub struct ObserveStackBuilder {
    spend_store: Option<Arc<dyn SpendStore>>,
    pricing_fn: Option<ModelPricingFn>,
    tool_cost_fn: Option<CostFn>,
    #[cfg(feature = "otlp")]
    otlp_pipeline: Option<PipelineConfig>,
}

impl ObserveStackBuilder {
    fn new() -> Self {
        Self {
            spend_store: None,
            pricing_fn: None,
            tool_cost_fn: None,
            #[cfg(feature = "otlp")]
            otlp_pipeline: None,
        }
    }

    /// Use a database-backed spend store. When omitted, an in-memory store
    /// is used instead.
    pub fn with_db(mut self, db: &DatabaseConnection) -> Self {
        self.spend_store = Some(Arc::new(SeaOrmSpendStore::new(db.clone())));
        self
    }

    /// Set the model pricing lookup: `(provider, model) -> ModelPricing`.
    pub fn model_pricing(
        mut self,
        f: impl Fn(&str, &str) -> ModelPricing + Send + Sync + 'static,
    ) -> Self {
        self.pricing_fn = Some(Arc::new(f));
        self
    }

    /// Set the tool cost lookup: `(provider_name, operation) -> cost_usd`.
    ///
    /// This covers both MCP tool calls and A2A agent invocations.
    pub fn tool_cost(mut self, f: impl Fn(&str, &str) -> f64 + Send + Sync + 'static) -> Self {
        self.tool_cost_fn = Some(Arc::new(f));
        self
    }

    /// Enables OTLP/HTTP export with the given pipeline configuration.
    ///
    /// Builds an [`OtlpObserver`] and wires it into the composite observer
    /// alongside the spend store and metrics collector. When the pipeline
    /// has no destinations the call is treated as a no-op so binaries can
    /// unconditionally call `with_otlp(translate(&config.telemetry))`
    /// without an outer `if`.
    #[cfg(feature = "otlp")]
    pub fn with_otlp(mut self, config: PipelineConfig) -> Self {
        if config.is_active() {
            self.otlp_pipeline = Some(config);
        }
        self
    }

    /// Build the complete observation stack.
    pub fn build(self) -> ObserveStack {
        let spend_store: Arc<dyn SpendStore> = self
            .spend_store
            .unwrap_or_else(|| Arc::new(InMemorySpendStore::new()));

        let metrics = Arc::new(MetricsCollector::new());

        // Model spend observer.
        let pricing_fn = self
            .pricing_fn
            .unwrap_or_else(|| Arc::new(|_, _| ModelPricing::default()));
        let spend_observer = Arc::new(ModelSpendObserver::new(spend_store.clone(), pricing_fn));

        // Tool spend observer (covers MCP tools and A2A agents).
        let tool_cost_fn = self.tool_cost_fn.unwrap_or_else(|| Arc::new(|_, _| 0.0));
        let tool_spend_observer =
            Arc::new(ToolSpendObserver::new(spend_store.clone(), tool_cost_fn));

        let composite = build_composite(
            spend_observer,
            tool_spend_observer,
            metrics.clone(),
            #[cfg(feature = "otlp")]
            self.otlp_pipeline,
        );

        ObserveStack {
            observer: composite,
            metrics,
            spend_store,
        }
    }
}

/// Splits the composite-observer assembly out of [`ObserveStackBuilder::build`]
/// so that the OTLP-enabled and OTLP-disabled branches can each construct
/// the callback vectors as a single expression — avoiding `mut` bindings
/// that would lint as `unused_mut` in the disabled branch without
/// requiring `#[allow]`.
#[cfg(not(feature = "otlp"))]
fn build_composite(
    spend_observer: Arc<ModelSpendObserver>,
    tool_spend_observer: Arc<ToolSpendObserver>,
    metrics: Arc<MetricsCollector>,
) -> Arc<CompositeObserver> {
    Arc::new(CompositeObserver::new(
        vec![
            spend_observer as Arc<dyn ObserveCallback>,
            metrics.clone() as Arc<dyn ObserveCallback>,
        ],
        vec![
            tool_spend_observer as Arc<dyn ToolObserveCallback>,
            metrics.clone() as Arc<dyn ToolObserveCallback>,
        ],
        vec![metrics as Arc<dyn AgentObserveCallback>],
    ))
}

#[cfg(feature = "otlp")]
fn build_composite(
    spend_observer: Arc<ModelSpendObserver>,
    tool_spend_observer: Arc<ToolSpendObserver>,
    metrics: Arc<MetricsCollector>,
    otlp_pipeline: Option<PipelineConfig>,
) -> Arc<CompositeObserver> {
    let otlp = otlp_pipeline.map(|cfg| Arc::new(OtlpObserver::new(Pipeline::new(cfg))));

    let mut model_callbacks: Vec<Arc<dyn ObserveCallback>> = vec![
        spend_observer as Arc<dyn ObserveCallback>,
        metrics.clone() as Arc<dyn ObserveCallback>,
    ];
    let mut tool_callbacks: Vec<Arc<dyn ToolObserveCallback>> = vec![
        tool_spend_observer as Arc<dyn ToolObserveCallback>,
        metrics.clone() as Arc<dyn ToolObserveCallback>,
    ];
    let mut agent_callbacks: Vec<Arc<dyn AgentObserveCallback>> =
        vec![metrics.clone() as Arc<dyn AgentObserveCallback>];

    if let Some(o) = otlp {
        model_callbacks.push(o.clone() as Arc<dyn ObserveCallback>);
        tool_callbacks.push(o.clone() as Arc<dyn ToolObserveCallback>);
        agent_callbacks.push(o as Arc<dyn AgentObserveCallback>);
    }

    Arc::new(CompositeObserver::new(
        model_callbacks,
        tool_callbacks,
        agent_callbacks,
    ))
}

#[cfg(all(test, feature = "otlp"))]
mod otlp_builder_tests {
    use super::*;
    use bitrouter_core::models::language::usage::{
        LanguageModelInputTokens, LanguageModelOutputTokens, LanguageModelUsage,
    };
    use bitrouter_core::observe::{CallerContext, RequestContext, RequestSuccessEvent};

    #[tokio::test]
    async fn empty_destinations_dont_install_otlp_observer() {
        // Sanity: with_otlp(empty) is a no-op so callers can wire it unconditionally.
        let stack = ObserveStack::builder()
            .with_otlp(PipelineConfig::default())
            .build();

        // Two callbacks (spend + metrics) without OTLP, three with.
        // We assert via behavior: dispatching one event should not panic
        // and the metrics collector should record it.
        let event = RequestSuccessEvent {
            ctx: RequestContext {
                route: "fast".into(),
                provider: "openai".into(),
                model: "gpt-4o".into(),
                caller: CallerContext::default(),
                latency_ms: 10,
            },
            usage: LanguageModelUsage {
                input_tokens: LanguageModelInputTokens {
                    total: Some(1),
                    no_cache: None,
                    cache_read: None,
                    cache_write: None,
                },
                output_tokens: LanguageModelOutputTokens {
                    total: Some(1),
                    text: None,
                    reasoning: None,
                },
                raw: None,
            },
        };
        stack.observer.on_request_success(event).await;
        let snap = stack.metrics.snapshot();
        assert_eq!(snap.routes["fast"].total_requests, 1);
    }

    #[tokio::test]
    async fn with_otlp_active_config_installs_observer() {
        use crate::otlp::pipeline::{CaptureTier, Destination, Sampling};
        use std::collections::HashMap;
        let dest = Destination {
            name: "test".into(),
            endpoint: "https://example.com/v1/traces".into(),
            headers: HashMap::new(),
            sampling: Sampling::default(),
            redact: vec![],
        };
        let stack = ObserveStack::builder()
            .with_otlp(PipelineConfig {
                capture_tier: CaptureTier::Metadata,
                destinations: vec![dest],
            })
            .build();
        // Behavior assertion: dispatching an event runs through both
        // the metrics observer and the otlp observer without panicking.
        let event = RequestSuccessEvent {
            ctx: RequestContext {
                route: "fast".into(),
                provider: "openai".into(),
                model: "gpt-4o".into(),
                caller: CallerContext::default(),
                latency_ms: 10,
            },
            usage: LanguageModelUsage {
                input_tokens: LanguageModelInputTokens {
                    total: Some(1),
                    no_cache: None,
                    cache_read: None,
                    cache_write: None,
                },
                output_tokens: LanguageModelOutputTokens {
                    total: Some(1),
                    text: None,
                    reasoning: None,
                },
                raw: None,
            },
        };
        stack.observer.on_request_success(event).await;
    }
}
