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
}

struct Policy {
    name: String,
    document: Arc<PolicyLock>,
}

pub(super) fn decode(envelope: Envelope) -> Result<ValidatedEnvelope> {
    let Envelope {
        document,
        routes,
        artifacts,
    } = envelope;
    let mut sources = HashMap::new();
    let mut policies = HashMap::new();
    let mut graph = HashMap::new();
    for Artifact {
        reference,
        definition,
        envelope: _,
    } in artifacts
    {
        graph.insert(reference.clone(), definition.dependencies());
        match definition {
            Definition::Route {
                routing,
                credentials,
                ..
            } => {
                sources.insert(
                    reference,
                    Source {
                        routing: Arc::new(routing),
                        credentials,
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
            Definition::Credential { .. } => {
                anyhow::bail!("credential plaintext in compiled envelope")
            }
        }
    }
    let mut retained = HashMap::new();
    let mut target_count = 0_usize;
    let mut content_bytes = 0_usize;
    let mut dependency_count = 0_usize;
    let mut native_budget = native_v1::Budget::new();
    for route in routes {
        let dependencies = dependencies(&route.reference, &graph)?;
        dependency_count = dependency_count
            .checked_add(dependencies.len())
            .context(ExecutionCapacityExceeded)?;
        ensure!(
            dependency_count <= MAX_ROUTE_DEPENDENCIES,
            ExecutionCapacityExceeded
        );
        let source = sources
            .remove(&route.reference)
            .context("missing retained route source")?;
        let data = route_data(
            route,
            source,
            dependencies,
            &policies,
            &mut target_count,
            &mut content_bytes,
            &mut native_budget,
        )?;
        ensure!(
            retained
                .insert(data.reference.resource_id.to_string(), Arc::new(data))
                .is_none(),
            "duplicate visible retained route"
        );
    }
    Ok(ValidatedEnvelope {
        document,
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
    native_budget: &mut native_v1::Budget,
) -> Result<RouteData> {
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
        chains.push(Chain {
            selector: chain.selector,
            inbound: chain.inbound,
            candidates: indices,
        });
    }
    ensure!(
        chains.iter().any(|chain| chain.selector == route.selector),
        "retained base chain missing"
    );
    let native = native_v1::derive(
        &route.selector,
        &source.routing,
        &source.credentials,
        &chains,
        &candidates,
        native_budget,
    );
    Ok(RouteData {
        reference: route.reference,
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
