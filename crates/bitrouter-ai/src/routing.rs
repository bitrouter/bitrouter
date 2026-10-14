//! Public routing identities and policy data carried by service APIs.
//! Selection, authorization and execution remain application-owned.

use crate::event::PipelineEvent;
use serde::{Deserialize, Serialize};

/// How a cascade chain should be ordered.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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

/// Summary of a routable model, for `GET /v1/models`.
///
/// Also the element type of the app-owned `list_models` action report. It
/// derives `JsonSchema` because typed control clients consume that report. It
/// lives here rather than beside the action because it already rides a wire —
/// `GET /v1/models` and the daemon control socket both carry it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// A routable model selector. Subscription-backed providers use an
    /// explicit `provider:canonical-model` selector here because a bare
    /// canonical request must not opt into a personal subscription.
    pub id: String,
    /// Providers that declare this model.
    pub providers: Vec<String>,
}

/// Stable identity of the named router selected for one request.
///
/// This value is frozen when Stage 0 resolves the raw selector, emitted as a
/// typed pipeline event, and preserved through policy selection and provider
/// fallback. `binding_digest` identifies the redaction-safe router binding;
/// any policy artifact digest remains a separate app-owned identity.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterRequestIdentity {
    /// Canonical router id, including the name normalized from a legacy preset.
    pub router_id: String,
    /// Exact inbound model selector before ingress transforms, such as
    /// `bitrouter/coding` or `@coding`.
    pub original_selector: String,
    /// Versioned, redaction-safe effective binding digest.
    pub binding_digest: String,
}

/// Context treatment owned by a policy action. It never grants host rights.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ContextStrategy {
    /// Preserve the complete admitted source view.
    Preserve,
    /// Select among semantically justified views within granted capabilities.
    #[default]
    Evidence,
}

/// One policy action: generation model, optional effort and context treatment.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyModelTarget {
    /// Normal generation route selector.
    pub model: String,
    /// Explicit policy effort; absence preserves a caller constraint.
    pub effort: Option<crate::types::ReasoningEffort>,
    /// Context strategy is part of the action identity, including Learn evidence.
    pub context: crate::routing::ContextStrategy,
}

/// Routing knobs shared by presets and variants.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoutingConfig {
    /// Cascade-chain ordering.
    pub sort: Option<SortOrder>,
    /// Require providers carrying all these tags.
    pub require_tags: Vec<String>,
    /// Restrict the chain to exactly these providers.
    pub only: Vec<String>,
    /// Drop these providers from the chain.
    pub ignore: Vec<String>,
}

impl PolicyModelTarget {
    /// Construct a model action with evidence routing and inherited effort.
    pub fn model_only(model: String) -> Self {
        Self {
            model,
            effort: None,
            context: Default::default(),
        }
    }
    /// Model selected by this action.
    pub fn model(&self) -> &str {
        &self.model
    }
    /// Optional policy effort.
    pub fn effort(&self) -> Option<crate::types::ReasoningEffort> {
        self.effort
    }
}

impl From<String> for PolicyModelTarget {
    fn from(model: String) -> Self {
        Self::model_only(model)
    }
}

impl From<&str> for PolicyModelTarget {
    fn from(model: &str) -> Self {
        Self::model_only(model.into())
    }
}

impl std::fmt::Display for PolicyModelTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.model)?;
        if let Some(effort) = self.effort {
            write!(formatter, "@{effort}")?;
        }
        write!(formatter, ":{:?}", self.context)
    }
}

impl PipelineEvent for RouterRequestIdentity {
    fn event_name(&self) -> &'static str {
        "router.resolved"
    }
}
