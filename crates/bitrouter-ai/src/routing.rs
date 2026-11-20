//! Public routing identities and policy data carried by service APIs.
//! Selection, authorization and execution remain application-owned.

use crate::event::PipelineEvent;
use serde::{Deserialize, Serialize};

/// How a cascade chain should be ordered.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    /// Sort by provider name, ascending; preserve declared order for ties.
    #[default]
    Alphabetical,
    /// Sort by lowest latency first.
    Latency,
    /// Sort by lowest cost first.
    Cost,
    /// Sort by highest observed output-token throughput first. Runtimes without
    /// qualified observations retain their deterministic baseline order.
    Throughput,
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
    /// Canonical router ID.
    pub router_id: String,
    /// Exact inbound model selector before ingress transforms, such as
    /// `bitrouter/coding` or `@coding`.
    pub original_selector: String,
    /// Versioned, redaction-safe effective binding digest.
    pub binding_digest: String,
}

impl PipelineEvent for RouterRequestIdentity {
    fn event_name(&self) -> &'static str {
        "router.resolved"
    }
}
