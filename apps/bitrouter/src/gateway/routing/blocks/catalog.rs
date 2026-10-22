//! Dependency validation against the configuration and policy actually serving.

use crate::gateway::model::capabilities::required_capabilities;
use std::collections::BTreeMap;

use crate::config::AccountStrategy;
use crate::config::Config;
use crate::config::routing_table::ConfigRoutingTable;
use crate::gateway::caller::CallerContext;
use crate::gateway::pipeline::context::PipelineContext;
use crate::gateway::pipeline::routing::RoutingTable;
use crate::gateway::pipeline::types::PipelineRequest;
use crate::gateway::pipeline::types::RoutingTarget;
use anyhow::{Context, Result, ensure};
use bitrouter_ai::types::ApiProtocol;
use serde_json::{Value, json};

use super::control::BlockDefinition;
use crate::policy_lock::PolicyRoutingSnapshot;
use crate::storage::evolution::digest;

/// A request-time compatibility guard is part of every block arm. If a
/// candidate's possible tiers cannot serve this request, retain the established
/// baseline and record that fallback under the original random assignment.
pub(crate) async fn request_compatible(
    config: &Config,
    policies: &PolicyRoutingSnapshot,
    route: &str,
    ctx: &PipelineContext,
) -> Result<bool> {
    let resolution = config.resolve_router(route)?;
    let mut preview = PipelineContext::new(PipelineRequest::new(
        route,
        ctx.caller().clone(),
        ctx.require_generation_prompt()?.clone(),
    ));
    preview.apply_preset_overrides(&resolution.overrides)?;
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
    let prompt = preview.require_generation_prompt()?;
    prefs.require_capabilities = required_capabilities(prompt);
    prefs.registry_requirements =
        Some(crate::gateway::catalog::execution::RequestRequirements::generation(prompt));
    prefs.inbound_protocol = ctx.inbound_protocol();
    for model in models {
        match table.route_resolved(&model, &prefs, ctx.caller()).await {
            Ok(chain) if !chain.is_empty() => {
                // Normal SDK routing treats capability metadata as positive
                // evidence, not an exhaustive denylist. A new treatment needs
                // positive support for every required capability on every hop.
                if !chain.iter().all(|target| {
                    crate::gateway::catalog::execution::validate_generation(target, prompt).is_ok()
                        && prefs.require_capabilities.iter().all(|capability| {
                            target.model_constraints.capabilities.contains(capability)
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

pub(crate) async fn block_digest(
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
                || (rule.selector.starts_with("bitrouter/")
                    && config.resolve_router(&rule.selector).is_ok())
                || config.models.contains_key(&rule.selector),
            "an evolution matcher must name a configured router, preset, or virtual route"
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

pub(crate) async fn route_contract(
    config: &Config,
    table: &ConfigRoutingTable,
    policies: &PolicyRoutingSnapshot,
    route: &str,
) -> Result<Value> {
    let resolution = config.resolve_router(route)?;
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
        "chat_google_extensions": target.chat_google_extensions,
        "reasoning_effort": target.reasoning_effort,
        "capabilities": target.model_constraints.capabilities,
        "limits": target.model_constraints.token_limits,
        "registry_provider_contract": provider.registry.as_ref().map(|raw| digest(&raw.to_string())).transpose()?,
        "registry_contract": target.registry.as_ref().map(|selected| json!({"interface":selected.interface.to_string(),"auth":selected.auth.to_string(),"provider":selected.provider_id,"native_model":selected.provider_model_id,"billing":selected.billing,"verified_profile":selected.verified_profile})),
        "pricing": metadata.map(|model| format!("{:?}", model.pricing)),
        "timeouts": format!("{:?}", provider.timeouts)}),
    )
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    #[tokio::test]
    async fn registry_preview_and_digest_cover_alternative_native_offers() -> Result<()> {
        let mut raw = crate::gateway::catalog::tests::document("a-low");
        raw["data"]["providers"][0]["models"][0]["interfaces"] = registry_json::json!([{"protocol":"responses","operation":"generation","capabilities":["reasoning"],"reasoning_effort":{"levels":["low"]}}]);
        let mut second = raw["data"]["providers"][0]["models"][0].clone();
        second["provider_model_id"] = registry_json::json!("b-high");
        second["interfaces"][0]["reasoning_effort"]["levels"] = registry_json::json!(["high"]);
        raw["data"]["providers"][0]["models"]
            .as_array_mut()
            .context("offers missing")?
            .push(second);
        let data = crate::gateway::catalog::decode::from_slice(&registry_json::to_vec(&raw)?)?;
        let mut config = Config::default();
        crate::providers::registry::apply::apply_registry(&mut config, &data);
        let home = tempfile::tempdir()?;
        let runtime = crate::policy_lock::PolicyRuntime::new(
            &config,
            Some(&home.path().join("bitrouter.yaml")),
            None,
            Default::default(),
            None,
        )
        .await?;
        let policies = runtime.routing_snapshot();
        let prompt = serde_json::from_value(
            json!({"model":"org/model","messages":[],"tools":[],"params":{"reasoning_effort":"high"},"stream":false}),
        )?;
        let ctx = PipelineContext::new(PipelineRequest::new(
            "org/model",
            CallerContext::local(),
            prompt,
        ));
        assert!(request_compatible(&config, &policies, "org/model", &ctx).await?);
        let table = ConfigRoutingTable::from_config(config.clone());
        let before = route_contract(&config, &table, &policies, "org/model").await?;
        let provider = config
            .providers
            .get_mut("fixture")
            .context("provider missing")?;
        provider.registry.as_mut().context("raw provider missing")?["models"][1]["interfaces"][0]
            ["limits"] = registry_json::json!({"max_output_tokens":321});
        let table = ConfigRoutingTable::from_config(config.clone());
        let after = route_contract(&config, &table, &policies, "org/model").await?;
        assert_ne!(before, after);
        Ok(())
    }
}
