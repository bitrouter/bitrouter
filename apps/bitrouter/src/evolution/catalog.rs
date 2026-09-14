//! Dependency validation against the configuration and policy actually serving.

use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::config::{AccountStrategy, Config, ConfigRoutingTable, resolve_presets};
use bitrouter_sdk::language_model::{
    ApiProtocol, PipelineContext, PipelineRequest, RoutingTable, RoutingTarget,
};
use serde_json::{Value, json};

use super::control::BlockDefinition;
use super::rubric::digest;
use crate::policy_lock::PolicyRoutingSnapshot;

/// A request-time compatibility guard is part of every block arm. If a
/// candidate's possible tiers cannot serve this request, retain the established
/// baseline and record that fallback under the original random assignment.
pub(super) async fn request_compatible(
    config: &Config,
    policies: &PolicyRoutingSnapshot,
    route: &str,
    ctx: &PipelineContext,
) -> Result<bool> {
    let resolution = resolve_presets(route, &config.presets, &config.variants)?;
    let mut preview = PipelineContext::new(PipelineRequest::new(
        route,
        ctx.caller().clone(),
        ctx.prompt().clone(),
    ));
    preview.apply_preset_overrides(&resolution.overrides);
    let mut models = vec![resolution.clean_model.clone()];
    if let Some(policy) = &resolution.policy {
        let document = policies
            .document()
            .context("named policy snapshot is unavailable")?;
        let variant_name = resolution
            .variant
            .as_ref()
            .map(|variant| format!("{policy}:{variant}"));
        let definition = variant_name
            .as_ref()
            .and_then(|name| document.policies.get(name))
            .or_else(|| document.policies.get(policy))
            .context("named policy is unavailable")?;
        models = definition
            .tiers
            .values()
            .map(|target| target.model().to_owned())
            .collect();
    }
    let mut stable = config.clone();
    for provider in stable.providers.values_mut() {
        provider.account_strategy = AccountStrategy::Failover;
    }
    let table = ConfigRoutingTable::from_config(stable);
    let mut prefs = resolution.prefs;
    prefs.require_capabilities = preview.prompt().required_capabilities();
    prefs.inbound_protocol = ctx.inbound_protocol();
    for model in models {
        match table.route_resolved(&model, &prefs, ctx.caller()).await {
            Ok(chain) if !chain.is_empty() => {
                // Normal SDK routing treats capability metadata as positive
                // evidence, not an exhaustive denylist. A new treatment needs
                // positive support for every required capability on every hop.
                if !chain.iter().all(|target| {
                    config
                        .providers
                        .get(&target.provider_name)
                        .is_some_and(|provider| {
                            prefs.require_capabilities.iter().all(|capability| {
                                provider.model_supports_capability(&target.service_id, *capability)
                            })
                        })
                }) {
                    return Ok(false);
                }
            }
            _ => return Ok(false),
        }
    }
    Ok(true)
}

pub(super) async fn block_digest(
    config: &Config,
    policies: &PolicyRoutingSnapshot,
    definition: &BlockDefinition,
) -> Result<String> {
    definition.validate()?;
    let mut stable = config.clone();
    // Account rotation is part of the policy, not a change to its identity.
    // Enumerate its complete ordered input and hash the strategy separately.
    for provider in stable.providers.values_mut() {
        provider.account_strategy = AccountStrategy::Failover;
    }
    let table = ConfigRoutingTable::from_config(stable);
    let mut routes = BTreeMap::new();
    for rule in &definition.rules {
        ensure!(
            rule.selector.starts_with('@')
                || rule.selector == "bitrouter/auto"
                || config.models.contains_key(&rule.selector),
            "an evolution matcher must name a configured preset or virtual route"
        );
        for route in [&rule.baseline_route, &rule.challenger_route] {
            if !routes.contains_key(route) {
                routes.insert(
                    route.clone(),
                    route_contract(config, &table, policies, route).await?,
                );
            }
        }
    }
    digest(&(
        "evolution-routing-contract-v1",
        &definition.rules,
        routes,
        format!("{:?}", config.upstream.timeouts),
        &config.upstream.fallback_backoff_ms,
    ))
}

pub(super) async fn route_contract(
    config: &Config,
    table: &ConfigRoutingTable,
    policies: &PolicyRoutingSnapshot,
    route: &str,
) -> Result<Value> {
    let resolution = resolve_presets(route, &config.presets, &config.variants)?;
    let mut models = BTreeMap::from([(resolution.clean_model.clone(), None)]);
    let mut policy_contract = Value::Null;
    if let Some(policy) = &resolution.policy {
        let document = policies
            .document()
            .context("named policy snapshot is unavailable")?;
        let variant_name = resolution
            .variant
            .as_ref()
            .map(|variant| format!("{policy}:{variant}"));
        let (name, definition) = variant_name
            .as_ref()
            .and_then(|name| document.policies.get_key_value(name))
            .or_else(|| document.policies.get_key_value(policy))
            .context("route references an unavailable named policy")?;
        models = definition
            .tiers
            .values()
            .map(|target| (target.model().to_owned(), target.effort()))
            .collect();
        policy_contract = json!({"name": name, "definition": definition,
            "certificates": document.certificates.get(name), "mode": config.policy.mode});
    }
    let mut chains = BTreeMap::new();
    for (model, effort) in models {
        let mut protocols = Vec::new();
        for protocol in [
            None,
            Some(ApiProtocol::ChatCompletions),
            Some(ApiProtocol::Messages),
            Some(ApiProtocol::GenerateContent),
            Some(ApiProtocol::Responses),
        ] {
            let mut prefs = resolution.prefs.clone();
            prefs.inbound_protocol = protocol.clone();
            let chain = table
                .route_resolved(&model, &prefs, &CallerContext::local())
                .await?;
            ensure!(!chain.is_empty(), "route contains an unavailable model");
            let targets = chain
                .iter()
                .map(|target| target_contract(config, target))
                .collect::<Result<Vec<_>>>()?;
            protocols.push(
                json!({"inbound": protocol.map(|p| p.as_str().to_owned()), "targets": targets}),
            );
        }
        chains.insert(
            model.clone(),
            json!({"effort": effort, "protocols": protocols,
            "virtual_strategy": config.models.get(&model).map(|v| format!("{:?}", v.strategy)),
            "virtual_pricing": config.models.get(&model).map(|v| format!("{:?}", v.pricing))}),
        );
    }
    Ok(json!({"policy": policy_contract, "models": chains,
        "prompt_defaults_digest": digest(&(&resolution.overrides.system_prompt, &resolution.overrides.params))?,
        "preferences": {"sort": resolution.prefs.sort, "tags": resolution.prefs.require_tags,
            "only": resolution.prefs.only, "ignore": resolution.prefs.ignore}}))
}

fn target_contract(config: &Config, target: &RoutingTarget) -> Result<Value> {
    // URL credentials and query strings can contain secrets; this feature does
    // not fingerprint those endpoints. They remain usable by ordinary routing.
    let url = url::Url::parse(&target.api_base)?;
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "evolution requires an endpoint without URL credentials, query or fragment"
    );
    let provider = config
        .providers
        .get(&target.provider_name)
        .context("provider disappeared")?;
    let metadata = provider.models.iter().find(|model| {
        model.id == target.service_id
            || model.provider_model_id.as_deref() == Some(&target.service_id)
    });
    Ok(
        json!({"provider": target.provider_name, "model": target.service_id,
        "endpoint": url.as_str(), "protocol": target.api_protocol.as_str(),
        "account": target.account_label, "account_strategy": format!("{:?}", provider.account_strategy),
        "chat_token_limit_field": target.chat_token_limit_field,
        "chat_supports_store": target.chat_supports_store,
        "chat_supports_stream_options": target.chat_supports_stream_options,
        "reasoning_effort": target.reasoning_effort,
        "capabilities": metadata.map(|model| &model.capabilities),
        "pricing": metadata.map(|model| format!("{:?}", model.pricing)),
        "timeouts": format!("{:?}", provider.timeouts)}),
    )
}
