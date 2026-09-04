//! The `route` action, implemented over the live daemon with a config
//! fallback.
//!
//! One implementation, two surfaces: `bitrouter route <model>` calls
//! [`RouteAction::report`] directly, and the origin MCP server's
//! `route_preview` tool calls it through the [`RouteQuery`] port. Both get the
//! same [`RouteReport`], so the CLI's `--json` and the tool's structured
//! content cannot drift — and, more to the point, they cannot *disagree*: the
//! config fallback runs the policy table on both, so neither surface names a
//! model the daemon would never pick where the other would not.
//!
//! The live-daemon path is the honest exception, on both surfaces alike: the
//! daemon's `route` verb resolves the requested model as given, because its
//! policy table runs on real requests rather than on this preview. That path
//! reports `effective_model == requested_model` and no `policy_decision`, and
//! says so through `resolved_via`.
//!
//! Read-only throughout. Routing is replayed, never performed: nothing is sent
//! upstream, and the resolved targets' secrets (api keys) never enter the
//! report — only the provider id, the upstream service id, and the wire
//! protocol do.
//!
//! **Config is resolved per call, not snapshotted.** A long-lived `mcp serve`
//! answers from whatever `bitrouter.yaml` says *now*; the CLI, which loads the
//! file on every invocation, always did. Snapshotting at server start was the
//! stale-preview bug this action closes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bitrouter_mcp::actions::route::{
    ContextTierRates, EstimatedCost, PolicySelection, ProviderHop, ResolvedVia, RouteInput,
    RouteQuery, RouteReport,
};
use bitrouter_mcp::error::ToolError;
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
        let live = self
            .socket
            .as_ref()
            .filter(|s| crate::daemon::endpoint_in_use(s));
        if let Some(socket) = live
            && let Some(report) = self.via_daemon(socket, &input.model).await?
        {
            return Ok(report);
        }
        self.via_config(input).await
    }

    /// Ask the running daemon to resolve the model.
    ///
    /// `Ok(None)` means "ask the config instead": the socket file exists but
    /// this process could not get an answer out of it. Only the daemon
    /// *refusing* the model is a real error — it resolved, and said no.
    async fn via_daemon(&self, socket: &Path, model: &str) -> Result<Option<RouteReport>> {
        match crate::daemon::send_command(
            socket,
            &DaemonCommand::Route {
                model: model.to_string(),
            },
        )
        .await
        {
            Ok(DaemonResponse::Route { chain }) => Ok(Some(assemble(
                model,
                model,
                None,
                ResolvedVia::Live,
                // The daemon's `route` verb resolves the model as given — its
                // policy table runs on real requests, not here — so there is
                // no decision to surface, and the effective model is the
                // requested one. `ResolvedVia::Live` documents this.
                None,
                &chain,
                &self.pricing().await,
            ))),
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
    /// The policy table is the half `bitrouter route` used to skip, which is
    /// how it could name a model the daemon would never pick. It runs here for
    /// both surfaces: the effective model is what the table selects, and the
    /// chain is resolved for *that*, not for what was asked.
    async fn via_config(&self, input: RouteInput) -> Result<RouteReport> {
        let resolved = self.resolved_config().await?;
        let pricing = crate::assemble::build_pricing_table(&resolved);
        let policy = PolicyTableRouter::from_config(&resolved.policy_table);
        let table = ConfigRoutingTable::from_config(resolved);

        let prompt = probe_prompt(&input);
        let decision = policy.map(|p| p.decision_for(&prompt, &HeaderMap::new()));
        let effective_model = decision
            .as_ref()
            .and_then(|d| d.selected_model.clone())
            .unwrap_or_else(|| input.model.clone());
        let effective_effort = decision.as_ref().and_then(|d| d.selected_effort);
        let chain: Vec<RouteHop> = table
            .route_chain(
                &effective_model,
                &RoutingPrefs::default(),
                &CallerContext::local(),
            )
            .await
            .with_context(|| format!("resolving model '{effective_model}'"))?
            .into_iter()
            .map(|t| RouteHop {
                provider: t.provider_name,
                service_id: t.service_id,
                api_protocol: format!("{:?}", t.api_protocol).to_lowercase(),
            })
            .collect();
        Ok(assemble(
            &input.model,
            &effective_model,
            effective_effort,
            // A file on disk and the built-in zero-config defaults resolve the
            // same way but are not the same answer: one is what the user wrote.
            if self.source.is_default() {
                ResolvedVia::ZeroConfig
            } else {
                ResolvedVia::Config
            },
            decision.as_ref(),
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
    effective_effort: Option<bitrouter_sdk::language_model::types::ReasoningEffort>,
    resolved_via: ResolvedVia,
    decision: Option<&PolicyDecision>,
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
        effective_effort,
        resolved_via,
        policy_decision: decision.map(policy_selection),
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

    /// The whole point of the phase: both surfaces answer with the same bytes.
    /// `bitrouter route` goes through `report()`; the MCP `route_preview` tool
    /// goes through the `RouteQuery` port.
    #[tokio::test]
    async fn both_surfaces_produce_the_same_report() {
        let dir = tempfile::tempdir().expect("tempdir");
        let action = RouteAction::new(config_source(dir.path(), ONE_MODEL), None);
        let input = RouteInput {
            model: "demo-model".to_string(),
            prompt: Some("hello".to_string()),
        };

        let cli = action.report(input.clone()).await.expect("cli surface");
        let tool = RouteQuery::route(&action, input)
            .await
            .expect("mcp surface");

        assert_eq!(
            serde_json::to_value(&cli).expect("cli json"),
            serde_json::to_value(&tool).expect("mcp json"),
            "the two surfaces of one action must be the same bytes"
        );
        assert_eq!(cli.requested_model, "demo-model");
        assert_eq!(cli.resolved_via, ResolvedVia::Config);
        assert_eq!(cli.provider_chain.len(), 1);
        assert_eq!(cli.provider_chain[0].provider, "demo");
        assert_eq!(cli.provider_chain[0].service_id, "demo-model");
    }

    /// The disagreement this phase resolves: `bitrouter route` used to skip the
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
        let tool = RouteQuery::route(&action, input)
            .await
            .expect("mcp surface");

        for (surface, report) in [("cli", &cli), ("mcp", &tool)] {
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
            serde_json::to_value(&tool).expect("mcp json"),
        );
    }

    /// The staleness bug: one long-lived action (as `mcp serve` holds) must see
    /// a `bitrouter.yaml` edited between two calls, exactly as a fresh CLI
    /// invocation would. Snapshotting config at construction is what made the
    /// MCP tool answer from a startup-time view of the world.
    #[tokio::test]
    async fn an_edited_config_is_visible_to_the_next_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = config_source(dir.path(), ONE_MODEL);
        // Built once and kept behind the port, exactly as `mcp serve` holds it
        // for the life of the process — not rebuilt per call.
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
            "the same long-lived server must see the edited config, not a \
             snapshot taken at `mcp serve` start"
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
