//! Read-only route preview and pricing evidence.

use crate::types::ReasoningEffort;

/// Arguments to the `route` action.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RouteInput {
    /// The model selector to resolve (as you'd send to the daemon's
    /// `/v1/chat/completions`).
    pub model: String,
    /// Optional prompt text. Used to derive the agent-loop step the policy
    /// table keys on; omit for a bare model resolution.
    ///
    /// Consulted on the config paths only. The daemon's control-socket `route`
    /// verb takes no prompt and resolves the model as given (see
    /// [`ResolvedVia::Live`]), so a [`ResolvedVia::Live`] answer
    /// is the same with or without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// Which path produced the chain.
///
/// This is not a provenance footnote: it says which routing snapshot supplied
/// the preview. [`RouteReport::policy_decision_executed`] separately says
/// whether a policy decision was actually replayed.
///
/// Wire values are `snake_case` — `live` / `config` / `zero_config` — the same
/// vocabulary as the `list_models` report's `resolved_via`
/// ([`ModelsSource`](crate::control::models::ModelsSource)), so an agent reads
/// one word for "a running router answered" across both actions.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedVia {
    /// The running daemon resolved it, so the chain reflects `reload`s and
    /// subscription-backed providers that static config alone cannot resolve.
    /// Serialized as `live`.
    ///
    /// The daemon resolves Stage 0 and the provider chain, but does not execute
    /// a dynamic policy decision. A policy-bound router reports its base model
    /// and candidate set without claiming one candidate was selected.
    Live,
    /// Resolved from a `bitrouter.yaml` on disk, policy table included.
    Config,
    /// Resolved from the built-in zero-config defaults — no config file was
    /// found. Serialized as `zero_config`.
    ZeroConfig,
}

/// One hop of the resolved fallback chain: which provider, under which upstream
/// id, over which wire protocol.
///
/// Deliberately three fields and no more. The resolved routing target also
/// carries the provider's credential; naming only the routable identity is what
/// keeps this report safe to hand to an agent.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderHop {
    /// The configured provider id, e.g. `"openai"`.
    pub provider: String,
    /// The id the provider itself knows the model by.
    pub service_id: String,
    /// The wire protocol BitRouter speaks to it, e.g. `"openai"`,
    /// `"anthropic"`.
    pub api_protocol: String,
}

/// What the static policy table decided, and why.
///
/// Present only on the config paths ([`ResolvedVia::Config`] /
/// [`ResolvedVia::ZeroConfig`]) and only when a policy table is configured. The
/// `static_*` fields are what the table declares for the request key; the
/// `selected_*` fields are what it actually chose, which can differ when a
/// route is pinned, locked, or under trial.
///
/// The informative, secret-free subset of the router's own decision: enough for
/// a reader to see *why* the effective model differs from the requested one,
/// without the snapshot internals that carry no meaning outside the router.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PolicySelection {
    /// The key the policy table matched the request on.
    pub request_key: String,
    /// Human-readable reason the decision came out this way.
    pub reason: String,
    /// The tier the table statically declares for `request_key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_tier: Option<String>,
    /// The model the table statically declares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_model: Option<String>,
    /// The reasoning effort the table statically declares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_effort: Option<ReasoningEffort>,
    /// The tier actually selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_tier: Option<String>,
    /// The model actually selected — this is what got routed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_model: Option<String>,
    /// The reasoning effort actually selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_effort: Option<ReasoningEffort>,
    /// Whether the route is pinned to its current selection.
    pub pinned: bool,
    /// Whether the selection is locked against further movement.
    pub locked: bool,
    /// Whether this request is part of an exploratory trial.
    pub trialed: bool,
}

/// A per-token rate bracket. The base rates live on [`EstimatedCost`]; this is
/// one of the steeper long-context brackets above it.
///
/// A step function, not graduated margins: once a request's input token count
/// exceeds [`Self::above_input_tokens`], this bracket's rates apply to the
/// **whole** request.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ContextTierRates {
    /// Exclusive lower bound on input tokens. A request strictly above this
    /// enters the bracket.
    pub above_input_tokens: u64,
    /// Micro-USD per input token inside this bracket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_micro_usd_per_token: Option<f64>,
    /// Micro-USD per output token inside this bracket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_micro_usd_per_token: Option<f64>,
}

/// The registry's per-token rates for the chain's first hop.
///
/// A *rate card*, not a total: nothing has been sent, so there are no token
/// counts to multiply by. [`Self::context_tiers`] is what keeps the card
/// honest for tiered models — reporting only the base bracket understates a
/// long-context request, which is why the tiers ride along and the note says
/// how they apply.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EstimatedCost {
    /// Micro-USD per input token, base bracket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_micro_usd_per_token: Option<f64>,
    /// Micro-USD per output token, base bracket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_micro_usd_per_token: Option<f64>,
    /// Steeper long-context brackets above the base rates, lowest bound first.
    /// Empty (and omitted) for flat pricing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_tiers: Vec<ContextTierRates>,
    /// How to read the rates above. Derived from whether tiers are present, so
    /// the two cannot describe each other wrongly.
    pub note: String,
}

/// How BitRouter would route a request, without sending one.
///
/// The requested model and the *effective* one are separate fields because they
/// genuinely differ: the policy table can select a different model for the same
/// request key, and a report that carried only one of them would either hide
/// what the caller asked for or hide what would actually run.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RouteReport {
    /// The model the caller asked about.
    pub requested_model: String,
    /// The model used to build the displayed provider chain. For a
    /// policy-bound router whose decision was not executed, this is its base
    /// model; consult [`Self::policy_decision_executed`] before treating it as
    /// a selected policy target.
    pub effective_model: String,
    /// The reasoning effort the policy table selected, when it selected one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_effort: Option<ReasoningEffort>,
    /// Which path resolved the chain.
    pub resolved_via: ResolvedVia,
    /// The static policy decision behind [`Self::effective_model`]. `None` on
    /// [`ResolvedVia::Live`] (the daemon's `route` verb does not replay
    /// policy, so there is no decision to show) and wherever no policy table
    /// is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_decision: Option<PolicySelection>,
    /// Stable identity of the named router resolved during Stage 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub router: Option<crate::routing::RouterRequestIdentity>,
    /// Whether the router came from canonical configuration or legacy preset
    /// compatibility syntax.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub router_source: Option<crate::control::models::RouterSource>,
    /// Dynamic policy bound to the named router, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_policy: Option<String>,
    /// Whether this preview actually executed the policy decision represented
    /// by `effective_model` and `policy_decision`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_decision_executed: Option<bool>,
    /// Models the bound policy may select. Empty for direct/fixed routes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidate_models: Vec<String>,
    /// The resolved fallback chain, preferred hop first. An empty chain either
    /// means no provider declares the effective model or a bound dynamic policy
    /// was not executed; `bound_policy` and `policy_decision_executed`
    /// distinguish those cases.
    #[serde(default)]
    pub provider_chain: Vec<ProviderHop>,
    /// The first hop's rate card, when the registry prices it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost: Option<EstimatedCost>,
}

impl EstimatedCost {
    /// The base rates plus any higher brackets, with the note that matches
    /// them. A constructor rather than three literals because the note is part
    /// of the contract: a tiered card whose note only mentions base rates is
    /// the misleading shape this type exists to avoid.
    pub fn new(
        input_micro_usd_per_token: Option<f64>,
        output_micro_usd_per_token: Option<f64>,
        context_tiers: Vec<ContextTierRates>,
    ) -> Self {
        let note = if context_tiers.is_empty() {
            "base-bracket per-token rates from the registry; multiply by expected token counts"
        } else {
            "base-bracket per-token rates from the registry; context_tiers lists steeper \
             long-context brackets — each applies to the whole request once its input tokens \
             exceed above_input_tokens. Multiply by expected token counts."
        };
        Self {
            input_micro_usd_per_token,
            output_micro_usd_per_token,
            context_tiers,
            note: note.to_string(),
        }
    }
}
