//! The `route` action: *how would BitRouter route this, and what would it
//! cost?*
//!
//! One report type, shared by `bitrouter route` and the MCP `route_preview`
//! tool, so the CLI's `--json` and the tool's structured content are the same
//! bytes. Read-only by construction: it replays routing without sending
//! anything upstream, and the resolved targets' secrets (api keys) never enter
//! the report.
//!
//! The crate owns the types and the port; the implementation lives app-side,
//! where the policy table, the routing table and the pricing registry are.

use bitrouter_sdk::language_model::types::ReasoningEffort;

use crate::error::ToolError;

/// Arguments to the `route` action (the `route_preview` tool's parameters, and
/// what `bitrouter route` builds from its own flags).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct RouteInput {
    /// The model selector to resolve (as you'd pass to `complete`).
    pub model: String,
    /// Optional prompt text. Used to derive the agent-loop step the policy
    /// table keys on; omit for a bare model resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// Which path produced the chain.
///
/// This is not a provenance footnote: it says whether
/// [`RouteReport::policy_decision`] can be populated at all. The daemon applies
/// its own policy table upstream and returns only the resulting chain, so a
/// live-daemon answer has no separate *static* decision to show — the absence
/// of a decision there means "already applied", not "no policy configured".
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub enum ResolvedVia {
    /// The running daemon resolved it, so the chain reflects `reload`s and
    /// subscription-backed providers that static config alone cannot resolve.
    #[serde(rename = "live daemon")]
    LiveDaemon,
    /// Resolved from a `bitrouter.yaml` on disk, policy table included.
    #[serde(rename = "config")]
    Config,
    /// Resolved from the built-in zero-config defaults — no config file was
    /// found.
    #[serde(rename = "zero-config")]
    ZeroConfig,
}

/// One hop of the resolved fallback chain: which provider, under which upstream
/// id, over which wire protocol.
///
/// Deliberately three fields and no more. The resolved routing target also
/// carries the provider's credential; naming only the routable identity is what
/// keeps this report safe to hand to an agent.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
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
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
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
#[derive(
    Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
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
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
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

/// How BitRouter would route a request, without sending one.
///
/// The requested model and the *effective* one are separate fields because they
/// genuinely differ: the policy table can select a different model for the same
/// request key, and a report that carried only one of them would either hide
/// what the caller asked for or hide what would actually run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct RouteReport {
    /// The model the caller asked about.
    pub requested_model: String,
    /// The model that would actually be routed to — equal to
    /// [`Self::requested_model`] unless the policy table selected another.
    pub effective_model: String,
    /// The reasoning effort the policy table selected, when it selected one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_effort: Option<ReasoningEffort>,
    /// Which path resolved the chain.
    pub resolved_via: ResolvedVia,
    /// The static policy decision behind [`Self::effective_model`]. `None` on
    /// [`ResolvedVia::LiveDaemon`] (the daemon applied policy upstream) and
    /// wherever no policy table is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_decision: Option<PolicySelection>,
    /// The resolved fallback chain, preferred hop first. Empty means no
    /// provider declares the effective model.
    #[serde(default)]
    pub provider_chain: Vec<ProviderHop>,
    /// The first hop's rate card, when the registry prices it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost: Option<EstimatedCost>,
}

/// The `route` port.
///
/// No `CallerAuth`: unlike `status` or `complete`, this reaches no upstream and
/// reads no per-caller state — it resolves *this machine's* routing table,
/// which is also why the tool is fenced out of the multi-tenant HTTP profile.
#[async_trait::async_trait]
pub trait RouteQuery: Send + Sync {
    /// Resolve `input` against the live daemon, else this machine's config, or
    /// a `ToolError` when the model does not resolve at all.
    async fn route(&self, input: RouteInput) -> Result<RouteReport, ToolError>;
}
