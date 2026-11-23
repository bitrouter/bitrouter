//! Tenant-neutral configured graph. Tenant wrappers borrow selected immutable roots.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::{
    tenancy::{
        admission::AdmissionProfile,
        authority::{OwnerKind, ResourceKind},
        catalog::{
            CandidateSourceV1, CatalogCandidatesV1, CatalogSelectionRequestV1, CatalogSelectionV1,
            CatalogTemplateV1,
        },
        configuration::{Configuration, GenerationDefaults, Limits, ResourceRef},
        identity::{AuthorityRevision, InstallationId, RevisionId, TenantId},
        ordered::GenerationProtocol,
    },
    types::ApiProtocol,
};
use serde::Serialize;

use super::{Candidate, Chain, RetainedExecutionRevision, RouteData, target};
use crate::{
    config::{AccountStrategy, Config},
    tenancy::resources::{Resources, compiler},
};

pub(in crate::tenancy::resources) mod owned;

/// One compiled file-owned graph, shared by all authenticated tenant wrappers.
pub(in crate::tenancy) struct Catalog {
    pub(in crate::tenancy) revision: RevisionId,
    installation: InstallationId,
    pool: String,
    roots: HashMap<String, Root>,
    credentials: HashMap<ResourceRef, String>,
    profiles: HashMap<ResourceRef, AdmissionProfile>,
    membership: HashMap<ResourceRef, HashMap<ResourceRef, HashSet<Vec<u8>>>>,
    references: HashSet<ResourceRef>,
}

struct Root {
    route: Arc<RouteData>,
    sources: Vec<Vec<CandidateSourceV1>>,
    templates: Vec<Template>,
}

struct Template {
    descriptor: CatalogTemplateV1,
    inbound: GenerationProtocol,
    target: crate::gateway::pipeline::types::RoutingTarget,
    routing: crate::tenancy::routing::Metadata,
    profile: ResourceRef,
    byok_profile: ResourceRef,
}

fn identity(resources: &Resources, purpose: &str, value: &impl Serialize) -> Result<String> {
    let key = resources
        .key
        .as_ref()
        .context("configured catalog requires wrapping key")?;
    key.fingerprint(
        "configured-catalog-v1",
        purpose,
        &serde_json::to_vec(value)?,
    )
}

fn reference(pool: &str, kind: ResourceKind, id: &str, revision: &str) -> Result<ResourceRef> {
    Ok(ResourceRef {
        owner_kind: OwnerKind::OperatorPool,
        owner_id: pool.try_into()?,
        resource_kind: kind,
        resource_id: id.try_into()?,
        revision_id: revision.try_into()?,
    })
}

impl Catalog {
    pub(in crate::tenancy) fn pool(&self) -> &str {
        &self.pool
    }

    pub(in crate::tenancy) fn compile(config: &Config, resources: &Resources) -> Result<Self> {
        let exposure = config
            .tenancy
            .catalog
            .as_ref()
            .context("configured catalog absent")?;
        exposure.validate()?;
        let mut source = config.clone();
        for provider in source.providers.values_mut() {
            provider.account_strategy = AccountStrategy::Failover;
        }
        let mut roots = HashMap::new();
        let mut credentials = HashMap::new();
        let mut profiles = HashMap::new();
        for (id, selector) in &exposure.routes {
            let resolution = source.resolve_router(selector)?;
            ensure!(
                resolution.policy.is_none() && resolution.request_checks.is_empty(),
                "configured catalog exposure does not support router policies or request checks"
            );
            canonical_defaults(&resolution.overrides)?;
            let mut candidates: Vec<Candidate> = Vec::new();
            let mut attachments = Vec::new();
            let mut chains = Vec::new();
            let mut sources = Vec::new();
            let mut templates = Vec::new();
            for inbound in [
                ApiProtocol::ChatCompletions,
                ApiProtocol::Messages,
                ApiProtocol::Responses,
                ApiProtocol::Custom(bitrouter_ai::tenancy::generate_content::PROTOCOL.into()),
            ] {
                let templates_start = templates.len();
                let mut prefs = resolution.prefs.clone();
                prefs.operation = inbound.operation();
                prefs.inbound_protocol = Some(inbound.clone());
                let targets = match crate::config::routing_table::resolve_clean_route_chain(
                    &source,
                    &resolution.clean_model,
                    &prefs,
                ) {
                    Ok(targets) => targets,
                    Err(_) => continue,
                };
                let protocol = super::generation_protocol(&inbound)?;
                // The native envelope seals its own absence/default choice.
                // File defaults that cannot enter that layer are not advertised
                // as a native exposure. The three canonical wires remain valid.
                if protocol == GenerationProtocol::GenerateContent
                    && !resolution.overrides.is_empty()
                {
                    continue;
                }
                let mut indices = Vec::new();
                let mut logical = Vec::new();
                for mut resolved in targets {
                    let account = resolved.account_label.as_deref().unwrap_or("default");
                    let canonical = resolved
                        .registry
                        .as_ref()
                        .and_then(|selected| selected.canonical_id.as_deref())
                        .or_else(|| {
                            source
                                .providers
                                .get(&resolved.provider_name)
                                .and_then(|provider| provider.model_config(&resolved.service_id))
                                .map(|model| model.id.as_str())
                        })
                        .unwrap_or(resolution.clean_model.as_str());
                    let source_id = format!("{}/{}/{canonical}", resolved.provider_name, account);
                    let member = exposure
                        .profiles
                        .get(&source_id)
                        .context("configured target has no exact source/profile membership")?;
                    ensure!(
                        resolved.api_key_override.is_none() && resolved.api_base_override.is_none(),
                        "configured catalog does not permit request credential/endpoint overrides"
                    );
                    let secret = std::mem::take(&mut resolved.api_key);
                    let slot = identity(
                        resources,
                        "account",
                        &(&resolved.provider_name, &resolved.account_label),
                    )?;
                    let credential_revision = identity(resources, "credential", &(&slot, &secret))?;
                    let credential = reference(
                        &exposure.pool_id,
                        ResourceKind::Credential,
                        &slot,
                        &credential_revision,
                    )?;
                    let available = !secret.is_empty();
                    if available {
                        credentials.insert(credential.clone(), secret);
                    }
                    let profile_id = identity(resources, "membership", &source_id)?;
                    let profile_revision = identity(resources, "rules", member)?;
                    let profile = reference(
                        &exposure.pool_id,
                        ResourceKind::Policy,
                        &profile_id,
                        &profile_revision,
                    )?;
                    profiles.insert(profile.clone(), member.clone());
                    let mut neutral = member.clone();
                    neutral.observe_rate_group_id = member
                        .observe_rate_group_id
                        .clone()
                        .or_else(|| member.rate.as_ref().map(|rate| rate.rate_group_id.clone()));
                    neutral.rate = None;
                    neutral.health = None;
                    neutral.health_group_id = None;
                    let neutral_id = identity(resources, "byok-membership", &source_id)?;
                    let neutral_revision = identity(resources, "rules", &neutral)?;
                    let byok_profile = reference(
                        &exposure.pool_id,
                        ResourceKind::Policy,
                        &neutral_id,
                        &neutral_revision,
                    )?;
                    profiles.insert(byok_profile.clone(), neutral);
                    let native = matches!(inbound, ApiProtocol::Custom(_))
                        || matches!(&resolved.api_protocol, ApiProtocol::Custom(name)
                            if name == bitrouter_ai::tenancy::generate_content::BRIDGE_PROTOCOL);
                    let encoded = compiler::target_value_scoped(&resolved, native)?;
                    let (target, projection) = target::decode(encoded)?;
                    let template_id = identity(
                        resources,
                        "template",
                        &(id, protocol, &source_id, &projection),
                    )?
                    .try_into()?;
                    templates.push(Template {
                        descriptor: CatalogTemplateV1 {
                            template_id,
                            source_id: source_id.clone(),
                            provider: target.provider_name.clone(),
                            account: target.account_label.clone(),
                            model: target.service_id.clone(),
                            outbound_protocol: target.api_protocol.clone(),
                            credential_ref: available.then(|| credential.clone()),
                            platform_profile: profile.clone(),
                            byok_profile: byok_profile.clone(),
                        },
                        inbound: protocol,
                        target: target.clone(),
                        routing: crate::tenancy::routing::Metadata::from_config(&source, &target),
                        profile: profile.clone(),
                        byok_profile,
                    });
                    if !available {
                        continue;
                    }
                    let ordinal = u16::try_from(indices.len())?;
                    ensure!(
                        usize::from(ordinal) < bitrouter_ai::tenancy::ordered::MAX_CANDIDATES,
                        "configured protocol root exceeds ordered candidate bound"
                    );
                    let index = candidates.len();
                    logical.push(CandidateSourceV1 {
                        candidate_index: ordinal,
                        source_id,
                        provider: target.provider_name.clone(),
                        account: target.account_label.clone(),
                        model: target.service_id.clone(),
                        outbound_protocol: target.api_protocol.clone(),
                    });
                    candidates.push(Candidate {
                        routing: crate::tenancy::routing::Metadata::from_config(&source, &target),
                        target,
                        projection,
                        credential: credential.clone(),
                    });
                    indices.push(index);
                    attachments.push(bitrouter_ai::tenancy::admission::AdmissionAttachment {
                        inbound_protocol: protocol,
                        candidate_index: ordinal,
                        credential,
                        profile,
                    });
                }
                ensure!(
                    templates.len() - templates_start
                        <= bitrouter_ai::tenancy::ordered::MAX_CANDIDATES,
                    "configured protocol root exceeds catalog template bound"
                );
                if templates
                    .iter()
                    .any(|template| template.inbound == protocol)
                {
                    chains.push(Chain {
                        selector: selector.clone(),
                        inbound,
                        candidates: indices,
                        sort: resolution.prefs.sort,
                    });
                    sources.push(logical);
                }
            }
            let version = identity(
                resources,
                "root",
                &(
                    id,
                    selector,
                    &resolution.overrides,
                    candidates
                        .iter()
                        .map(|candidate| {
                            (
                                &candidate.projection,
                                &candidate.credential,
                                &candidate.routing,
                            )
                        })
                        .collect::<Vec<_>>(),
                    &attachments,
                    templates
                        .iter()
                        .map(|template| {
                            (
                                &template.descriptor,
                                &template.profile,
                                &template.byok_profile,
                            )
                        })
                        .collect::<Vec<_>>(),
                ),
            )?;
            let root = reference(&exposure.pool_id, ResourceKind::Route, id, &version)?;
            let mut dependencies = vec![root.clone()];
            for attachment in &attachments {
                for reference in [&attachment.credential, &attachment.profile] {
                    if !dependencies.contains(reference) {
                        dependencies.push(reference.clone());
                    }
                }
            }
            roots.insert(
                id.clone(),
                Root {
                    route: Arc::new(RouteData {
                        reference: root.clone(),
                        authority_root: Some(root),
                        admission: Some(attachments),
                        dependencies,
                        selector: selector.clone(),
                        overrides: resolution.overrides,
                        policy: None,
                        chains,
                        candidates,
                    }),
                    sources,
                    templates,
                },
            );
        }
        let mut revisions: Vec<_> = roots.values().map(|root| &root.route.reference).collect();
        revisions.sort_by(|a, b| a.resource_id.as_str().cmp(b.resource_id.as_str()));
        let revision = identity(resources, "snapshot", &revisions)?.try_into()?;
        let mut membership: HashMap<ResourceRef, HashMap<ResourceRef, HashSet<Vec<u8>>>> =
            HashMap::new();
        let mut references: HashSet<_> =
            credentials.keys().chain(profiles.keys()).cloned().collect();
        for root in roots.values() {
            references.insert(root.route.reference.clone());
            for attachment in root.route.admission.iter().flatten() {
                let chain = root
                    .route
                    .chains
                    .iter()
                    .find(|chain| {
                        super::generation_protocol(&chain.inbound).ok()
                            == Some(attachment.inbound_protocol)
                    })
                    .context("configured membership protocol absent")?;
                let index = chain
                    .candidates
                    .get(usize::from(attachment.candidate_index))
                    .context("configured membership ordinal absent")?;
                let candidate = root
                    .route
                    .candidates
                    .get(*index)
                    .context("configured membership candidate absent")?;
                membership
                    .entry(candidate.credential.clone())
                    .or_default()
                    .entry(attachment.profile.clone())
                    .or_default()
                    .insert(candidate.projection.clone());
            }
        }
        Ok(Self {
            revision,
            installation: resources.installation.clone(),
            pool: exposure.pool_id.clone(),
            roots,
            credentials,
            profiles,
            membership,
            references,
        })
    }

    /// Only the selected root's Arc is copied; the platform graph is never tenant-owned.
    pub(in crate::tenancy) fn execution(
        &self,
        tenant: &TenantId,
        route: &str,
        authority_revision: AuthorityRevision,
    ) -> Result<RetainedExecutionRevision> {
        let root = self.roots.get(route).context("configured route absent")?;
        Ok(RetainedExecutionRevision {
            installation: self.installation.clone(),
            tenant: tenant.clone(),
            revision: self.revision.clone(),
            authority_revision,
            document: Configuration {
                routes: vec![root.route.reference.clone()],
                policy: None,
                generation_defaults: GenerationDefaults {
                    max_output_tokens: 0,
                    temperature: 1.0,
                },
                eval_publication: false,
                limits: Limits {
                    active_executions: 0,
                    queued_work: 0,
                },
            },
            routes: HashMap::from([(route.to_owned(), root.route.clone())]),
        })
    }

    pub(in crate::tenancy) fn candidates(
        &self,
        tenant: &TenantId,
        route: &str,
        protocol: GenerationProtocol,
        authority_revision: AuthorityRevision,
    ) -> Result<CatalogCandidatesV1> {
        let root = self.roots.get(route).context("configured route absent")?;
        let view = self.execution(tenant, route, authority_revision)?;
        let manifest = view
            .candidate_manifest(&root.route.reference, protocol, authority_revision)
            .map_err(|_| anyhow::anyhow!("configured protocol root unavailable"))?;
        let index = root
            .route
            .chains
            .iter()
            .position(|chain| super::generation_protocol(&chain.inbound).ok() == Some(protocol))
            .context("configured protocol root absent")?;
        Ok(CatalogCandidatesV1 {
            schema_version: 1,
            manifest,
            sources: root
                .sources
                .get(index)
                .context("configured source mapping absent")?
                .clone(),
            templates: root
                .templates
                .iter()
                .filter(|template| template.inbound == protocol)
                .map(|template| template.descriptor.clone())
                .collect(),
        })
    }

    pub(in crate::tenancy) fn prepare_with_targets(
        &self,
        resources: &Resources,
        tenant: &TenantId,
        input: &CatalogSelectionRequestV1,
        authority_revision: AuthorityRevision,
        owned: &HashMap<ResourceRef, crate::gateway::pipeline::types::RoutingTarget>,
    ) -> Result<(RetainedExecutionRevision, CatalogSelectionV1)> {
        use bitrouter_ai::tenancy::validation::Validate;
        input.validate()?;
        ensure!(
            input.catalog_revision == self.revision,
            "configured snapshot changed"
        );
        let root = self
            .roots
            .get(input.route_id.as_str())
            .context("configured root absent")?;
        let version = self.selection_revision(resources, input)?;
        let mut selected_reference = root.route.reference.clone();
        selected_reference.revision_id = version.clone();
        let mut dependencies = vec![root.route.reference.clone()];
        let mut candidates = Vec::new();
        let mut attachments = Vec::new();
        let mut sources = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut expanded_bytes = 0_usize;
        for (ordinal, selected) in input.candidates.iter().enumerate() {
            ensure!(
                seen.insert((
                    &selected.template_id,
                    &selected.credential_ref,
                    &selected.route_ref
                )),
                "duplicate configured candidate and credential"
            );
            let template = root
                .templates
                .iter()
                .find(|template| {
                    template.descriptor.template_id == selected.template_id
                        && template.inbound == input.inbound_protocol
                })
                .context("candidate is outside the configured root/protocol")?;
            let credential = match &selected.credential_ref {
                Some(credential) => {
                    ensure!(
                        credential.owner_kind == OwnerKind::Tenant
                            && credential.owner_id.as_str() == tenant.as_str()
                            && credential.resource_kind == ResourceKind::Credential,
                        "BYOK slot is outside the authenticated tenant"
                    );
                    credential.clone()
                }
                None => template
                    .descriptor
                    .credential_ref
                    .clone()
                    .context("configured template has no platform key")?,
            };
            let profile = if selected.credential_ref.is_some() {
                &template.byok_profile
            } else {
                &template.profile
            };
            for reference in [&credential, profile] {
                if !dependencies.contains(reference) {
                    dependencies.push(reference.clone());
                }
            }
            if let Some(route) = &selected.route_ref
                && !dependencies.contains(route)
            {
                dependencies.push(route.clone());
            }
            let candidate_index = u16::try_from(ordinal)?;
            let mut target = match &selected.route_ref {
                Some(route) => owned
                    .get(route)
                    .context("owned target proof absent")?
                    .clone(),
                None => template.target.clone(),
            };
            if selected.credential_ref.is_some() {
                // The private routing slot distinguishes credentials for the same
                // upstream. Public source metadata retains the declared account.
                target.account_label = Some(format!(
                    "owned.{}",
                    identity(resources, "dispatch-credential-slot", &credential)?
                ));
            }
            let projection = target::project(&target)?;
            expanded_bytes = expanded_bytes
                .checked_add(projection.len())
                .filter(|bytes| *bytes <= crate::tenancy::resources::MAX_CLOSURE_BYTES)
                .context("configured selection exceeds retained read expansion bound")?;
            candidates.push(Candidate {
                target,
                projection,
                credential: credential.clone(),
                routing: if selected.credential_ref.is_some() {
                    Default::default()
                } else {
                    template.routing.clone()
                },
            });
            attachments.push(bitrouter_ai::tenancy::admission::AdmissionAttachment {
                inbound_protocol: input.inbound_protocol,
                candidate_index,
                credential,
                profile: profile.clone(),
            });
            sources.push(CandidateSourceV1 {
                candidate_index,
                source_id: template.descriptor.source_id.clone(),
                provider: template.descriptor.provider.clone(),
                account: template.descriptor.account.clone(),
                model: template.descriptor.model.clone(),
                outbound_protocol: template.descriptor.outbound_protocol.clone(),
            });
        }
        let inbound = match input.inbound_protocol {
            GenerationProtocol::ChatCompletions => ApiProtocol::ChatCompletions,
            GenerationProtocol::Messages => ApiProtocol::Messages,
            GenerationProtocol::Responses => ApiProtocol::Responses,
            GenerationProtocol::GenerateContent => {
                ApiProtocol::Custom(bitrouter_ai::tenancy::generate_content::PROTOCOL.into())
            }
        };
        let data = Arc::new(RouteData {
            reference: selected_reference.clone(),
            authority_root: Some(root.route.reference.clone()),
            admission: Some(attachments),
            dependencies,
            selector: root.route.selector.clone(),
            overrides: root.route.overrides.clone(),
            policy: None,
            chains: vec![Chain {
                selector: root.route.selector.clone(),
                inbound,
                candidates: (0..candidates.len()).collect(),
                sort: root
                    .route
                    .chains
                    .first()
                    .context("configured root has no protocol")?
                    .sort,
            }],
            candidates,
        });
        let mut view = self.execution(tenant, input.route_id.as_str(), authority_revision)?;
        view.revision = version;
        view.document.routes = vec![selected_reference.clone()];
        view.routes = HashMap::from([(input.route_id.to_string(), data)]);
        let manifest = view
            .candidate_manifest(
                &selected_reference,
                input.inbound_protocol,
                authority_revision,
            )
            .map_err(|_| anyhow::anyhow!("prepared catalog root unavailable"))?;
        let prepared = CatalogSelectionV1 {
            selection: input.clone(),
            candidates: CatalogCandidatesV1 {
                schema_version: 1,
                manifest,
                sources,
                templates: Vec::new(),
            },
        };
        prepared.validate()?;
        Ok((view, prepared))
    }

    pub(in crate::tenancy) fn credential(&self, reference: &ResourceRef) -> Option<&str> {
        self.credentials.get(reference).map(String::as_str)
    }

    pub(in crate::tenancy) fn root_reference(&self, route: &str) -> Option<&ResourceRef> {
        self.roots.get(route).map(|root| &root.route.reference)
    }

    pub(in crate::tenancy) fn selection_revision(
        &self,
        resources: &Resources,
        input: &CatalogSelectionRequestV1,
    ) -> Result<RevisionId> {
        ensure!(
            input.catalog_revision == self.revision,
            "configured snapshot changed"
        );
        identity(resources, "selection", input)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid configured selection identity"))
    }

    pub(in crate::tenancy) fn require_membership(
        &self,
        credential: &ResourceRef,
        actual: &crate::gateway::pipeline::types::RoutingTarget,
        pinned: Option<&ResourceRef>,
    ) -> std::result::Result<(), bitrouter_ai::tenancy::operation::ErrorCode> {
        use bitrouter_ai::tenancy::operation::ErrorCode;
        if credential.owner_kind != OwnerKind::OperatorPool
            || credential.owner_id.as_str() != self.pool
        {
            return Ok(());
        }
        let pinned = pinned.ok_or(ErrorCode::Forbidden)?;
        if !self.credentials.contains_key(credential) || !self.profiles.contains_key(pinned) {
            return Err(ErrorCode::Forbidden);
        }
        let projection = target::project(actual).map_err(|_| ErrorCode::Forbidden)?;
        if self
            .membership
            .get(credential)
            .and_then(|profiles| profiles.get(pinned))
            .is_some_and(|projections| projections.contains(&projection))
        {
            return Ok(());
        }
        Err(ErrorCode::Forbidden)
    }

    pub(in crate::tenancy) fn contains(&self, reference: &ResourceRef) -> bool {
        self.references.contains(reference)
    }

    pub(in crate::tenancy) fn extend_facts(
        &self,
        state: &mut crate::tenancy::model::Snapshot,
        references: &[ResourceRef],
    ) {
        let mut admission = (*state.resource_admission).clone();
        for reference in references
            .iter()
            .filter(|reference| self.contains(reference))
        {
            let record = crate::tenancy::resources::lifecycle::Record {
                reference: reference.clone(),
                is_head: true,
                revoked: false,
                authority_revision: state.revision,
            };
            state.resource_lifecycle.insert(record.clone());
            if let Some(profile) = self.profiles.get(reference) {
                admission.record_configured_profile(reference, profile);
                admission.apply_lifecycle(&record);
            }
            if reference.resource_kind == ResourceKind::Route {
                admission.record_profiled_route(reference);
            }
        }
        state.resource_admission = Arc::new(admission);
    }
}

pub(in crate::tenancy) fn canonical_defaults(
    overrides: &crate::gateway::pipeline::routing::PromptOverrides,
) -> Result<bitrouter_ai::tenancy::generation_defaults::CanonicalGenerationDefaultsV1> {
    use bitrouter_ai::tenancy::validation::Validate;
    let mut value = serde_json::Map::from_iter([("schema_version".into(), 1.into())]);
    for name in [
        "temperature",
        "top_p",
        "max_tokens",
        "reasoning_effort",
        "top_k",
        "seed",
        "stop",
        "presence_penalty",
        "frequency_penalty",
    ] {
        if let Some(input) = overrides.params.get(name) {
            value.insert(name.into(), input.clone());
        }
    }
    for alias in ["max_completion_tokens", "max_output_tokens"] {
        if let Some(input) = overrides.params.get(alias) {
            ensure!(
                value
                    .get("max_tokens")
                    .is_none_or(|previous| previous == input),
                "conflicting configured token defaults"
            );
            value.insert("max_tokens".into(), input.clone());
        }
    }
    let defaults: bitrouter_ai::tenancy::generation_defaults::CanonicalGenerationDefaultsV1 =
        serde_json::from_value(serde_json::Value::Object(value))?;
    defaults.validate()?;
    Ok(defaults)
}
