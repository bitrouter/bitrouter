//! Builder for the complete observation stack.
//!
//! [`ObserveStack`] encapsulates spend tracking, metrics collection, and
//! request observation behind a single construction point, keeping wiring
//! details out of the server binary.

use std::sync::Arc;

use bitrouter_core::observe::{AgentObserveCallback, ObserveCallback, ToolObserveCallback};
use bitrouter_core::routers::routing_table::ModelPricing;
use sea_orm::DatabaseConnection;

use crate::agent_observer::AgentSpendObserver;
use crate::composite::CompositeObserver;
use crate::metrics::MetricsCollector;
use crate::observer::SpendObserver;
use crate::spend::memory::InMemorySpendStore;
use crate::spend::sea_orm_store::SeaOrmSpendStore;
use crate::spend::store::SpendStore;
use crate::tool_observer::ToolSpendObserver;

/// A fully assembled observation pipeline.
///
/// Holds the composite observer (which implements all three callback traits),
/// the metrics collector (for the `/v1/metrics` endpoint), and the spend
/// store (for budget queries).
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
    agent_cost_fn: Option<CostFn>,
}

impl ObserveStackBuilder {
    fn new() -> Self {
        Self {
            spend_store: None,
            pricing_fn: None,
            tool_cost_fn: None,
            agent_cost_fn: None,
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

    /// Set the tool cost lookup: `(server_name, tool_name) -> cost_usd`.
    pub fn tool_cost(mut self, f: impl Fn(&str, &str) -> f64 + Send + Sync + 'static) -> Self {
        self.tool_cost_fn = Some(Arc::new(f));
        self
    }

    /// Set the agent cost lookup: `(agent_name, method) -> cost_usd`.
    pub fn agent_cost(mut self, f: impl Fn(&str, &str) -> f64 + Send + Sync + 'static) -> Self {
        self.agent_cost_fn = Some(Arc::new(f));
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
        let spend_observer = Arc::new(SpendObserver::new(spend_store.clone(), pricing_fn));

        // Tool spend observer.
        let tool_cost_fn = self.tool_cost_fn.unwrap_or_else(|| Arc::new(|_, _| 0.0));
        let tool_spend_observer =
            Arc::new(ToolSpendObserver::new(spend_store.clone(), tool_cost_fn));

        // Agent spend observer.
        let agent_cost_fn = self.agent_cost_fn.unwrap_or_else(|| Arc::new(|_, _| 0.0));
        let agent_spend_observer =
            Arc::new(AgentSpendObserver::new(spend_store.clone(), agent_cost_fn));

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
            vec![
                agent_spend_observer as Arc<dyn AgentObserveCallback>,
                metrics.clone() as Arc<dyn AgentObserveCallback>,
            ],
        ));

        ObserveStack {
            observer: composite,
            metrics,
            spend_store,
        }
    }
}
