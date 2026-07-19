//! The routing-preview adapter — the app side of the orchestrator profile's
//! `route_preview` tool (TUI_SPEC §4, PR-2 B1).
//!
//! Implements `bitrouter-mcp`'s
//! [`RoutingQuery`] port by
//! replaying BitRouter's *real* routing over a probe prompt — the policy-table
//! decision, the resolved provider fallback chain, and the registry's per-token
//! rates for the top hop. Read-only: nothing is sent upstream, and the
//! resolved targets' secrets (api keys) are never surfaced.

use std::path::PathBuf;

use anyhow::{Context, Result};
use bitrouter_mcp::capabilities::routing::{RoutePreviewArgs, RoutingQuery};
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
use crate::policy_table_router::{PolicyDecision, PolicyTableRouter};

/// Previews routing over a snapshot of the daemon's routing/policy/pricing
/// tables. Built once at bridge start; every `route_preview` reuses it.
pub struct RoutingPreview {
    table: ConfigRoutingTable,
    policy: Option<PolicyTableRouter>,
    pricing: PricingTable,
    /// The daemon control socket, when known. `route_preview` resolves through
    /// the live daemon first (like `bitrouter route`), so it reflects `reload`s
    /// and subscription-backed providers the static config alone can't resolve;
    /// it falls back to config resolution when the daemon is unreachable.
    socket: Option<PathBuf>,
}

impl RoutingPreview {
    /// Snapshot the routing surface from `config`: apply the built-in provider
    /// defaults first (so a zero-config built-in still resolves), then build the
    /// routing table, the static policy table, and the pricing table — the
    /// tables used for the config-fallback path. `socket` is the daemon control
    /// socket used for the (preferred) live-daemon path.
    pub fn new(config: &Config, socket: Option<PathBuf>) -> Self {
        let mut resolved = config.clone();
        bitrouter_providers::apply_builtin_defaults(&mut resolved);
        let pricing = crate::assemble::build_pricing_table(&resolved);
        let policy = PolicyTableRouter::from_config(&resolved.policy_table);
        let table = ConfigRoutingTable::from_config(resolved);
        Self {
            table,
            policy,
            pricing,
            socket,
        }
    }

    /// A probe prompt for the preview: the requested model plus, when given, the
    /// prompt text as a single user turn (so the policy fingerprint reflects an
    /// opening request for that model).
    fn probe_prompt(&self, args: &RoutePreviewArgs) -> Prompt {
        let messages = match &args.prompt {
            Some(text) => vec![Message::text(Role::User, text.clone())],
            None => Vec::new(),
        };
        Prompt {
            model: args.model.clone(),
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

    async fn do_preview(&self, args: RoutePreviewArgs) -> Result<serde_json::Value> {
        // Daemon-first, mirroring `bitrouter route`: the live routing table
        // reflects `reload`s and subscription-backed providers (claude-code,
        // google-ai, …) that the static config alone can't resolve. Only when
        // the daemon isn't reachable do we fall back to config resolution.
        // A reachable daemon, if we have one; `None` (unset socket or daemon
        // down) skips straight to config resolution below.
        let live_socket = self
            .socket
            .as_ref()
            .filter(|s| crate::daemon::endpoint_in_use(s));
        if let Some(socket) = live_socket {
            match crate::daemon::send_command(
                socket,
                &DaemonCommand::Route {
                    model: args.model.clone(),
                },
            )
            .await
            {
                Ok(DaemonResponse::Route { chain }) => {
                    // The daemon applied its own policy to produce the chain, so
                    // there's no separate static decision to surface here.
                    return Ok(self.report(&args.model, &args.model, "live daemon", None, &chain));
                }
                Ok(DaemonResponse::Error { message }) => {
                    anyhow::bail!("resolving model '{}': {message}", args.model);
                }
                // An unexpected response or a transport error just falls back to
                // config resolution — the daemon may not be reachable from this
                // process even though the socket file exists.
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(error = %e, "daemon route_preview failed — resolving from config");
                }
            }
        }

        // Config fallback: the static policy table's view (the effective model it
        // selects can differ from the requested one) plus config resolution.
        let prompt = self.probe_prompt(&args);
        let decision = self
            .policy
            .as_ref()
            .map(|p| p.decision_for(&prompt, &HeaderMap::new()));
        let effective_model = decision
            .as_ref()
            .and_then(|d| d.selected_model.clone())
            .unwrap_or_else(|| args.model.clone());
        let chain: Vec<RouteHop> = self
            .table
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
        Ok(self.report(
            &args.model,
            &effective_model,
            "config",
            decision.as_ref(),
            &chain,
        ))
    }

    /// Assemble the preview JSON from a resolved hop chain (daemon- or
    /// config-sourced), pricing the top hop from the registry. `resolved_via`
    /// records which path produced the chain so the orchestrator knows whether
    /// it reflects the live daemon or a static config snapshot.
    fn report(
        &self,
        requested_model: &str,
        effective_model: &str,
        resolved_via: &str,
        decision: Option<&PolicyDecision>,
        chain: &[RouteHop],
    ) -> serde_json::Value {
        // The registry's per-token rates for the top hop, when priced. Surfaces
        // the base bracket *and* any higher context tiers (PR-2 review finding
        // 3: reporting only the base rates was misleading for tiered models —
        // long-context requests bill at the steeper bracket).
        let estimated_cost = chain
            .first()
            .and_then(|h| self.pricing.resolve(&h.provider, &h.service_id))
            .filter(|p| !p.is_unconfigured())
            .map(|p| pricing_json(&p));
        serde_json::json!({
            "requested_model": requested_model,
            "effective_model": effective_model,
            "resolved_via": resolved_via,
            "policy_decision": decision.map(decision_json),
            "provider_chain": chain
                .iter()
                .map(|h| serde_json::json!({
                    "provider": h.provider,
                    "service_id": h.service_id,
                    "api_protocol": h.api_protocol,
                }))
                .collect::<Vec<_>>(),
            "estimated_cost": estimated_cost,
        })
    }
}

/// The estimated-cost JSON for the top hop: the base per-token rates plus, for
/// tiered models, the higher long-context brackets so the preview isn't
/// misleading (PR-2 review finding 3). Each bracket applies to the whole
/// request once its input size crosses `above_input_tokens` (a step function).
fn pricing_json(p: &crate::metering::pricing::ModelPricing) -> serde_json::Value {
    let mut cost = serde_json::json!({
        "input_micro_usd_per_token": p.input_micro_usd_per_token,
        "output_micro_usd_per_token": p.output_micro_usd_per_token,
        "note": "base-bracket per-token rates from the registry; multiply by expected token counts",
    });
    if !p.context_tiers.is_empty() {
        cost["context_tiers"] = serde_json::Value::Array(
            p.context_tiers
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "above_input_tokens": t.above_input_tokens,
                        "input_micro_usd_per_token": t.input_micro_usd_per_token,
                        "output_micro_usd_per_token": t.output_micro_usd_per_token,
                    })
                })
                .collect(),
        );
        cost["note"] = serde_json::json!(
            "base-bracket per-token rates from the registry; context_tiers lists steeper \
             long-context brackets — each applies to the whole request once its input tokens \
             exceed above_input_tokens. Multiply by expected token counts."
        );
    }
    cost
}

/// The informative, secret-free subset of a [`PolicyDecision`].
fn decision_json(d: &PolicyDecision) -> serde_json::Value {
    serde_json::json!({
        "request_key": d.request_key,
        "reason": d.reason.to_string(),
        "static_tier": d.static_tier,
        "static_model": d.static_model,
        "selected_tier": d.selected_tier,
        "selected_model": d.selected_model,
        "pinned": d.pinned,
        "locked": d.locked,
        "trialed": d.trialed,
    })
}

#[async_trait::async_trait]
impl RoutingQuery for RoutingPreview {
    async fn preview(&self, args: RoutePreviewArgs) -> Result<serde_json::Value, ToolError> {
        self.do_preview(args)
            .await
            .map_err(|e| ToolError::new(format!("{e:#}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config with one active provider declaring one model — enough for the
    /// routing table to resolve a chain. Built through serde so it doesn't
    /// couple to the config structs' private fields.
    fn config_with_model() -> Config {
        serde_json::from_value(serde_json::json!({
            "providers": {
                "demo": {
                    "api_base": "https://api.example.test",
                    "api_key": "sk-test",
                    "active": true,
                    "models": [{ "id": "demo-model" }]
                }
            }
        }))
        .expect("valid config")
    }

    #[tokio::test]
    async fn preview_resolves_the_provider_chain() {
        // No daemon socket → the config-resolution path.
        let preview = RoutingPreview::new(&config_with_model(), None);
        let out = preview
            .do_preview(RoutePreviewArgs {
                model: "demo-model".to_string(),
                prompt: Some("hello".to_string()),
            })
            .await
            .expect("resolves");
        assert_eq!(out["requested_model"], "demo-model");
        assert_eq!(out["resolved_via"], "config");
        let chain = out["provider_chain"].as_array().expect("chain");
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0]["provider"], "demo");
        assert_eq!(chain[0]["service_id"], "demo-model");
    }

    #[tokio::test]
    async fn preview_falls_back_to_config_when_the_daemon_socket_is_dead() {
        // A socket path that isn't in use must not stall or error — it falls
        // straight through to config resolution (the daemon-first path is a
        // best-effort preference, not a hard dependency).
        let dead = std::env::temp_dir().join("bitrouter-nonexistent-route-preview.sock");
        let preview = RoutingPreview::new(&config_with_model(), Some(dead));
        let out = preview
            .do_preview(RoutePreviewArgs {
                model: "demo-model".to_string(),
                prompt: None,
            })
            .await
            .expect("resolves via config fallback");
        assert_eq!(out["resolved_via"], "config");
        assert_eq!(out["provider_chain"][0]["provider"], "demo");
    }

    #[test]
    fn pricing_json_surfaces_context_tiers() {
        use crate::metering::pricing::{ContextTier, ModelPricing};
        // Flat pricing: no `context_tiers` key, base note.
        let flat = pricing_json(&ModelPricing::new(1.0, 2.0));
        assert!(flat.get("context_tiers").is_none(), "flat: {flat}");
        assert_eq!(flat["input_micro_usd_per_token"], 1.0);

        // Tiered pricing: the higher bracket is surfaced (PR-2 finding 3), so a
        // long-context model's preview isn't misleading.
        let tiered = ModelPricing {
            input_micro_usd_per_token: Some(1.0),
            output_micro_usd_per_token: Some(2.0),
            context_tiers: vec![ContextTier {
                above_input_tokens: 200_000,
                input_micro_usd_per_token: Some(2.0),
                output_micro_usd_per_token: Some(4.0),
            }],
        };
        let out = pricing_json(&tiered);
        let tiers = out["context_tiers"].as_array().expect("tiers present");
        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0]["above_input_tokens"], 200_000);
        assert_eq!(tiers[0]["input_micro_usd_per_token"], 2.0);
        assert!(
            out["note"]
                .as_str()
                .is_some_and(|n| n.contains("context_tiers")),
            "note explains the brackets: {out}"
        );
    }

    #[tokio::test]
    async fn preview_errors_when_the_model_does_not_resolve() {
        let preview = RoutingPreview::new(&config_with_model(), None);
        let err = preview
            .preview(RoutePreviewArgs {
                model: "nonexistent-model".to_string(),
                prompt: None,
            })
            .await
            .expect_err("an unroutable model surfaces a ToolError");
        assert!(!err.0.is_empty(), "the error names the unresolved model");
    }
}
