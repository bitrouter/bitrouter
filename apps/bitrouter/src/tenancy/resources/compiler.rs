//! Pure construction of route-specific envelopes from explicitly retained input.

use super::manifest::{Artifact, ByteBudget, Definition, Import, bounded_bytes};
use crate::{
    config::{AccountStrategy, Config},
    gateway::pipeline::types::RoutingTarget,
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::{
    tenancy::{
        configuration::{Configuration, ResourceRef},
        operation::ErrorCode,
    },
    types::ApiProtocol,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

// Conservative source-expansion quota before the existing resolver allocates
// account-expanded targets. This is an internal provisioning profile: even
// candidates later filtered by protocol/preferences count toward the quota.
const MAX_EXPANDED_SOURCE_BYTES: usize = 8 * 1024 * 1024;

/// Secret-bearing routing defaults/targets are only ever persisted inside AEAD.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RouteEnvelope {
    pub reference: ResourceRef,
    pub selector: String,
    pub policy: Option<ResourceRef>,
    pub policy_name: Option<String>,
    pub system_prompt: Option<String>,
    pub defaults: serde_json::Map<String, Value>,
    /// One entry per base/tier/exploration selector, retaining fallback order for
    /// each inbound protocol. Entries from another route are never combined.
    pub chains: Vec<ApprovedChain>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ApprovedChain {
    /// Original selector identity. The base uses RouteEnvelope.selector; its
    /// effective clean model has already been resolved into the retained targets.
    pub selector: String,
    pub inbound: ApiProtocol,
    pub targets: Vec<ApprovedTarget>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ApprovedTarget {
    pub credential: ResourceRef,
    /// Complete resolved target excluding secret key bytes; header defaults and
    /// endpoints remain private inside the encrypted configuration envelope.
    pub target: Value,
}

pub(super) fn prepare_route(import: &Import, policy: Option<&Artifact>) -> Result<RouteEnvelope> {
    let Definition::Route {
        routing,
        selector,
        policy: policy_ref,
        credentials,
        exploration,
    } = &import.definition
    else {
        anyhow::bail!("route definition required");
    };
    let (mut config, provider_sizes) = routing_config(routing)?;
    let resolution = config
        .resolve_router(selector)
        .map_err(|_| anyhow::anyhow!("invalid retained selector"))?;
    ensure!(
        resolution.request_checks.is_empty(),
        "native request check imports are not qualified"
    );
    // Keep the original selector as the envelope's lookup identity. Its base
    // already passed Stage 0 above; reparsing clean_model can reinterpret a
    // literal provider model suffix as a configured variant.
    let mut selections = vec![selector.clone()];
    let policy_name = match (&resolution.policy, policy_ref, policy) {
        (Some(selected), Some(reference), Some(artifact)) => {
            ensure!(
                artifact.reference == *reference,
                "qualified policy identity mismatch"
            );
            let Definition::Policy { document, name, .. } = &artifact.definition else {
                anyhow::bail!("policy definition required");
            };
            let variant = resolution
                .variant
                .as_ref()
                .map(|v| format!("{selected}:{v}"));
            let effective = variant
                .as_deref()
                .filter(|name| document.policies.contains_key(*name))
                .unwrap_or(selected);
            ensure!(effective == name, "selected policy name mismatch");
            crate::policy_lock::validate_for_config(&config, document)
                .map_err(|_| anyhow::anyhow!("policy routing validation failed"))?;
            let definition = document
                .policies
                .get(name)
                .context("selected policy missing")?;
            ensure!(
                definition.tiers.len() <= 256,
                "policy tier capacity exceeded"
            );
            selections.extend(definition.tiers.values().map(|target| target.model.clone()));
            // Construction qualifies the immutable in-memory policy path without
            // introducing a shared backend or any policy-file access.
            if !config.trajectory.enabled {
                crate::policy_lock::PolicyRuntime::from_document(
                    &config,
                    Some(document.clone()),
                    Default::default(),
                    None,
                )
                .map_err(|_| anyhow::anyhow!("immutable policy construction failed"))?;
            }
            Some(name.clone())
        }
        (None, None, None) => None,
        _ => anyhow::bail!("route requires its exact qualified policy"),
    };
    selections.extend(exploration.iter().cloned());
    let mut unique = HashSet::new();
    selections.retain(|selector| unique.insert(selector.clone()));
    ensure!(
        selections.len() <= 512 && selections.iter().all(|s| !s.is_empty() && s.len() <= 512),
        "route selection capacity exceeded"
    );

    // Enumeration must not consume the installation-global balance counter.
    // Preserve the source strategy in the encrypted definition; the approved
    // membership includes every explicitly bound account in declared order.
    for provider in config.providers.values_mut() {
        provider.account_strategy = AccountStrategy::Failover;
    }
    let mut envelope = RouteEnvelope {
        reference: import.reference.clone(),
        selector: selector.clone(),
        policy: policy_ref.clone(),
        policy_name,
        system_prompt: resolution.overrides.system_prompt,
        defaults: resolution.overrides.params,
        chains: Vec::new(),
    };
    // Charge the empty arrays once, then each entry and separator before push.
    // Counting borrows the values and never allocates a serialized copy.
    let mut budget = ByteBudget::new(super::MAX_ARTIFACT_BYTES);
    budget
        .charge(&envelope)
        .context("route envelope byte capacity exceeded")?;
    let mut used_slots = HashSet::new();
    for (index, selection) in selections.into_iter().enumerate() {
        // Only tier/exploration inputs still need Stage 0. Resolve them once,
        // retain their preferences, then refine with the root route's settings
        // in the same order as resolve_route_chain's caller preferences.
        let selected = if index == 0 {
            None
        } else {
            let selected = config
                .resolve_router(&selection)
                .map_err(|_| anyhow::anyhow!("invalid policy or exploration selector"))?;
            ensure!(
                selected.policy.is_none() && selected.request_checks.is_empty(),
                "nested policy or native checker selection is not qualified"
            );
            Some(selected)
        };
        let (clean, prefs) = match &selected {
            None => (&resolution.clean_model, resolution.prefs.clone()),
            Some(selected) => {
                let mut prefs = selected.prefs.clone();
                crate::config::routing_table::merge_prefs(&mut prefs, &resolution.prefs);
                (&selected.clean_model, prefs)
            }
        };
        resolution_capacity(&config, &provider_sizes, clean)?;
        let mut found = false;
        for inbound in [
            ApiProtocol::ChatCompletions,
            ApiProtocol::Messages,
            ApiProtocol::Responses,
            ApiProtocol::Decisions,
        ] {
            let mut prefs = prefs.clone();
            prefs.inbound_protocol = Some(inbound.clone());
            prefs.operation = inbound.operation();
            let result =
                crate::config::routing_table::resolve_clean_route_chain(&config, clean, &prefs);
            let targets = match result {
                Ok(targets) => targets,
                // An otherwise valid selector can be incompatible with this
                // operation; at least one complete chain must qualify below.
                Err(_) => continue,
            };
            if targets.is_empty() {
                continue;
            }
            found = true;
            ensure!(targets.len() <= 256, "route target capacity exceeded");
            let mut chain = ApprovedChain {
                selector: selection.clone(),
                inbound,
                targets: Vec::new(),
            };
            budget
                .separator(!envelope.chains.is_empty())
                .and_then(|()| budget.charge(&chain))
                .context("route envelope byte capacity exceeded")?;
            for target in &targets {
                let account = target.account_label.as_deref().unwrap_or("default");
                let binding = credentials
                    .iter()
                    .find(|binding| {
                        binding.provider == target.provider_name && binding.account == account
                    })
                    .context("resolved account has no immutable credential binding")?;
                used_slots.insert((binding.provider.clone(), binding.account.clone()));
                let approved = ApprovedTarget {
                    credential: binding.credential.clone(),
                    target: target_value(target)?,
                };
                budget
                    .separator(!chain.targets.is_empty())
                    .and_then(|()| budget.charge(&approved))
                    .context("route envelope byte capacity exceeded")?;
                chain.targets.push(approved);
            }
            envelope.chains.push(chain);
        }
        ensure!(found, "route has no approved targets");
    }
    ensure!(
        used_slots.len() == credentials.len(),
        "credential binding is outside the selected route closure"
    );
    Ok(envelope)
}

pub(super) fn routing_config(source: &Value) -> Result<(Config, HashMap<String, usize>)> {
    routing_config_bounded(source, None)
}

// Native projection shares the v1 parser, but reserves cumulative work before
// cloning source/ancestry. The wire compiler keeps its existing independent cap.
pub(super) fn routing_config_bounded(
    source: &Value,
    mut work: Option<&mut ByteBudget>,
) -> Result<(Config, HashMap<String, usize>)> {
    if let Some(work) = work.as_deref_mut() {
        work.charge(source)
            .map_err(|_| super::ExecutionCapacityExceeded)?;
    }
    bounded_bytes(source, super::MAX_ARTIFACT_BYTES)?;
    let fields = source
        .as_object()
        .context("routing source must be an object")?;
    ensure!(
        fields.keys().all(|key| matches!(
            key.as_str(),
            "providers"
                | "models"
                | "routers"
                | "presets"
                | "variants"
                | "policy"
                | "upstream"
                | "trajectory"
                | "continuation"
        )),
        "host configuration is not resource routing input"
    );
    let providers = fields
        .get("providers")
        .and_then(Value::as_object)
        .context("explicit providers required")?;
    ensure!(providers.len() <= 256, "provider capacity exceeded");
    let mut provider_sizes = HashMap::new();
    let mut expanded = ByteBudget::new(MAX_EXPANDED_SOURCE_BYTES);
    for name in providers.keys() {
        let mut ancestry = ByteBudget::new(super::MAX_ARTIFACT_BYTES);
        let mut seen = HashSet::new();
        let mut current = name.as_str();
        loop {
            ensure!(
                seen.len() < 32 && seen.insert(current),
                "invalid provider derivation graph"
            );
            let provider = providers
                .get(current)
                .context("unresolved provider derivation")?;
            if let Some(work) = work.as_deref_mut() {
                work.charge(&(current, provider))
                    .map_err(|_| super::ExecutionCapacityExceeded)?;
            }
            // Charge all ancestors before parse_resolved clones inherited fields.
            // Explicit overrides still count, keeping the preflight conservative.
            expanded
                .charge(&(current, provider))
                .context("provider expansion byte capacity exceeded")?;
            ancestry
                .charge(&(current, provider))
                .context("provider expansion byte capacity exceeded")?;
            match provider.get("derives").and_then(Value::as_str) {
                Some(parent) => current = parent,
                None => break,
            }
        }
        provider_sizes.insert(
            name.clone(),
            super::MAX_ARTIFACT_BYTES - ancestry.remaining(),
        );
    }
    let mut fields = fields.clone();
    fields.insert("inherit_defaults".into(), json!(false));
    fields.insert("registry".into(), json!({"enabled": false}));
    let config = crate::config::parse_resolved(&serde_json::to_string(&fields)?)
        .map_err(|_| anyhow::anyhow!("invalid explicit routing configuration"))?;
    ensure!(
        config.policy.path.is_none(),
        "policy file access is not retained authority"
    );
    ensure!(
        config.models.len() <= 256
            && config.routers.len() <= 256
            && config.presets.len() <= 256
            && config.variants.len() <= 256,
        "routing catalog capacity exceeded"
    );
    for provider in config.providers.values() {
        ensure!(
            provider
                .api_protocol
                .values()
                .flat_map(|protocols| protocols.as_slice())
                .all(|protocol| !matches!(protocol, ApiProtocol::Custom(_)))
                && provider
                    .models
                    .iter()
                    .filter_map(|model| model.api_protocol.as_ref())
                    .flat_map(|protocols| protocols.as_slice())
                    .all(|protocol| !matches!(protocol, ApiProtocol::Custom(_))),
            "custom protocol imports are not qualified"
        );
        ensure!(
            !provider.auto_discover && !provider.api_base.is_empty(),
            "provider discovery is not retained authority"
        );
        ensure!(
            provider.api_key.is_empty() && provider.accounts.iter().all(|a| a.api_key.is_empty()),
            "provider secrets require separately owned credential slots"
        );
        ensure!(
            provider.models.len() <= 256 && provider.accounts.len() <= 256,
            "provider capacity exceeded"
        );
        ensure!(
            provider.headers.values().all(|header| !matches!(header,
            crate::config::ProviderHeaderConfig::Policy(policy) if policy.passthrough)),
            "inbound account/header passthrough is not qualified"
        );
        let mut accounts = HashSet::new();
        ensure!(
            provider
                .accounts
                .iter()
                .all(|account| !account.label.is_empty() && accounts.insert(&account.label)),
            "accounts require explicit unique labels"
        );
    }
    Ok((config, provider_sizes))
}

/// Candidate count and conservative source-expansion bytes, before resolution.
pub(super) fn resolution_capacity(
    config: &Config,
    sizes: &HashMap<String, usize>,
    clean: &str,
) -> Result<(usize, usize)> {
    ensure!(clean.len() <= 512, "route selection capacity exceeded");
    let mut count = 0_usize;
    let mut bytes = 0_usize;
    let mut charge = |name: &str, provider: &crate::config::ProviderConfig| -> Result<()> {
        if !provider.active {
            return Ok(());
        }
        let accounts = provider.accounts.len().max(1);
        count = count.saturating_add(accounts);
        // Source includes every possible protocol/header/model override. Account
        // expansion and repeated virtual endpoints must be counted before the
        // resolver clones their strings; fixed overhead covers target fields.
        bytes = bytes.saturating_add(
            sizes
                .get(name)
                .context("unretained provider")?
                .saturating_add(clean.len())
                .saturating_add(1024)
                .saturating_mul(accounts),
        );
        ensure!(
            count <= 256 && bytes <= MAX_EXPANDED_SOURCE_BYTES,
            "route resolver expansion capacity exceeded"
        );
        Ok(())
    };
    if let Some((name, _)) = clean.split_once(':')
        && let Some(provider) = config.providers.get(name).filter(|p| p.active)
    {
        charge(name, provider)?;
    } else if let Some(model) = config.models.get(clean) {
        for endpoint in &model.endpoints {
            ensure!(
                endpoint.service_id.len() <= 512,
                "route selection capacity exceeded"
            );
            if let Some(provider) = config.providers.get(&endpoint.provider) {
                charge(&endpoint.provider, provider)?;
            }
        }
    } else {
        for (name, provider) in &config.providers {
            if provider.models.iter().any(|model| model.id == clean) {
                charge(name, provider)?;
            }
        }
    }
    Ok((count, bytes))
}

pub(super) fn target_value(target: &RoutingTarget) -> Result<Value> {
    ensure!(
        !matches!(target.api_protocol, ApiProtocol::Custom(_)),
        "custom protocol imports are not qualified"
    );
    ensure!(
        target.api_key.is_empty() && target.api_key_override.is_none(),
        "unbound target secret"
    );
    let api_base = target.effective_api_base();
    crate::config::url_validator::validate_upstream_url(api_base)
        .map_err(|_| anyhow::anyhow!("invalid retained target URL"))?;
    let headers = target.headers.iter().map(|header| Ok(json!({
        "name": header.name().as_str(), "default": header.default().map(|v| v.to_str()).transpose()
            .map_err(|_| anyhow::anyhow!("invalid retained header"))?, "passthrough": header.passthrough(),
    }))).collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "provider": target.provider_name, "model": target.service_id,
        "api_base": api_base, "protocol": target.api_protocol,
        "account": target.account_label, "auth_scheme": target.auth_scheme,
        "chat_token_limit_field": target.chat_token_limit_field,
        "chat_supports_store": target.chat_supports_store,
        "chat_supports_stream_options": target.chat_supports_stream_options,
        "chat_google_extensions": target.chat_google_extensions,
        "reasoning_effort": target.reasoning_effort,
        "model_constraints": target.model_constraints, "headers": headers,
    }))
}

pub(super) fn configuration_routes(
    document: &Configuration,
    artifacts: &[Artifact],
) -> std::result::Result<Vec<RouteEnvelope>, ErrorCode> {
    let mut routes = Vec::new();
    let mut budget = ByteBudget::new(super::MAX_ARTIFACT_BYTES);
    for reference in &document.routes {
        let artifact = artifacts
            .iter()
            .find(|a| a.reference == *reference)
            .ok_or(ErrorCode::NotFound)?;
        let envelope = artifact
            .envelope
            .as_ref()
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        budget
            .charge(envelope)
            .map_err(|_| ErrorCode::CapacityExceeded)?;
        if envelope.reference != *reference {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        if envelope.policy.is_some() && envelope.policy != document.policy {
            return Err(ErrorCode::InvalidRequest);
        }
        let mut envelope = envelope.clone();
        // These values remain defaults. Request values are applied ahead of them
        // by the execution adapter; compilation never rewrites a request body.
        envelope.defaults.insert(
            "max_output_tokens".into(),
            json!(document.generation_defaults.max_output_tokens),
        );
        envelope.defaults.insert(
            "temperature".into(),
            json!(document.generation_defaults.temperature),
        );
        routes.push(envelope);
    }
    Ok(routes)
}
