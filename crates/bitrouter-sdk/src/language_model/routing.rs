//! Routing for the `language_model` protocol: the `RoutingTable` trait,
//! `RoutingPrefs`, `ModelInfo`, and the `FallbackPolicy`. The full cascade
//! resolution (Strategy 0–3) and config/registry tables land in Phase 4; this
//! module carries the trait surface plus a `StaticRoutingTable` used by the
//! pipeline tests and the minimal server path.

use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::caller::CallerContext;
use crate::error::{BitrouterError, Result};
use crate::language_model::hooks::FallbackDecision;
use crate::language_model::types::{ApiProtocol, Capability, RoutingTarget};

/// How a cascade chain should be ordered.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    /// Sort by provider name, ascending. The Phase-1 default; later replaced by
    /// a recommender (cost / latency / throughput scoring).
    #[default]
    Alphabetical,
    /// Sort by lowest latency first.
    Latency,
    /// Sort by lowest cost first.
    Cost,
}

/// Routing preferences distilled from `@preset` / `:variant`.
/// Feeds both explicit-virtual-model endpoint selection and auto-cascade
/// ordering / filtering.
#[derive(Debug, Clone, Default)]
pub struct RoutingPrefs {
    /// Ordering applied to the cascade chain.
    pub sort: SortOrder,
    /// Only providers carrying all these tags are eligible.
    pub require_tags: Vec<String>,
    /// If non-empty, restrict the chain to exactly these providers.
    pub only: Vec<String>,
    /// Drop these providers from the chain.
    pub ignore: Vec<String>,
    /// The capabilities a request needs; a capability-aware [`RoutingTable`]
    /// should treat only providers advertising all of these as eligible. The
    /// pipeline populates it from
    /// [`Prompt::required_capabilities`](crate::language_model::Prompt::required_capabilities).
    /// Empty (the default) imposes no capability constraint.
    pub require_capabilities: Vec<Capability>,
    /// The inbound wire protocol the request arrived on, if known. A
    /// protocol-native [`RoutingTable`] prefers, for each chosen target, the
    /// protocol matching this when the upstream supports it (a faithful
    /// same-protocol round-trip), otherwise the provider's configured default.
    /// `None` imposes no native preference. Set by the pipeline from
    /// [`PipelineContext::inbound_protocol`](crate::language_model::PipelineContext::inbound_protocol).
    pub inbound_protocol: Option<ApiProtocol>,
}

/// Summary of a routable model, for `GET /v1/models`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// The model id.
    pub id: String,
    /// Providers that declare this model.
    pub providers: Vec<String>,
}

/// Resolves a model name into a fallback chain. v1 has no single-target
/// `route()` — everything is `route_chain()`; a "single target" is just a
/// length-1 chain. Implementations: `ConfigRoutingTable` (yaml) and
/// `StaticRoutingTable` here.
#[async_trait]
pub trait RoutingTable: Send + Sync {
    /// Resolve `model` into an ordered fallback chain.
    async fn route_chain(
        &self,
        model: &str,
        prefs: &RoutingPrefs,
        caller: &CallerContext,
    ) -> Result<Vec<RoutingTarget>>;

    /// List every routable model (for `GET /v1/models`).
    fn list_models(&self) -> Vec<ModelInfo>;

    /// Look up one model's info.
    fn model_info(&self, model: &str) -> Option<ModelInfo>;

    /// Hot-reload the underlying config.
    async fn reload(&self) -> Result<()>;

    /// Stage-0 preset prompt-body overrides for `model`. Implementations that
    /// don't know about presets return an empty [`PromptOverrides`]; the
    /// pipeline applies these (shallow-merge into params, set system prompt)
    /// before execution. Default impl returns nothing so non-preset-aware
    /// tables (e.g. [`StaticRoutingTable`]) work unchanged.
    async fn preset_overrides(&self, _model: &str) -> Result<PromptOverrides> {
        Ok(PromptOverrides::default())
    }
}

/// Prompt body overrides carried by a preset, shallow-merged into the request.
///
/// Returned by [`RoutingTable::preset_overrides`] and applied by the pipeline
/// *before* the request is dispatched: a non-empty `system_prompt` is set on
/// the canonical [`Prompt`](crate::language_model::Prompt) when it has none,
/// and `params` entries are inserted into
/// [`GenerationParams::extra`](crate::language_model::GenerationParams::extra)
/// for keys not already present.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PromptOverrides {
    /// System prompt to set, if the preset defines one.
    pub system_prompt: Option<String>,
    /// Generation-parameter overrides (shallow-merged).
    pub params: serde_json::Map<String, serde_json::Value>,
}

impl PromptOverrides {
    /// Whether there is nothing to apply.
    pub fn is_empty(&self) -> bool {
        self.system_prompt.is_none() && self.params.is_empty()
    }
}

/// Classifies an upstream error into a fallback decision. `FallbackPolicy` is
/// the default implementation behind `ExecutionHook::on_failure`.
pub trait FallbackPolicy: Send + Sync {
    /// Decide whether to try the next target after `err` on `attempted`.
    fn classify(&self, err: &BitrouterError, attempted: &RoutingTarget) -> FallbackDecision;
}

/// The default policy: 5xx / 408 / 429 / transport / payment-exhaustion
/// errors → try next; other 4xx → fail. Mirrors v0's
/// `DefaultFallbackPolicy`.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultFallbackPolicy;

impl FallbackPolicy for DefaultFallbackPolicy {
    fn classify(&self, err: &BitrouterError, _attempted: &RoutingTarget) -> FallbackDecision {
        match err {
            BitrouterError::Upstream { status, .. } if *status >= 500 => FallbackDecision::TryNext,
            BitrouterError::Upstream { status, .. } if *status == 408 || *status == 429 => {
                FallbackDecision::TryNext
            }
            BitrouterError::UpstreamTimeout => FallbackDecision::TryNext,
            BitrouterError::RateLimited { .. } => FallbackDecision::TryNext,
            // Payment / credit exhaustion → try the next target. For a
            // multi-account provider this drops to the next account
            // (the "fall back when a subscription runs out" path); for
            // a plain cascade it tries the next provider, which may
            // still have funds. If every target is drained the chain
            // exhausts and the original error is returned.
            BitrouterError::PaymentRequired(_) => FallbackDecision::TryNext,
            // Any other error is the request's own fault — do not retry; the
            // original error is preserved.
            other => FallbackDecision::Fail(other.clone()),
        }
    }
}

/// A trivial in-memory routing table: `model -> ordered targets`. Used by the
/// Phase-1 pipeline tests and the minimal server path. Phase 4 replaces it with
/// `ConfigRoutingTable`.
pub struct StaticRoutingTable {
    routes: RwLock<HashMap<String, Vec<RoutingTarget>>>,
}

impl StaticRoutingTable {
    /// An empty table.
    pub fn new() -> Self {
        Self {
            routes: RwLock::new(HashMap::new()),
        }
    }

    /// Register a chain for `model`.
    pub fn insert(&self, model: impl Into<String>, chain: Vec<RoutingTarget>) {
        self.routes
            .write()
            .expect("routing table lock poisoned")
            .insert(model.into(), chain);
    }
}

impl Default for StaticRoutingTable {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RoutingTable for StaticRoutingTable {
    async fn route_chain(
        &self,
        model: &str,
        prefs: &RoutingPrefs,
        _caller: &CallerContext,
    ) -> Result<Vec<RoutingTarget>> {
        let guard = self.routes.read().expect("routing table lock poisoned");
        let mut chain = guard
            .get(model)
            .cloned()
            .ok_or_else(|| BitrouterError::NotFound(format!("no route for model '{model}'")))?;

        if !prefs.only.is_empty() {
            chain.retain(|t| prefs.only.contains(&t.provider_name));
        }
        chain.retain(|t| !prefs.ignore.contains(&t.provider_name));
        if matches!(prefs.sort, SortOrder::Alphabetical) {
            chain.sort_by(|a, b| a.provider_name.cmp(&b.provider_name));
        }
        if chain.is_empty() {
            return Err(BitrouterError::NotFound(format!(
                "no route for model '{model}' after applying routing preferences"
            )));
        }
        Ok(chain)
    }

    fn list_models(&self) -> Vec<ModelInfo> {
        let guard = self.routes.read().expect("routing table lock poisoned");
        guard
            .iter()
            .map(|(id, chain)| ModelInfo {
                id: id.clone(),
                providers: chain.iter().map(|t| t.provider_name.clone()).collect(),
            })
            .collect()
    }

    fn model_info(&self, model: &str) -> Option<ModelInfo> {
        let guard = self.routes.read().expect("routing table lock poisoned");
        guard.get(model).map(|chain| ModelInfo {
            id: model.to_string(),
            providers: chain.iter().map(|t| t.provider_name.clone()).collect(),
        })
    }

    async fn reload(&self) -> Result<()> {
        Ok(())
    }
}
