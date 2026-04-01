//! Builder for the complete observation stack.
//!
//! [`ObserveStack`] encapsulates spend tracking, metrics collection, and
//! request observation behind a single construction point, keeping wiring
//! details out of the server binary.

use std::sync::Arc;

use bitrouter_core::observe::{ObserveCallback, ToolObserveCallback};
use bitrouter_core::routers::routing_table::ModelPricing;
use sea_orm::DatabaseConnection;

use crate::composite::CompositeObserver;
use crate::metrics::MetricsCollector;
use crate::model_observer::ModelSpendObserver;
use crate::spend::memory::InMemorySpendStore;
use crate::spend::sea_orm_store::SeaOrmSpendStore;
use crate::spend::store::SpendStore;
use crate::tool_observer::ToolSpendObserver;

/// A fully assembled observation pipeline.
///
/// Holds the composite observer (which implements [`ObserveCallback`] and
/// [`ToolObserveCallback`]), the metrics collector (for the `/v1/metrics`
/// endpoint), and the spend store (for budget queries).
pub struct ObserveStack {
    /// Composite observer implementing [`ObserveCallback`] and
    /// [`ToolObserveCallback`].
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
}

impl ObserveStackBuilder {
    fn new() -> Self {
        Self {
            spend_store: None,
            pricing_fn: None,
            tool_cost_fn: None,
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

        // Compose all observers.
        let composite = Arc::new(CompositeObserver::new(
            vec![
                spend_observer as Arc<dyn ObserveCallback>,
                metrics.clone() as Arc<dyn ObserveCallback>,
            ],
            vec![
                tool_spend_observer as Arc<dyn ToolObserveCallback>,
                metrics.clone() as Arc<dyn ToolObserveCallback>,
            ],
        ));

        ObserveStack {
            observer: composite,
            metrics,
            spend_store,
        }
    }
}
