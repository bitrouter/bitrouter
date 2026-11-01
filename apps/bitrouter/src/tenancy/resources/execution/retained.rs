//! Build typed route data exactly once from the authenticated envelope.

use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{authority::ResourceKind, configuration::ResourceRef};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use super::super::{
    Envelope, ExecutionCapacityExceeded, MAX_CLOSURE,
    compiler::RouteEnvelope,
    manifest::{Artifact, CredentialBinding, Definition},
};
use super::{Candidate, Chain, RetainedPolicy, RouteData, ValidatedEnvelope, native_v1, target};
use crate::{gateway::pipeline::routing::PromptOverrides, policy_lock::PolicyLock};

// Internal decoded-work limits, in addition to the authenticated 1 MiB plaintext
// bound. This counts target/projection content, not process RSS or cache weight.
const MAX_TARGETS: usize = 4096;
const MAX_TARGET_CONTENT_BYTES: usize = 8 * 1024 * 1024;
// Shared dependencies may occur in several selected routes; bound the sum of
// owned reference copies as well as each individual reachable graph.
const MAX_ROUTE_DEPENDENCIES: usize = 16_384;

struct Source {
    routing: Arc<serde_json::Value>,
    credentials: Vec<CredentialBinding>,
    choice: Option<super::super::core_choice::Profile>,
    envelope: Option<RouteEnvelope>,
    assessment: Option<ResourceRef>,
    block: Option<Box<super::super::block::Profile>>,
}

struct Policy {
    name: String,
    document: Arc<PolicyLock>,
}

pub(super) fn decode(envelope: Envelope) -> Result<ValidatedEnvelope> {
    decode_with_operation(envelope, None)
}

pub(super) fn decode_with_operation(
    envelope: Envelope,
    operation: Option<&crate::storage::operation_budget::OperationBudget<'_>>,
) -> Result<ValidatedEnvelope> {
    envelope.validate_format()?;
    let Envelope {
        format: _,
        document,
        routes,
        artifacts,
    } = envelope;
    ensure!(
        artifacts.len() <= MAX_CLOSURE && routes.len() <= 256,
        ExecutionCapacityExceeded
    );
    super::super::core_choice::validate_graph(&artifacts, &document.routes, false)?;
    let mut sources = HashMap::new();
    let mut policies = HashMap::new();
    let mut graph = HashMap::new();
    let mut learned_checkpoint = false;
    for Artifact {
        reference,
        definition,
        envelope,
    } in artifacts
    {
        graph.insert(reference.clone(), definition.dependencies());
        match definition {
            Definition::Route {
                routing,
                credentials,
                core_choice,
                assessment,
                block,
                ..
            } => {
                sources.insert(
                    reference,
                    Source {
                        routing: Arc::new(routing),
                        credentials,
                        choice: core_choice.map(|profile| *profile),
                        envelope,
                        assessment: assessment.map(|profile| profile.route),
                        block,
                    },
                );
            }
            Definition::Policy { document, name, .. } => {
                policies.insert(
                    reference,
                    Policy {
                        name,
                        document: Arc::new(document),
                    },
                );
            }
            Definition::Checkpoint { evidence } => {
                evidence.validate(&reference)?;
                learned_checkpoint = true;
            }
            Definition::Credential { .. } => {
                anyhow::bail!("credential plaintext in compiled envelope")
            }
        }
    }
    let mut retained = HashMap::new();
    let mut target_count = 0_usize;
    let mut content_bytes = 0_usize;
    let mut dependency_count = 0_usize;
    let mut native_budget = native_v1::Budget::with_operation(operation);
    // Hidden children are decoded once by complete identity, using the SAME
    // cumulative work/target/dependency budgets as visible roots.
    let child_refs: HashSet<_> = sources
        .values()
        .filter_map(|source| source.choice.as_ref().map(|profile| profile.route.clone()))
        .collect();
    let mut children = HashMap::new();
    for reference in child_refs {
        let dependencies = dependencies(&reference, &graph)?;
        dependency_count = dependency_count
            .checked_add(dependencies.len())
            .context(ExecutionCapacityExceeded)?;
        ensure!(
            dependency_count <= MAX_ROUTE_DEPENDENCIES,
            ExecutionCapacityExceeded
        );
        let mut source = sources
            .remove(&reference)
            .context("missing CoreChoice child source")?;
        let route = source
            .envelope
            .take()
            .context("missing CoreChoice child envelope")?;
        let mut data = route_data(
            route,
            source,
            dependencies,
            &policies,
            &mut target_count,
            &mut content_bytes,
            &mut native_budget,
        )?;
        let native = data.native.chains().map_err(|code| match code {
            bitrouter_ai::tenancy::operation::ErrorCode::CapacityExceeded => {
                anyhow::Error::new(ExecutionCapacityExceeded)
            }
            _ => anyhow::anyhow!("CoreChoice child cannot execute canonical Decisions"),
        })?;
        ensure!(
            native.len() == 1
                && native[0].selector == data.selector
                && native[0].operation == bitrouter_ai::types::ModelOperation::Decisions
                && native[0].candidates.len() == 1,
            "CoreChoice canonical child must have one target"
        );
        // Normalize opaque IDs before sharing the child Arc among parents.
        super::compact::route_data(&mut data)
            .map_err(|_| anyhow::anyhow!("CoreChoice identifier normalization failed"))?;
        children.insert(reference, Arc::new(data));
    }
    for route in routes {
        let dependencies = dependencies(&route.reference, &graph)?;
        dependency_count = dependency_count
            .checked_add(dependencies.len())
            .context(ExecutionCapacityExceeded)?;
        ensure!(
            dependency_count <= MAX_ROUTE_DEPENDENCIES,
            ExecutionCapacityExceeded
        );
        let mut source = sources
            .remove(&route.reference)
            .context("missing retained route source")?;
        let choice = source.choice.take();
        let mut data = route_data(
            route,
            source,
            dependencies,
            &policies,
            &mut target_count,
            &mut content_bytes,
            &mut native_budget,
        )?;
        if let Some(profile) = choice {
            ensure!(
                data.native
                    .chains()
                    .map_err(|_| anyhow::anyhow!("CoreChoice parent native route unavailable"))?
                    .iter()
                    .any(|chain| chain.selector == data.selector
                        && chain.operation == bitrouter_ai::types::ModelOperation::Generation),
                "CoreChoice parent must support native Generation"
            );
            data.choice = Some(super::RetainedChoice {
                route: children
                    .get(&profile.route)
                    .context("CoreChoice child projection missing")?
                    .clone(),
                digest: profile.digest()?,
                #[cfg(all(feature = "core", feature = "harness"))]
                policy: profile.policy,
            });
        }
        ensure!(
            retained
                .insert(data.reference.resource_id.to_string(), Arc::new(data))
                .is_none(),
            "duplicate visible retained route"
        );
    }
    for parent in retained.values() {
        if let Some(reference) = &parent.assessment {
            ensure!(
                matches!(&parent.native, super::NativeChains::Ready(chains) if chains.iter().any(|chain|
                chain.selector == parent.selector && chain.operation == bitrouter_ai::types::ModelOperation::Generation)),
                "assessment parent must support native generation"
            );
            let child = retained
                .get(reference.resource_id.as_str())
                .context("assessment route is not an explicit configuration root")?;
            ensure!(
                child.reference == *reference,
                "assessment revision mismatch"
            );
            child
                .static_assessment()
                .map_err(|_| anyhow::anyhow!("unsupported assessment route"))?;
        }
    }
    Ok(ValidatedEnvelope {
        document,
        learned_checkpoint,
        routes: retained,
    })
}

/// Walk the authenticated graph from this route alone. Credential definitions
/// are deliberately absent from the execution envelope and terminate traversal.
fn dependencies(
    root: &ResourceRef,
    graph: &HashMap<ResourceRef, Vec<ResourceRef>>,
) -> Result<Vec<ResourceRef>> {
    let mut result = vec![root.clone()];
    let mut seen = HashSet::from([root]);
    let mut pending = vec![root];
    while let Some(reference) = pending.pop() {
        if reference.resource_kind == ResourceKind::Credential {
            continue;
        }
        for dependency in graph
            .get(reference)
            .context("missing retained dependency artifact")?
        {
            if seen.insert(dependency) {
                ensure!(seen.len() <= MAX_CLOSURE, ExecutionCapacityExceeded);
                result.push(dependency.clone());
                pending.push(dependency);
            }
        }
    }
    Ok(result)
}

fn route_data(
    route: RouteEnvelope,
    source: Source,
    dependencies: Vec<ResourceRef>,
    policies: &HashMap<ResourceRef, Policy>,
    target_count: &mut usize,
    content_bytes: &mut usize,
    native_budget: &mut native_v1::Budget<'_>,
) -> Result<RouteData> {
    // Derive runtime-only limits/ordering from authenticated source. Keeping
    // these out of the exact target codec preserves compiler-v1 ciphertext.
    let runtime_config = native_budget.block_config(&source.routing)?;
    let root_preferences = runtime_config.resolve_router(&route.selector)?.prefs;
    let policy = match (&route.policy, &route.policy_name) {
        (Some(reference), Some(name)) => {
            let policy = policies
                .get(reference)
                .context("missing selected retained policy")?;
            ensure!(policy.name == *name, "retained policy name mismatch");
            Some(RetainedPolicy {
                reference: reference.clone(),
                name: name.clone(),
                document: policy.document.clone(),
                routing_source: source.routing.clone(),
            })
        }
        (None, None) => None,
        _ => anyhow::bail!("invalid selected retained policy"),
    };
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut chains = Vec::new();
    for chain in route.chains {
        // Public resource IDs and internal selectors are separate namespaces:
        // ingress resolve_model maps the literal resource ID to the base;
        // policy selection uses route_resolved directly with the retained tier.
        let mut indices = Vec::new();
        for approved in chain.targets {
            *target_count = target_count
                .checked_add(1)
                .context(ExecutionCapacityExceeded)?;
            ensure!(*target_count <= MAX_TARGETS, ExecutionCapacityExceeded);
            native_budget.target(&approved.target)?;
            let (target, projection) = target::decode(approved.target)?;
            *content_bytes = content_bytes
                .checked_add(projection.len().saturating_mul(2))
                .context(ExecutionCapacityExceeded)?;
            ensure!(
                *content_bytes <= MAX_TARGET_CONTENT_BYTES,
                ExecutionCapacityExceeded
            );
            ensure!(
                target.api_protocol.operation() == chain.inbound.operation(),
                "retained operation mismatch"
            );
            let index = if let Some(index) = candidates
                .iter()
                .position(|candidate| candidate.projection == projection)
            {
                ensure!(
                    candidates[index].credential == approved.credential,
                    "ambiguous retained target credential"
                );
                index
            } else {
                let index = candidates.len();
                candidates.push(Candidate {
                    routing: crate::tenancy::routing::Metadata::from_config(
                        &runtime_config,
                        &target,
                    ),
                    target,
                    projection,
                    credential: approved.credential,
                });
                index
            };
            let candidate = &candidates[index];
            ensure!(
                source.credentials.iter().any(|binding| binding.provider
                    == candidate.target.provider_name
                    && binding.account
                        == candidate
                            .target
                            .account_label
                            .as_deref()
                            .unwrap_or("default")
                    && binding.credential == candidate.credential),
                "target is outside the selected credential bindings"
            );
            indices.push(index);
        }
        let mut preferences = runtime_config.resolve_router(&chain.selector)?.prefs;
        crate::config::routing_table::merge_prefs(&mut preferences, &root_preferences);
        chains.push(Chain {
            sort: preferences.sort,
            selector: chain.selector,
            inbound: chain.inbound,
            candidates: indices,
        });
    }
    ensure!(
        chains.iter().any(|chain| chain.selector == route.selector),
        "retained base chain missing"
    );
    if let Some(block) = &source.block {
        block.validate(&route.selector)?;
        ensure!(
            policy.is_none() && source.choice.is_none(),
            "unsupported Block closure"
        );
        for selector in [route.selector.as_str(), block.challenger()?] {
            ensure!(
                chains.iter().any(|chain| chain.selector == selector
                    && chain.inbound.operation()
                        == bitrouter_ai::types::ModelOperation::Generation),
                "retained Block arm is missing"
            );
        }
    }
    let mut block_pricing = Vec::new();
    if source.block.is_some() {
        let config = native_budget.block_config(&source.routing)?;
        native_budget.block_pricing_bytes(
            candidates
                .len()
                .checked_mul(std::mem::size_of::<
                    Option<crate::gateway::pipeline::stream::UsagePricing>,
                >())
                .context(ExecutionCapacityExceeded)?,
        )?;
        block_pricing.reserve_exact(candidates.len());
        for candidate in &candidates {
            let tiers = config
                .providers
                .get(&candidate.target.provider_name)
                .and_then(|provider| provider.model_config(&candidate.target.service_id))
                .and_then(|model| model.pricing_for(&candidate.target.api_protocol))
                .map_or(0, |pricing| pricing.context_tiers.len());
            native_budget.block_pricing_bytes(
                tiers
                    .checked_mul(std::mem::size_of::<
                        crate::gateway::pipeline::stream::UsagePricingTier,
                    >())
                    .context(ExecutionCapacityExceeded)?,
            )?;
            let pricing =
                crate::config::routing_table::retained_usage_pricing(&config, &candidate.target);
            ensure!(
                pricing
                    .as_ref()
                    .is_none_or(|price| price.registry_price.is_none()),
                "registry pricing is not qualified by the owned manifest"
            );
            block_pricing.push(pricing);
        }
    }
    let native = native_v1::derive(
        &route.selector,
        &source.routing,
        &source.credentials,
        &chains,
        &candidates,
        native_budget,
    );
    native_budget.check_operation()?;
    Ok(RouteData {
        reference: route.reference,
        assessment: source.assessment,
        block: source.block,
        block_pricing,
        dependencies,
        selector: route.selector,
        overrides: PromptOverrides {
            system_prompt: route.system_prompt,
            params: route.defaults,
        },
        policy,
        chains,
        native,
        candidates,
        choice: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_ai::tenancy::authority::OwnerKind;

    fn reference(kind: ResourceKind, name: &str) -> Result<ResourceRef> {
        Ok(ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: "tenant".to_owned().try_into()?,
            resource_kind: kind,
            resource_id: name.to_owned().try_into()?,
            revision_id: "v1".to_owned().try_into()?,
        })
    }

    #[test]
    fn selected_graph_traverses_nested_edges_once_without_unrelated_roots() -> Result<()> {
        let root = reference(ResourceKind::Route, "root")?;
        let policy = reference(ResourceKind::Policy, "policy")?;
        let nested_policy = reference(ResourceKind::Policy, "nested-policy")?;
        let nested_route = reference(ResourceKind::Route, "nested-route")?;
        let slot = reference(ResourceKind::Credential, "slot")?;
        let unrelated = reference(ResourceKind::Route, "unrelated")?;
        // The current import profile rejects nested route/policy dependencies.
        // Test graph traversal independently so future qualification cannot
        // accidentally reduce authorization to the direct policy edge.
        let graph = HashMap::from([
            (root.clone(), vec![policy.clone()]),
            (policy.clone(), vec![nested_policy.clone()]),
            (
                nested_policy.clone(),
                vec![nested_route.clone(), slot.clone()],
            ),
            (nested_route.clone(), vec![root.clone(), slot.clone()]),
            (unrelated, vec![]),
        ]);
        let retained = dependencies(&root, &graph)?;
        assert_eq!(retained.len(), 5);
        assert_eq!(
            retained.into_iter().collect::<HashSet<_>>(),
            HashSet::from([
                root.clone(),
                policy,
                nested_policy.clone(),
                nested_route,
                slot,
            ])
        );
        let mut missing = graph;
        missing.remove(&nested_policy);
        assert!(dependencies(&root, &missing).is_err());
        Ok(())
    }
}
