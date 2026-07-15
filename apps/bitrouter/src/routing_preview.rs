//! The routing-preview adapter — the app side of the orchestrator profile's
//! `route_preview` tool (TUI_SPEC §4, PR-2 B1).
//!
//! Implements `bitrouter-mcp`'s
//! [`RoutingQuery`](bitrouter_mcp::capabilities::routing::RoutingQuery) port by
//! replaying BitRouter's *real* routing over a probe prompt — the policy-table
//! decision, the resolved provider fallback chain, and the registry's per-token
//! rates for the top hop. Read-only: nothing is sent upstream, and the
//! resolved targets' secrets (api keys) are never surfaced.

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

use crate::metering::PricingTable;
use crate::policy_table_router::{PolicyDecision, PolicyTableRouter};

/// Previews routing over a snapshot of the daemon's routing/policy/pricing
/// tables. Built once at bridge start; every `route_preview` reuses it.
pub struct RoutingPreview {
    table: ConfigRoutingTable,
    policy: Option<PolicyTableRouter>,
    pricing: PricingTable,
}

impl RoutingPreview {
    /// Snapshot the routing surface from `config`: apply the built-in provider
    /// defaults first (so a zero-config built-in still resolves), then build the
    /// routing table, the static policy table, and the pricing table — exactly
    /// the tables the daemon routes with.
    pub fn new(config: &Config) -> Self {
        let mut resolved = config.clone();
        bitrouter_providers::apply_builtin_defaults(&mut resolved);
        let pricing = crate::assemble::build_pricing_table(&resolved);
        let policy = PolicyTableRouter::from_config(&resolved.policy_table);
        let table = ConfigRoutingTable::from_config(resolved);
        Self {
            table,
            policy,
            pricing,
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
        let prompt = self.probe_prompt(&args);
        // The policy decision, when a policy table is configured — the effective
        // model it selects can differ from the requested one.
        let decision = self
            .policy
            .as_ref()
            .map(|p| p.decision_for(&prompt, &HeaderMap::new()));
        let effective_model = decision
            .as_ref()
            .and_then(|d| d.selected_model.clone())
            .unwrap_or_else(|| args.model.clone());

        // Resolve the effective model into its provider fallback chain (the same
        // resolution `bitrouter route` does; no daemon needed).
        let chain = self
            .table
            .route_chain(
                &effective_model,
                &RoutingPrefs::default(),
                &CallerContext::local(),
            )
            .await
            .with_context(|| format!("resolving model '{effective_model}'"))?;

        // The registry's per-token rates for the top hop, when priced.
        let estimated_cost = chain
            .first()
            .and_then(|t| self.pricing.resolve(&t.provider_name, &t.service_id))
            .filter(|p| !p.is_unconfigured())
            .map(|p| {
                serde_json::json!({
                    "input_micro_usd_per_token": p.input_micro_usd_per_token,
                    "output_micro_usd_per_token": p.output_micro_usd_per_token,
                    "note": "per-token rates from the registry; multiply by expected token counts",
                })
            });

        Ok(serde_json::json!({
            "requested_model": args.model,
            "effective_model": effective_model,
            "policy_decision": decision.as_ref().map(decision_json),
            "provider_chain": chain
                .iter()
                .map(|t| serde_json::json!({
                    "provider": t.provider_name,
                    "service_id": t.service_id,
                    "api_protocol": format!("{:?}", t.api_protocol).to_lowercase(),
                }))
                .collect::<Vec<_>>(),
            "estimated_cost": estimated_cost,
        }))
    }
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
        let preview = RoutingPreview::new(&config_with_model());
        let out = preview
            .do_preview(RoutePreviewArgs {
                model: "demo-model".to_string(),
                prompt: Some("hello".to_string()),
            })
            .await
            .expect("resolves");
        assert_eq!(out["requested_model"], "demo-model");
        let chain = out["provider_chain"].as_array().expect("chain");
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0]["provider"], "demo");
        assert_eq!(chain[0]["service_id"], "demo-model");
    }

    #[tokio::test]
    async fn preview_errors_when_the_model_does_not_resolve() {
        let preview = RoutingPreview::new(&config_with_model());
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
