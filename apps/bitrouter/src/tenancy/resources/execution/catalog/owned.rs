//! Borrow one authenticated tenant target; only its approved endpoint may differ.
//! Callers own read-work admission and current dependency authorization.

use std::collections::HashMap;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::{
    tenancy::{
        authority::{OwnerKind, ResourceKind},
        catalog::CatalogSelectionRequestV1,
        configuration::ResourceRef,
        identity::TenantId,
        ordered::GenerationProtocol,
        validation::Validate,
    },
    types::ApiProtocol,
};
use sea_orm::ConnectionTrait;

use super::super::{generation_protocol, target};
use super::{Catalog, Template, identity};
use crate::{
    gateway::{pipeline::types::RoutingTarget, providers::auth::credentials::Credential},
    tenancy::resources::{
        MAX_CLOSURE_BYTES, Resources,
        manifest::{Artifact, ByteBudget, CredentialBinding, Definition},
    },
};

impl Catalog {
    /// Authenticate exact owned-route revisions without executing their graph.
    /// Returned targets retain their approved base; preparation assigns the
    /// request-private account slot and the template's neutral BYOK profile.
    ///
    /// # Errors
    /// Rejects foreign references, missing/corrupt source, nonstatic or ambiguous
    /// targets, different credentials, defaults or changes beyond the endpoint.
    pub(in crate::tenancy) async fn owned_targets(
        &self,
        resources: &Resources,
        db: &impl ConnectionTrait,
        tenant: &TenantId,
        input: &CatalogSelectionRequestV1,
    ) -> Result<HashMap<ResourceRef, RoutingTarget>> {
        input.validate()?;
        ensure!(
            self.installation == resources.installation && self.revision == input.catalog_revision,
            "owned target catalog identity mismatch"
        );
        let root = self
            .roots
            .get(input.route_id.as_str())
            .context("configured root absent")?;
        let mut artifacts = HashMap::<ResourceRef, Artifact>::new();
        let mut targets = HashMap::<ResourceRef, RoutingTarget>::new();
        let mut budget = ByteBudget::new(MAX_CLOSURE_BYTES);
        for selected in &input.candidates {
            if let Some(credential) = &selected.credential_ref {
                require_owner(credential, tenant, ResourceKind::Credential)?;
                if !artifacts.contains_key(credential) {
                    let artifact = resources.load(db, credential).await.map_err(|_| {
                        anyhow::anyhow!("authenticated owned credential unavailable")
                    })?;
                    ensure!(
                        artifact.reference == *credential
                            && artifact.envelope.is_none()
                            && matches!(
                                &artifact.definition,
                                Definition::Credential {
                                    credential: Credential::ApiKey { .. },
                                    oauth_generation: 0,
                                    oauth: None,
                                }
                            ),
                        "owned selection requires an immutable static credential"
                    );
                    budget
                        .charge(&artifact)
                        .context("owned credential expansion exceeds byte budget")?;
                    artifacts.insert(credential.clone(), artifact);
                }
            }
            let Some(route) = &selected.route_ref else {
                continue;
            };
            let credential = selected
                .credential_ref
                .as_ref()
                .context("owned route requires BYOK credential")?;
            require_owner(route, tenant, ResourceKind::Route)?;
            require_owner(credential, tenant, ResourceKind::Credential)?;
            let template = root
                .templates
                .iter()
                .find(|template| {
                    template.descriptor.template_id == selected.template_id
                        && template.inbound == input.inbound_protocol
                })
                .context("owned route template is outside configured root/protocol")?;
            // A declared bridge membership stays on its operator-approved
            // callback for every inbound wire, including ordinary codecs.
            let bridge_source = root.templates.iter().any(|peer| {
                peer.descriptor.source_id == template.descriptor.source_id
                    && peer.descriptor.provider == template.descriptor.provider
                    && peer.descriptor.account == template.descriptor.account
                    && peer.descriptor.model == template.descriptor.model
                    && matches!(&peer.target.api_protocol, ApiProtocol::Custom(name)
                        if name == bitrouter_ai::tenancy::generate_content::BRIDGE_PROTOCOL)
            });
            require_supported(
                input.inbound_protocol,
                &template.target.api_protocol,
                bridge_source,
            )?;
            for reference in [route, credential] {
                if !artifacts.contains_key(reference) {
                    let artifact = resources
                        .load(db, reference)
                        .await
                        .map_err(|_| anyhow::anyhow!("authenticated owned resource unavailable"))?;
                    budget
                        .charge(&artifact)
                        .context("owned target read expansion exceeds byte budget")?;
                    artifacts.insert(reference.clone(), artifact);
                }
            }
            let secret = artifacts
                .get(credential)
                .context("owned credential absent")?;
            ensure!(
                matches!(
                    &secret.definition,
                    Definition::Credential {
                        credential: Credential::ApiKey { .. },
                        oauth_generation: 0,
                        oauth: None,
                    }
                ),
                "owned target requires immutable static credential"
            );
            let artifact = artifacts.get(route).context("owned route absent")?;
            let actual = decode_owned(artifact, credential, input.inbound_protocol)?;
            compare_template(resources, template, credential, &actual)?;
            if let Some(previous) = targets.get(route) {
                ensure!(
                    target::project(previous)? == target::project(&actual)?,
                    "owned route resolves to different selected targets"
                );
            } else {
                targets.insert(route.clone(), actual);
            }
        }
        Ok(targets)
    }
}

fn require_supported(
    inbound: GenerationProtocol,
    outbound: &ApiProtocol,
    bridge_source: bool,
) -> Result<()> {
    ensure!(
        !bridge_source && !matches!(outbound, ApiProtocol::Custom(_)),
        "owned route_ref is unsupported for a bridge source; retain its operator-approved private callback"
    );
    ensure!(
        matches!(
            inbound,
            GenerationProtocol::ChatCompletions
                | GenerationProtocol::Messages
                | GenerationProtocol::Responses
        ) && matches!(
            outbound,
            ApiProtocol::ChatCompletions | ApiProtocol::Messages | ApiProtocol::Responses
        ),
        "owned route_ref requires an ordinary codec-supported tenant route"
    );
    Ok(())
}

fn require_owner(reference: &ResourceRef, tenant: &TenantId, kind: ResourceKind) -> Result<()> {
    ensure!(
        reference.owner_kind == OwnerKind::Tenant
            && reference.owner_id.as_str() == tenant.as_str()
            && reference.resource_kind == kind,
        "owned route/credential is outside actual tenant"
    );
    Ok(())
}

fn static_source(artifact: &Artifact) -> Result<(&str, &[CredentialBinding])> {
    match &artifact.definition {
        Definition::Route {
            selector,
            credentials,
            policy,
            exploration,
            core_choice,
            assessment,
            block,
            ..
        } => {
            ensure!(
                policy.is_none()
                    && exploration.is_empty()
                    && core_choice.is_none()
                    && assessment.is_none()
                    && block.is_none(),
                "owned endpoint route must be static"
            );
            Ok((selector, credentials))
        }
        Definition::ProfiledRoute {
            selector,
            credentials,
            policy,
            ..
        } => {
            ensure!(policy.is_none(), "owned endpoint route has a policy");
            Ok((selector, credentials))
        }
        _ => anyhow::bail!("owned endpoint requires a route definition"),
    }
}

fn decode_owned(
    artifact: &Artifact,
    credential: &ResourceRef,
    inbound: bitrouter_ai::tenancy::ordered::GenerationProtocol,
) -> Result<RoutingTarget> {
    let (selector, credentials) = static_source(artifact)?;
    ensure!(
        credentials.len() == 1 && credentials[0].credential == *credential,
        "owned endpoint requires the exact single selected credential"
    );
    let envelope = artifact
        .envelope
        .as_ref()
        .context("owned route has no authenticated target envelope")?;
    ensure!(
        envelope.reference == artifact.reference
            && envelope.selector == selector
            && envelope.policy.is_none()
            && envelope.policy_name.is_none()
            && envelope.system_prompt.is_none()
            && envelope.defaults.is_empty(),
        "owned endpoint route carries foreign identity, policy or defaults"
    );
    // A static logical target may have several inbound codec views. Each must
    // have one target and the same exact credential; none can hide a fallback.
    ensure!(
        !envelope.chains.is_empty()
            && envelope.chains.iter().all(|chain| {
                chain.selector == selector
                    && chain.targets.len() == 1
                    && chain.targets[0].credential == *credential
            }),
        "owned endpoint route contains a fallback or another selector/credential"
    );
    let mut selected = envelope
        .chains
        .iter()
        .filter(|chain| generation_protocol(&chain.inbound).ok() == Some(inbound));
    let chain = selected
        .next()
        .context("owned route has no selected inbound protocol")?;
    ensure!(
        selected.next().is_none(),
        "owned route inbound target is ambiguous"
    );
    let (actual, _) = target::decode(chain.targets[0].target.clone())?;
    let binding = &credentials[0];
    ensure!(
        binding.provider == actual.provider_name
            && binding.account == actual.account_label.as_deref().unwrap_or("default"),
        "owned route credential slot association changed"
    );
    Ok(actual)
}

fn compare_template(
    resources: &Resources,
    template: &Template,
    credential: &ResourceRef,
    actual: &RoutingTarget,
) -> Result<()> {
    let private_slot = format!(
        "owned.{}",
        identity(resources, "dispatch-credential-slot", credential)?
    );
    ensure!(
        actual.account_label == template.target.account_label
            || actual.account_label.as_deref() == Some(private_slot.as_str()),
        "owned route account is not the configured or request-private slot"
    );
    let mut normalized = actual.clone();
    normalized.api_base.clone_from(&template.target.api_base);
    normalized
        .account_label
        .clone_from(&template.target.account_label);
    // The exhaustive target projection covers codec, auth scheme, compatibility,
    // reasoning/model constraints and every fixed header; no names-only match.
    ensure!(
        target::project(&normalized)? == target::project(&template.target)?,
        "owned route differs from configured template beyond approved endpoint"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenancy::{
        configuration_store,
        resources::{compiler, manifest::Import, storage},
    };
    use bitrouter_ai::tenancy::{
        catalog::CatalogCandidateV1, identity::RevisionId, ordered::GenerationProtocol,
    };
    use serde_json::json;

    fn reference(kind: ResourceKind, id: &str) -> Result<ResourceRef> {
        Ok(ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: "tenant".try_into()?,
            resource_kind: kind,
            resource_id: id.try_into()?,
            revision_id: "v1".try_into()?,
        })
    }

    fn route() -> Result<Artifact> {
        let import: Import = serde_json::from_value(json!({
            "reference": reference(ResourceKind::Route, "owned")?,
            "definition": {"kind":"route", "selector":"fixture:base", "policy":null,
                "routing":{"providers":{"fixture":{"api_base":"https://owned.example/v1",
                    "api_protocol":[{"*":"chat_completions"}],
                    "headers":{"x-fixed":"retained"}, "models":[{"id":"base"}]}}},
                "credentials":[{"provider":"fixture","account":"default","credential":reference(ResourceKind::Credential,"key")?}]}
        }))?;
        import.validate()?;
        let envelope = compiler::prepare_route(&import, None)?;
        Ok(Artifact {
            reference: import.reference,
            definition: import.definition,
            envelope: Some(envelope),
        })
    }

    fn catalog(resources: &Resources) -> Result<Catalog> {
        let config = crate::config::parse(
            r#"
inherit_defaults: false
control:
  enabled: true
  credentials:
    - id: owned-fixture-operator
      token_env: OWNED_FIXTURE_OPERATOR_TOKEN
      scopes: [control:tenancy]
providers:
  fixture:
    api_base: https://platform.example/v1
    api_key: private-platform-key
    api_protocol: [{"*": chat_completions}]
    headers: {x-fixed: retained}
    models: [{id: base}]
tenancy:
  enabled: true
  wrapping_key_env: OWNED_FIXTURE_WRAPPING_KEY
  catalog:
    pool_id: platform
    routes: {route: 'fixture:base'}
    profiles:
      fixture/default/base:
        schema_version: 2
        rate: null
        health: null
        health_group_id: null
        fallback: ordered_terminal_rejections_v1
"#,
        )?;
        Catalog::compile(&config, resources)
    }

    fn input(catalog: &Catalog) -> Result<CatalogSelectionRequestV1> {
        let template = catalog
            .roots
            .get("route")
            .context("fixture root missing")?
            .templates
            .iter()
            .find(|template| template.inbound == GenerationProtocol::ChatCompletions)
            .context("fixture template missing")?;
        Ok(CatalogSelectionRequestV1 {
            schema_version: 1,
            catalog_revision: catalog.revision.clone(),
            route_id: "route".try_into()?,
            inbound_protocol: GenerationProtocol::ChatCompletions,
            candidates: vec![CatalogCandidateV1 {
                template_id: template.descriptor.template_id.clone(),
                credential_ref: Some(reference(ResourceKind::Credential, "key")?),
                route_ref: Some(reference(ResourceKind::Route, "owned")?),
            }],
        })
    }

    #[test]
    fn owned_endpoint_preserves_every_template_field_except_approved_base() -> Result<()> {
        let resources = Resources::with_key("installation".try_into()?, [7; 32])?;
        let catalog = catalog(&resources)?;
        let template = catalog
            .roots
            .get("route")
            .context("root")?
            .templates
            .iter()
            .find(|template| template.inbound == GenerationProtocol::ChatCompletions)
            .context("template")?;
        let credential = reference(ResourceKind::Credential, "key")?;
        let actual = decode_owned(&route()?, &credential, GenerationProtocol::ChatCompletions)?;
        assert_eq!(actual.api_base, "https://owned.example/v1");
        compare_template(&resources, template, &credential, &actual)?;
        let mut extra_header = actual.clone();
        extra_header
            .headers
            .push(crate::gateway::pipeline::types::OutboundHeaderRule::new(
                "x-extra",
                Some("no"),
                false,
            )?);
        assert!(compare_template(&resources, template, &credential, &extra_header).is_err());
        let mut weakened = actual.clone();
        weakened.headers.clear();
        assert!(compare_template(&resources, template, &credential, &weakened).is_err());
        let mut changed = actual.clone();
        changed.chat_google_extensions = !changed.chat_google_extensions;
        assert!(compare_template(&resources, template, &credential, &changed).is_err());
        changed = actual.clone();
        changed.service_id = "other-model".into();
        assert!(compare_template(&resources, template, &credential, &changed).is_err());
        changed = actual;
        changed.account_label = Some("unrelated-account".into());
        assert!(compare_template(&resources, template, &credential, &changed).is_err());
        Ok(())
    }

    #[test]
    fn defaults_policy_fallback_and_different_selected_credential_are_rejected() -> Result<()> {
        let credential = reference(ResourceKind::Credential, "key")?;
        let mut artifact = route()?;
        artifact
            .envelope
            .as_mut()
            .context("envelope")?
            .defaults
            .insert("top_p".into(), json!(0.5));
        assert!(decode_owned(&artifact, &credential, GenerationProtocol::ChatCompletions).is_err());
        artifact = route()?;
        let chain = artifact
            .envelope
            .as_mut()
            .context("envelope")?
            .chains
            .first_mut()
            .context("chain")?;
        chain.targets.push(chain.targets[0].clone());
        assert!(decode_owned(&artifact, &credential, GenerationProtocol::ChatCompletions).is_err());
        artifact = route()?;
        if let Definition::Route { policy, .. } = &mut artifact.definition {
            *policy = Some(reference(ResourceKind::Policy, "unrelated")?);
        }
        assert!(decode_owned(&artifact, &credential, GenerationProtocol::ChatCompletions).is_err());
        assert!(
            decode_owned(
                &route()?,
                &reference(ResourceKind::Credential, "other-key")?,
                GenerationProtocol::ChatCompletions
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn endpoint_borrowing_keeps_bridge_callbacks_operator_approved() -> Result<()> {
        for protocol in [
            ApiProtocol::ChatCompletions,
            ApiProtocol::Messages,
            ApiProtocol::Responses,
        ] {
            require_supported(GenerationProtocol::ChatCompletions, &protocol, false)?;
            assert!(
                require_supported(GenerationProtocol::ChatCompletions, &protocol, true).is_err()
            );
        }
        let bridge =
            ApiProtocol::Custom(bitrouter_ai::tenancy::generate_content::BRIDGE_PROTOCOL.into());
        let error = require_supported(GenerationProtocol::ChatCompletions, &bridge, false)
            .err()
            .context("bridge unexpectedly accepted")?;
        assert!(
            error
                .to_string()
                .contains("operator-approved private callback")
        );
        assert!(
            require_supported(
                GenerationProtocol::GenerateContent,
                &ApiProtocol::ChatCompletions,
                false
            )
            .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn canonical_encrypted_lookup_preserves_base_and_rejects_corruption_and_foreign_tenant()
    -> Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let resources = Resources::with_key("installation".try_into()?, [7; 32])?;
        let catalog = catalog(&resources)?;
        let input = input(&catalog)?;
        let route = route()?;
        let credential = Artifact {
            reference: reference(ResourceKind::Credential, "key")?,
            envelope: None,
            definition: Definition::Credential {
                credential: Credential::ApiKey {
                    value: "private-owned-key".into(),
                },
                oauth_generation: 0,
                oauth: None,
            },
        };
        for artifact in [route, credential] {
            configuration_store::insert_resource(&db, &storage::seal(&resources, &artifact)?)
                .await?;
        }
        let tenant = "tenant".try_into()?;
        let targets = catalog
            .owned_targets(&resources, &db, &tenant, &input)
            .await?;
        assert_eq!(targets.len(), 1);
        assert_eq!(
            targets
                .get(&reference(ResourceKind::Route, "owned")?)
                .context("owned target")?
                .api_base,
            "https://owned.example/v1"
        );
        assert!(
            catalog
                .owned_targets(&resources, &db, &"other-tenant".try_into()?, &input)
                .await
                .is_err()
        );
        let wrong_key = Resources::with_key("installation".try_into()?, [8; 32])?;
        assert!(
            catalog
                .owned_targets(&wrong_key, &db, &tenant, &input)
                .await
                .is_err()
        );
        let mut platform = input.clone();
        platform.candidates[0].credential_ref = None;
        assert!(
            catalog
                .owned_targets(&resources, &db, &tenant, &platform)
                .await
                .is_err()
        );
        db.execute(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "UPDATE tenancy_resource_revisions SET payload_json='{}' WHERE resource_id='owned'",
        ))
        .await?;
        assert!(
            catalog
                .owned_targets(&resources, &db, &tenant, &input)
                .await
                .is_err()
        );
        let mut changed = input;
        changed.catalog_revision = RevisionId::try_from("different")?;
        assert!(
            catalog
                .owned_targets(&resources, &db, &tenant, &changed)
                .await
                .is_err()
        );
        Ok(())
    }
}
