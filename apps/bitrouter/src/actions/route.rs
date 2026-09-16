//! The `route` action: *how would BitRouter route this, and what would it
//! cost?*
//!
//! One report type is shared by `bro route`, Code sessions, and typed remote
//! control so all retained surfaces preserve the same JSON shape. Read-only by
//! construction: it replays routing without sending
//! anything upstream, and the resolved targets' secrets (api keys) never enter
//! the report.
//!
//! The app owns the types, port, and implementation beside the policy table,
//! routing table, and pricing registry.

use bitrouter_sdk::language_model::types::ReasoningEffort;

use super::ToolError;

/// Arguments to the `route` action.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
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
/// ([`ModelsSource`](crate::actions::models::ModelsSource)), so an agent reads
/// one word for "a running router answered" across both actions.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
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
    pub router: Option<bitrouter_sdk::language_model::routing::RouterRequestIdentity>,
    /// Whether the router came from canonical configuration or legacy preset
    /// compatibility syntax.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub router_source: Option<crate::actions::models::RouterSource>,
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

/// The `route` port.
///
/// This reaches no upstream and reads no per-caller state: it resolves this
/// machine's routing table.
#[async_trait::async_trait]
pub trait RouteQuery: Send + Sync {
    /// Resolve `input` against the live daemon, else this machine's config, or
    /// a `ToolError` when the model does not resolve at all.
    async fn route(&self, input: RouteInput) -> Result<RouteReport, ToolError>;
}

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bitrouter_sdk::HeaderMap;
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::config::{Config, ConfigRoutingTable};
use bitrouter_sdk::language_model::types::{
    GenerationParams, Message, Prompt, ProviderMetadata, Role,
};
use bitrouter_sdk::language_model::{RoutingPrefs, RoutingTable};

use crate::daemon::{DaemonCommand, DaemonResponse, RouteHop};
use crate::metering::PricingTable;
use crate::paths::ConfigSource;
use crate::policy_table_router::{PolicyDecision, PolicyTableRouter};

#[derive(Default)]
struct RouteMetadata {
    effective_effort: Option<bitrouter_sdk::language_model::types::ReasoningEffort>,
    router: Option<bitrouter_sdk::language_model::routing::RouterRequestIdentity>,
    router_source: Option<crate::actions::models::RouterSource>,
    bound_policy: Option<String>,
    policy_decision_executed: Option<bool>,
    candidate_models: Vec<String>,
}

pub(crate) fn router_source(
    config: &Config,
    router_id: &str,
) -> Option<crate::actions::models::RouterSource> {
    let source = config
        .router_inventory()
        .ok()?
        .into_iter()
        .find(|entry| entry.id == router_id)?
        .source;
    Some(match source {
        bitrouter_sdk::config::router::RouterConfigSource::User => {
            crate::actions::models::RouterSource::User
        }
        bitrouter_sdk::config::router::RouterConfigSource::LegacyPreset => {
            crate::actions::models::RouterSource::Legacy
        }
    })
}

pub(crate) fn policy_candidate_models(
    policy: Option<&crate::actions::administration::PolicyReport>,
    policy_name: Option<&str>,
    base_model: &str,
) -> Vec<String> {
    let Some(policy_name) = policy_name else {
        return Vec::new();
    };
    let mut candidates = std::collections::BTreeSet::from([base_model.to_string()]);
    if let Some(definition) = policy.and_then(|report| report.definitions.get(policy_name)) {
        candidates.extend(
            definition
                .tiers
                .values()
                .map(|target| target.model().to_string()),
        );
    }
    candidates.into_iter().collect()
}

pub(crate) async fn resolvable_policy_candidates(
    table: &dyn RoutingTable,
    candidates: Vec<String>,
    prefs: &RoutingPrefs,
) -> Vec<String> {
    let mut resolved = Vec::new();
    for candidate in candidates {
        if table
            .route_resolved(&candidate, prefs, &CallerContext::local())
            .await
            .is_ok_and(|chain| !chain.is_empty())
        {
            resolved.push(candidate);
        }
    }
    resolved
}

/// Resolves a model against the daemon's live routing table, falling back to
/// this machine's config.
///
/// Holds the *source* of the config rather than a parsed snapshot of it: see
/// the module docs. The cost is one file read and one table build per call,
/// which is the price of answering the question that was asked instead of the
/// one that was true at startup.
pub struct RouteAction {
    source: ConfigSource,
    /// The daemon control socket, when known. Resolution prefers the live
    /// daemon — it reflects `reload`s and subscription-backed providers that
    /// static config alone cannot resolve — and falls back to config when the
    /// socket is unreachable.
    socket: Option<PathBuf>,
}

impl RouteAction {
    /// Resolve against `source`, preferring the daemon on `socket`.
    ///
    /// `socket` is `Option` because the daemon path is a preference, not a
    /// dependency: a caller that could not work out where the control socket
    /// lives gets config resolution, never a failure.
    pub fn new(source: ConfigSource, socket: Option<PathBuf>) -> Self {
        Self { source, socket }
    }

    /// Resolve `input`, daemon-first.
    pub async fn report(&self, input: RouteInput) -> Result<RouteReport> {
        // A reachable daemon, if we have one; `None` (unset socket or daemon
        // down) skips straight to config resolution below.
        if let Some(socket) = self.socket.as_ref()
            && let Some(report) = self.via_daemon(socket, &input.model).await?
        {
            return Ok(report);
        }
        self.via_config(input).await
    }

    /// Ask the running daemon to resolve the model.
    ///
    /// `Ok(None)` means "ask the config instead": this process could not get
    /// an answer from the configured endpoint. Only the daemon *refusing* the
    /// model is a real error — it resolved, and said no.
    async fn via_daemon(&self, socket: &Path, model: &str) -> Result<Option<RouteReport>> {
        match crate::daemon::send_command(
            socket,
            &DaemonCommand::Route {
                model: model.to_string(),
            },
        )
        .await
        {
            Ok(DaemonResponse::Route {
                chain,
                resolved_model,
                router,
                router_source,
                bound_policy,
                candidate_models,
            }) => {
                let effective_model = resolved_model.as_deref().unwrap_or(model);
                let policy_decision_executed = bound_policy.as_ref().map(|_| false);
                Ok(Some(assemble(
                    model,
                    effective_model,
                    ResolvedVia::Live,
                    None,
                    RouteMetadata {
                        effective_effort: None,
                        router,
                        router_source,
                        bound_policy,
                        policy_decision_executed,
                        candidate_models,
                    },
                    &chain,
                    &self.pricing().await,
                )))
            }
            Ok(DaemonResponse::Error { message }) => {
                Err(anyhow::anyhow!("resolving model '{model}': {message}"))
            }
            Ok(other) => {
                tracing::debug!(response = ?other, "unexpected daemon route response — resolving from config");
                Ok(None)
            }
            Err(e) => {
                tracing::debug!(error = %e, "daemon route failed — resolving from config");
                Ok(None)
            }
        }
    }

    /// Resolve from this machine's config, policy table included.
    ///
    /// The policy table is the half `bro route` used to skip, which is
    /// how it could name a model the daemon would never pick. It runs here for
    /// both surfaces: the effective model is what the table selects, and the
    /// chain is resolved for *that*, not for what was asked.
    async fn via_config(&self, input: RouteInput) -> Result<RouteReport> {
        let resolved = self.resolved_config().await?;
        let pricing = crate::assemble::build_pricing_table(&resolved);
        let policy = PolicyTableRouter::from_config(&resolved.policy_table);
        let table = ConfigRoutingTable::from_config(resolved.clone());
        let resolution = table
            .resolve_model(&input.model)
            .await
            .with_context(|| format!("resolving model '{}'", input.model))?;
        let router_source = resolution
            .router
            .as_ref()
            .and_then(|identity| router_source(&resolved, &identity.router_id));
        let bound_policy = resolution.policy.clone();

        let prompt = probe_prompt(&input);
        // Named adaptive policies need request/session state owned by the live
        // runtime. A preview resolves their base route and lists candidates,
        // but never fabricates a decision. The legacy global policy table is
        // deterministic and remains replayable for routes without a binding.
        let decision = if bound_policy.is_some() {
            None
        } else {
            policy.map(|p| p.decision_for(&prompt, &HeaderMap::new()))
        };
        let effective_model = decision
            .as_ref()
            .and_then(|d| d.selected_model.clone())
            .unwrap_or_else(|| resolution.clean_model.clone());
        let effective_effort = decision.as_ref().and_then(|d| d.selected_effort);
        let policy_report = if bound_policy.is_some() {
            crate::actions::administration::disk_policy(&self.source)
                .await
                .ok()
        } else {
            None
        };
        let candidate_models = policy_candidate_models(
            policy_report.as_ref(),
            bound_policy.as_deref(),
            &resolution.clean_model,
        );
        let candidate_models =
            resolvable_policy_candidates(&table, candidate_models, &resolution.prefs).await;
        let chain: Vec<RouteHop> = if bound_policy.is_some() {
            Vec::new()
        } else {
            table
                .route_resolved(&effective_model, &resolution.prefs, &CallerContext::local())
                .await
                .with_context(|| format!("resolving model '{effective_model}'"))?
                .into_iter()
                .map(|t| RouteHop {
                    provider: t.provider_name,
                    service_id: t.service_id,
                    api_protocol: format!("{:?}", t.api_protocol).to_lowercase(),
                })
                .collect()
        };
        let policy_decision_executed = bound_policy
            .as_ref()
            .map(|_| false)
            .or_else(|| decision.as_ref().map(|_| true));
        Ok(assemble(
            &input.model,
            &effective_model,
            // A file on disk and the built-in zero-config defaults resolve the
            // same way but are not the same answer: one is what the user wrote.
            if self.source.is_default() {
                ResolvedVia::ZeroConfig
            } else {
                ResolvedVia::Config
            },
            decision.as_ref(),
            RouteMetadata {
                effective_effort,
                router: resolution.router,
                router_source,
                bound_policy,
                policy_decision_executed,
                candidate_models,
            },
            &chain,
            &pricing,
        ))
    }

    /// This call's config, resolved the way the daemon resolves its own at
    /// start-up (built-in defaults, then stored-credential activation), so a
    /// zero-config built-in and a subscription-backed provider both resolve.
    async fn resolved_config(&self) -> Result<Config> {
        Ok(crate::commands::resolve_static(
            crate::paths::load_config(&self.source).await?,
        ))
    }

    /// The pricing table for the daemon path, which otherwise needs no config.
    ///
    /// Best-effort: an unreadable config costs the rate card, not the report.
    /// The daemon answered the routing question, and a preview with no
    /// `estimated_cost` is still the right answer to it.
    async fn pricing(&self) -> PricingTable {
        match self.resolved_config().await {
            Ok(config) => crate::assemble::build_pricing_table(&config),
            Err(e) => {
                tracing::debug!(error = %e, "no config for pricing — reporting the chain unpriced");
                PricingTable::default()
            }
        }
    }
}

/// Assemble the report from a resolved hop chain, pricing the top hop.
///
/// A free function, not a method: it reads nothing from the action's own state,
/// and both resolution paths hand it everything explicitly — which is what makes
/// the daemon path's "no static decision" a visible argument rather than a
/// silent default.
fn assemble(
    requested_model: &str,
    effective_model: &str,
    resolved_via: ResolvedVia,
    decision: Option<&PolicyDecision>,
    metadata: RouteMetadata,
    chain: &[RouteHop],
    pricing: &PricingTable,
) -> RouteReport {
    let estimated_cost = chain
        .first()
        .and_then(|h| pricing.resolve(&h.provider, &h.service_id))
        .filter(|p| !p.is_unconfigured())
        .map(|p| estimated_cost(&p));
    RouteReport {
        requested_model: requested_model.to_string(),
        effective_model: effective_model.to_string(),
        effective_effort: metadata.effective_effort,
        resolved_via,
        policy_decision: decision.map(policy_selection),
        router: metadata.router,
        router_source: metadata.router_source,
        bound_policy: metadata.bound_policy,
        policy_decision_executed: metadata.policy_decision_executed,
        candidate_models: metadata.candidate_models,
        provider_chain: chain
            .iter()
            .map(|h| ProviderHop {
                provider: h.provider.clone(),
                service_id: h.service_id.clone(),
                api_protocol: h.api_protocol.clone(),
            })
            .collect(),
        estimated_cost,
    }
}

/// A probe prompt for the preview: the requested model plus, when given, the
/// prompt text as a single user turn (so the policy fingerprint reflects an
/// opening request for that model).
fn probe_prompt(input: &RouteInput) -> Prompt {
    let messages = match &input.prompt {
        Some(text) => vec![Message::text(Role::User, text.clone())],
        None => Vec::new(),
    };
    Prompt {
        model: input.model.clone(),
        system: None,
        system_provider_metadata: ProviderMetadata::new(),
        messages,
        tools: Vec::new(),
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    }
}

/// The top hop's rate card: the base per-token rates plus, for tiered models,
/// the higher long-context brackets so the preview isn't misleading (PR-2
/// review finding 3 — reporting only the base rates understates a long-context
/// request, which bills at the steeper bracket). The explanatory note is chosen
/// by [`EstimatedCost::new`] from whether tiers are present, so the two cannot
/// describe each other wrongly.
fn estimated_cost(p: &crate::metering::pricing::ModelPricing) -> EstimatedCost {
    EstimatedCost::new(
        p.input_micro_usd_per_token,
        p.output_micro_usd_per_token,
        p.context_tiers
            .iter()
            .map(|t| ContextTierRates {
                above_input_tokens: t.above_input_tokens,
                input_micro_usd_per_token: t.input_micro_usd_per_token,
                output_micro_usd_per_token: t.output_micro_usd_per_token,
            })
            .collect(),
    )
}

/// The informative, secret-free subset of a [`PolicyDecision`].
fn policy_selection(d: &PolicyDecision) -> PolicySelection {
    PolicySelection {
        request_key: d.request_key.clone(),
        reason: d.reason.to_string(),
        static_tier: d.static_tier.clone(),
        static_model: d.static_model.clone(),
        static_effort: d.static_effort,
        selected_tier: d.selected_tier.clone(),
        selected_model: d.selected_model.clone(),
        selected_effort: d.selected_effort,
        pinned: d.pinned,
        locked: d.locked,
        trialed: d.trialed,
    }
}

#[async_trait::async_trait]
impl RouteQuery for RouteAction {
    async fn route(&self, input: RouteInput) -> Result<RouteReport, ToolError> {
        self.report(input)
            .await
            .map_err(|e| ToolError::new(format!("{e:#}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write `yaml` as a `bitrouter.yaml` in a fresh temp dir and return the
    /// source pointing at it.
    fn config_source(dir: &Path, yaml: &str) -> ConfigSource {
        let path = dir.join("bitrouter.yaml");
        std::fs::write(&path, yaml).expect("write config");
        ConfigSource::File(path)
    }

    /// One active provider declaring one model — enough for the routing table
    /// to resolve a chain.
    const ONE_MODEL: &str = r#"
providers:
  demo:
    api_base: https://api.example.test
    api_key: sk-test
    active: true
    models:
      - id: demo-model
"#;

    /// Two models, with a policy table that routes every request to the
    /// *second* one. What the caller asks for and what would run differ.
    const POLICY_REDIRECTS: &str = r#"
providers:
  demo:
    api_base: https://api.example.test
    api_key: sk-test
    active: true
    models:
      - id: demo-model
      - id: demo-model-big
policy_table:
  tiers:
    big:
      model: demo-model-big
      effort: high
  default_tier: big
"#;

    /// The direct path and injected port answer with the same bytes.
    #[tokio::test]
    async fn both_surfaces_produce_the_same_report() {
        let dir = tempfile::tempdir().expect("tempdir");
        let action = RouteAction::new(config_source(dir.path(), ONE_MODEL), None);
        let input = RouteInput {
            model: "demo-model".to_string(),
            prompt: Some("hello".to_string()),
        };

        let cli = action.report(input.clone()).await.expect("cli surface");
        let port = RouteQuery::route(&action, input)
            .await
            .expect("port surface");

        assert_eq!(
            serde_json::to_value(&cli).expect("cli json"),
            serde_json::to_value(&port).expect("port json"),
            "the two surfaces of one action must be the same bytes"
        );
        assert_eq!(cli.requested_model, "demo-model");
        assert_eq!(cli.resolved_via, ResolvedVia::Config);
        assert_eq!(cli.provider_chain.len(), 1);
        assert_eq!(cli.provider_chain[0].provider, "demo");
        assert_eq!(cli.provider_chain[0].service_id, "demo-model");
    }

    /// The disagreement this phase resolves: `bro route` used to skip the
    /// policy table, so it named the requested model while the daemon would
    /// have run another one. Both surfaces now run it, and both say so.
    #[tokio::test]
    async fn both_surfaces_apply_the_policy_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let action = RouteAction::new(config_source(dir.path(), POLICY_REDIRECTS), None);
        let input = RouteInput {
            model: "demo-model".to_string(),
            prompt: Some("write me a function".to_string()),
        };

        let cli = action.report(input.clone()).await.expect("cli surface");
        let port = RouteQuery::route(&action, input)
            .await
            .expect("port surface");

        for (surface, report) in [("cli", &cli), ("port", &port)] {
            assert_eq!(report.requested_model, "demo-model", "{surface}");
            assert_eq!(
                report.effective_model, "demo-model-big",
                "{surface} surface ignored the policy table — it would name a model the \
                 daemon would never pick"
            );
            // The chain is resolved for the *effective* model, not the asked-for
            // one: a preview that priced the wrong model would be worse than
            // none.
            assert_eq!(report.provider_chain[0].service_id, "demo-model-big");
            let decision = report
                .policy_decision
                .as_ref()
                .unwrap_or_else(|| panic!("{surface} surface reported no policy decision"));
            assert_eq!(decision.selected_model.as_deref(), Some("demo-model-big"));
        }
        assert_eq!(
            serde_json::to_value(&cli).expect("cli json"),
            serde_json::to_value(&port).expect("port json"),
        );
    }

    #[tokio::test]
    async fn policy_router_preview_lists_candidates_without_fabricating_a_decision()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let source = config_source(
            dir.path(),
            r#"
providers:
  demo:
    api_base: https://api.example.test
    api_key: sk-test
    active: true
    models:
      - id: demo-model
      - id: demo-model-big
routers:
  coding:
    selection:
      kind: policy
      policy: coding
      base_model: demo-model
"#,
        );
        std::fs::write(
            dir.path().join("policy-lock.yaml"),
            r#"lockfileVersion: 1
policies:
  coding:
    key_strategy: agent_trace
    tiers:
      strong: demo-model-big
    routes: {}
    default_tier: strong
    tool_use_tier: strong
    tool_safe_tiers: [strong]
"#,
        )?;

        let report = RouteAction::new(source, None)
            .report(RouteInput {
                model: "bitrouter/coding".to_string(),
                prompt: Some("write a function".to_string()),
            })
            .await?;

        assert_eq!(report.effective_model, "demo-model");
        assert_eq!(report.bound_policy.as_deref(), Some("coding"));
        assert_eq!(report.policy_decision_executed, Some(false));
        assert!(report.policy_decision.is_none());
        assert_eq!(
            report
                .router
                .as_ref()
                .map(|router| router.router_id.as_str()),
            Some("coding")
        );
        assert_eq!(
            report.router_source,
            Some(crate::actions::models::RouterSource::User)
        );
        assert_eq!(
            report.candidate_models,
            ["demo-model".to_string(), "demo-model-big".to_string()]
        );
        assert!(
            report.provider_chain.is_empty(),
            "without a policy decision no provider chain is selected"
        );
        Ok(())
    }

    /// The staleness bug: one long-lived action must see
    /// a `bitrouter.yaml` edited between two calls, exactly as a fresh CLI
    /// invocation would. Snapshotting config at construction is what made the
    /// consumer answer from a startup-time view of the world.
    #[tokio::test]
    async fn an_edited_config_is_visible_to_the_next_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = config_source(dir.path(), ONE_MODEL);
        // Built once and kept behind the port for the life of the process —
        // not rebuilt per call.
        let served: std::sync::Arc<dyn RouteQuery> =
            std::sync::Arc::new(RouteAction::new(source, None));
        let ask = || RouteInput {
            model: "demo-model".to_string(),
            prompt: None,
        };

        let before = served.route(ask()).await.expect("resolves");
        assert_eq!(before.effective_model, "demo-model");
        assert!(before.policy_decision.is_none(), "no policy table yet");

        // The user edits the file the server was started against.
        config_source(dir.path(), POLICY_REDIRECTS);

        let after = served
            .route(ask())
            .await
            .expect("resolves against the edited config");
        assert_eq!(
            after.effective_model, "demo-model-big",
            "the same long-lived action must see the edited config, not a \
             snapshot taken at construction"
        );
    }

    /// The daemon path is a preference, not a dependency: a socket path that
    /// nothing is listening on falls straight through to config resolution
    /// rather than stalling or erroring.
    #[tokio::test]
    async fn a_dead_daemon_socket_falls_back_to_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dead = dir.path().join("nothing-listening.sock");
        let action = RouteAction::new(config_source(dir.path(), ONE_MODEL), Some(dead));
        let report = action
            .report(RouteInput {
                model: "demo-model".to_string(),
                prompt: None,
            })
            .await
            .expect("resolves via config fallback");
        assert_eq!(report.resolved_via, ResolvedVia::Config);
        assert_eq!(report.provider_chain[0].provider, "demo");
    }

    #[tokio::test]
    async fn an_unroutable_model_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let action = RouteAction::new(config_source(dir.path(), ONE_MODEL), None);
        let err = RouteQuery::route(
            &action,
            RouteInput {
                model: "nonexistent-model".to_string(),
                prompt: None,
            },
        )
        .await
        .expect_err("an unroutable model surfaces a ToolError");
        assert!(
            err.0.contains("nonexistent-model"),
            "the error names the unresolved model: {}",
            err.0
        );
    }

    /// A tiered model's higher brackets ride along (PR-2 finding 3): a preview
    /// that showed only the base rates would understate a long-context request.
    #[test]
    fn the_rate_card_surfaces_context_tiers() {
        use crate::metering::pricing::{ContextTier, ModelPricing};

        let flat = estimated_cost(&ModelPricing::new(1.0, 2.0));
        assert!(flat.context_tiers.is_empty());
        assert_eq!(flat.input_micro_usd_per_token, Some(1.0));
        assert!(!flat.note.contains("context_tiers"), "note: {}", flat.note);

        let tiered = estimated_cost(&ModelPricing {
            input_micro_usd_per_token: Some(1.0),
            output_micro_usd_per_token: Some(2.0),
            cache_read_micro_usd_per_token: None,
            cache_write_micro_usd_per_token: None,
            context_tiers: vec![ContextTier {
                above_input_tokens: 200_000,
                input_micro_usd_per_token: Some(2.0),
                output_micro_usd_per_token: Some(4.0),
                cache_read_micro_usd_per_token: None,
                cache_write_micro_usd_per_token: None,
            }],
        });
        assert_eq!(tiered.context_tiers.len(), 1);
        assert_eq!(tiered.context_tiers[0].above_input_tokens, 200_000);
        assert_eq!(tiered.context_tiers[0].input_micro_usd_per_token, Some(2.0));
        assert!(
            tiered.note.contains("context_tiers"),
            "the note has to explain the brackets: {}",
            tiered.note
        );
    }
}
