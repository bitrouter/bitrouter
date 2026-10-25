use super::*;
use bitrouter_ai::tenancy::configuration::{
    Activation, Configuration, ConfigurationPut, ConfigurationRevision, GenerationDefaults,
    Learning, LearningMode, Limits,
};

fn empty_document() -> Configuration {
    Configuration {
        routes: vec![],
        policy: None,
        generation_defaults: GenerationDefaults {
            max_output_tokens: 1024,
            temperature: 0.0,
        },
        learning: Learning {
            mode: LearningMode::Off,
            max_evaluations: 0,
        },
        limits: Limits {
            active_executions: 1,
            queued_work: 0,
            learning_jobs: 0,
        },
    }
}

fn retained(value: StoredResponse) -> Result<ConfigurationRevision> {
    match value {
        StoredResponse::Configuration(value) => Ok(value),
        _ => anyhow::bail!("expected retained configuration"),
    }
}

async fn replace_configuration_fields(
    fixture: &Fixture,
    revision: &bitrouter_ai::tenancy::identity::RevisionId,
    fields: Vec<(&str, String)>,
) -> Result<()> {
    use sea_orm::{
        ConnectionTrait,
        sea_query::{Alias, Expr, Query},
    };
    let mut update = Query::update();
    update
        .table(Alias::new("tenancy_configurations"))
        .and_where(Expr::col(Alias::new("tenant_id")).eq(fixture.tenant.as_str()))
        .and_where(Expr::col(Alias::new("revision_id")).eq(revision.as_str()));
    for (field, value) in fields {
        update.value(Alias::new(field), value);
    }
    ensure!(
        fixture
            .db
            .execute(fixture.db.get_database_backend().build(&update))
            .await?
            .rows_affected()
            == 1
    );
    Ok(())
}

#[tokio::test]
async fn configuration_get_authenticates_public_metadata_before_returning_it() -> Result<()> {
    for changed_field in ["document_json", "content_digest"] {
        let fixture = Fixture::new().await?;
        let revision = retained(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Configuration(
                        fixture.tenant.clone(),
                        fixture.configuration_input("draft-for-read")?,
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
        let mut document = revision.document;
        document.generation_defaults.temperature = 1.0;
        let replacement = if changed_field == "document_json" {
            serde_json::to_string(&document)?
        } else {
            "0".repeat(64)
        };
        replace_configuration_fields(
            &fixture,
            &revision.revision_id,
            vec![(changed_field, replacement)],
        )
        .await?;
        ensure!(
            matches!(fixture.service.configuration(&fixture.operator, &fixture.tenant, &revision.revision_id).await,
            Err(error) if error.0.code == ErrorCode::StorageUnavailable)
        );
        ensure!(fixture.service.current().is_err());
        fixture.service.shutdown().await?;
    }
    Ok(())
}

#[cfg(not(feature = "core"))]
#[tokio::test]
async fn minimal_build_rejects_activation_and_restore_of_retained_learning() -> Result<()> {
    use sha2::{Digest, Sha256};
    for was_active in [false, true] {
        let fixture = Fixture::new().await?;
        let revision = retained(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Configuration(
                        fixture.tenant.clone(),
                        fixture.configuration_input("full-build-draft")?,
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
        let activation = Activation {
            operation_id: "activate-learning".try_into()?,
            expected_active_revision: None,
            revision_id: revision.revision_id.clone(),
            expected_authority_revision: fixture.service.current().map_err(failure)?.revision,
            installation_id: fixture.installation.clone(),
        };
        if was_active {
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Activate(fixture.tenant.clone(), activation.clone()),
                )
                .await
                .map_err(failure)?;
            fixture.service.shutdown().await?;
        }
        // Preserve a valid compiler binding, as produced by a full build, then
        // exercise the minimal binary's activation/restart boundary.
        let mut document = revision.document;
        document.learning.mode = LearningMode::Observe;
        let compiled = fixture
            .service
            .resources
            .compile(
                &fixture.db,
                &fixture.tenant,
                &revision.revision_id,
                &document,
            )
            .await
            .map_err(|code| anyhow::anyhow!("fixture compiler rejected: {code:?}"))?;
        let mut public = serde_json::to_value(&document)?;
        super::super::command::sort(&mut public);
        let digest = hex::encode(Sha256::digest(serde_json::to_vec(&public)?));
        replace_configuration_fields(
            &fixture,
            &revision.revision_id,
            vec![
                ("document_json", serde_json::to_string(&document)?),
                ("compiled_json", serde_json::to_string(&compiled.payload)?),
                ("content_digest", digest),
            ],
        )
        .await?;
        let stored = super::super::configuration_store::configuration(
            &fixture.db,
            &fixture.tenant,
            &revision.revision_id,
        )
        .await?
        .context("retained fixture missing")?;
        fixture
            .service
            .resources
            .validate_compiled(&stored)
            .map_err(|code| anyhow::anyhow!("fixture integrity rejected: {code:?}"))?;
        if was_active {
            ensure!(Service::open(fixture.db.clone(), None).await.is_err());
        } else {
            ensure!(
                matches!(fixture.service.execute(fixture.operator.clone(), Command::Activate(fixture.tenant.clone(), activation)).await,
                Err(error) if error.0.code == ErrorCode::InvalidRequest)
            );
            ensure!(
                fixture
                    .service
                    .active_revision(&fixture.operator, &fixture.tenant)
                    .map_err(failure)?
                    .revision_id
                    .is_none()
            );
            ensure!(fixture.service.current().is_ok());
            fixture.service.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn invalid_retained_bytes_fence_activation_and_startup() -> Result<()> {
    use sea_orm::{
        ConnectionTrait,
        sea_query::{Alias, Expr, Query},
    };
    for was_active in [false, true] {
        let fixture = Fixture::new().await?;
        let revision = retained(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Configuration(
                        fixture.tenant.clone(),
                        fixture.configuration_input("retained-before-corruption")?,
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
        let activation = Activation {
            operation_id: "activate-retained".try_into()?,
            expected_active_revision: None,
            revision_id: revision.revision_id.clone(),
            expected_authority_revision: fixture.service.current().map_err(failure)?.revision,
            installation_id: fixture.installation.clone(),
        };
        if was_active {
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Activate(fixture.tenant.clone(), activation.clone()),
                )
                .await
                .map_err(failure)?;
            fixture.service.shutdown().await?;
        }
        let update = Query::update()
            .table(Alias::new("tenancy_configurations"))
            .value(
                Alias::new("compiled_json"),
                r#"{"kind":"empty","binding":null}"#,
            )
            .and_where(Expr::col(Alias::new("tenant_id")).eq(fixture.tenant.as_str()))
            .and_where(Expr::col(Alias::new("revision_id")).eq(revision.revision_id.as_str()))
            .to_owned();
        ensure!(
            fixture
                .db
                .execute(fixture.db.get_database_backend().build(&update))
                .await?
                .rows_affected()
                == 1
        );
        if was_active {
            ensure!(Service::open(fixture.db.clone(), None).await.is_err());
        } else {
            let result = fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Activate(fixture.tenant.clone(), activation.clone()),
                )
                .await;
            ensure!(
                matches!(result, Err(error) if error.0.code == ErrorCode::AuthorityUnavailable)
            );
            ensure!(fixture.service.current().is_err());
            ensure!(
                super::super::configuration_store::active_revisions(&fixture.db)
                    .await?
                    .is_empty()
            );
            let Caller::Installation(principal) = &fixture.operator else {
                anyhow::bail!("expected operator fixture");
            };
            ensure!(
                super::super::store::operation(
                    &fixture.db,
                    &fixture.installation,
                    principal,
                    &activation.operation_id,
                )
                .await?
                .is_none()
            );
            fixture.service.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn retained_nonempty_configuration_requires_every_credential_dependency() -> Result<()> {
    use bitrouter_ai::tenancy::{
        authority::{OwnerKind, ResourceKind},
        configuration::ResourceRef,
    };
    use serde_json::json;
    let mut fixture = Fixture::new().await?;
    fixture.service.shutdown().await?;
    let resources =
        super::super::resources::Resources::with_key(fixture.installation.clone(), [17; 32])?;
    let reference = |kind, id: &str| -> Result<ResourceRef> {
        Ok(ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: fixture.tenant.as_str().try_into()?,
            resource_kind: kind,
            resource_id: id.try_into()?,
            revision_id: "retained-v1".try_into()?,
        })
    };
    let route = reference(ResourceKind::Route, "provider:opaque-route")?;
    let slot = reference(ResourceKind::Credential, "explicit-slot")?;
    resources.provision_bytes(&fixture.db, &serde_json::to_vec(&json!({
        "format": 1, "installation_id": fixture.installation,
        "resources": [
            {"reference": slot, "definition": {"kind": "credential", "credential": {"type": "api_key", "data": {"value": "private-provider-secret"}}}},
            {"reference": route, "definition": {
                "kind": "route", "selector": "mock:retained-model", "policy": null,
                "routing": {"providers": {"mock": {"api_base": "https://example.invalid/v1", "models": [{"id": "retained-model"}]}}},
                "credentials": [{"provider": "mock", "account": "default", "credential": slot}]
            }}
        ]
    }))?).await?;
    let snapshot = super::super::store::load(&fixture.db, Utc::now()).await?;
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        None,
        super::super::actor::OperationLimits::default(),
        snapshot,
        std::sync::Arc::new(resources),
    )?;
    let credential = fixture.issue("resource-editor").await?;
    let caller = fixture
        .service
        .authenticate(credential.secret.as_deref().context("secret missing")?)
        .map_err(failure)?;
    let mut input = fixture.configuration_input("owned-route-draft")?;
    input.document.routes.push(route.clone());
    let r1 = retained(
        fixture
            .service
            .execute(
                caller.clone(),
                Command::Configuration(fixture.tenant.clone(), input.clone()),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(r1.document == input.document);
    let stored = super::super::configuration_store::configuration(
        &fixture.db,
        &fixture.tenant,
        &r1.revision_id,
    )
    .await?
    .context("retained configuration missing")?;
    ensure!(stored.dependencies.contains(&route) && stored.dependencies.contains(&slot));
    let serialized = serde_json::to_string(&stored.compiled)?;
    ensure!(!serialized.contains("private-provider-secret"));
    let narrow = BindingPut {
        operation_id: "remove-credential-scope".try_into()?,
        expected_revision: fixture.binding_result.resource_revision.clone(),
        principal_id: fixture.principal.clone(),
        role: TenantRole::TenantAdmin,
        scope: GrantScope::Resources {
            resources: vec![route.key()],
        },
        expires_at: credential.expires_at,
        expected_management_owner: Some(PrincipalId::for_control_credential("test-operator")?),
        installation_id: fixture.installation.clone(),
    };
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Binding(fixture.tenant.clone(), fixture.binding.clone(), narrow),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(
        matches!(fixture.service.configuration(&caller, &fixture.tenant, &r1.revision_id).await, Err(error) if error.0.code == ErrorCode::Forbidden)
    );
    ensure!(
        matches!(fixture.service.execute(caller.clone(), Command::Configuration(fixture.tenant.clone(), input.clone())).await, Err(error) if error.0.code == ErrorCode::Forbidden)
    );
    input.operation_id = "cannot-compile-without-slot".try_into()?;
    input.expected_authority_revision = fixture.service.current().map_err(failure)?.revision;
    ensure!(
        matches!(fixture.service.execute(caller, Command::Configuration(fixture.tenant.clone(), input)).await, Err(error) if error.0.code == ErrorCode::Forbidden)
    );
    // Shape-valid removal of a transitive dependency cannot bypass the narrowed
    // binding: reads authenticate the closure before trusting its requirements.
    replace_configuration_fields(
        &fixture,
        &r1.revision_id,
        vec![("dependencies_json", serde_json::to_string(&vec![route])?)],
    )
    .await?;
    let caller = fixture
        .service
        .authenticate(credential.secret.as_deref().context("secret missing")?)
        .map_err(failure)?;
    ensure!(
        matches!(fixture.service.configuration(&caller, &fixture.tenant, &r1.revision_id).await,
        Err(error) if error.0.code == ErrorCode::StorageUnavailable)
    );
    fixture.service.shutdown().await?;
    Ok(())
}

impl Fixture {
    pub(super) fn configuration_input(&self, id: &str) -> Result<ConfigurationPut> {
        Ok(ConfigurationPut {
            operation_id: id.try_into()?,
            expected_authority_revision: self.service.current().map_err(failure)?.revision,
            document: empty_document(),
            installation_id: self.installation.clone(),
        })
    }
}

#[tokio::test]
async fn drafts_are_immutable_and_activation_rollback_is_cas_bound() -> Result<()> {
    let fixture = Fixture::new().await?;
    let input = fixture.configuration_input("prepare-r1")?;
    let r1 = retained(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Configuration(fixture.tenant.clone(), input.clone()),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(r1.document == input.document);
    ensure!(
        fixture
            .service
            .active_revision(&fixture.operator, &fixture.tenant)
            .map_err(failure)?
            .revision_id
            .is_none()
    );
    let replay = retained(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Configuration(fixture.tenant.clone(), input.clone()),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(replay.revision_id == r1.revision_id);
    let journal = fixture
        .service
        .operation(&fixture.operator, &input.operation_id)
        .await
        .map_err(failure)?;
    ensure!(journal.resource_revision.as_ref() == Some(&r1.revision_id));
    let mut changed = input.clone();
    changed.document.generation_defaults.max_output_tokens = 64;
    ensure!(
        matches!(fixture.service.execute(fixture.operator.clone(), Command::Configuration(fixture.tenant.clone(), changed)).await,
        Err(error) if error.0.code == ErrorCode::OperationConflict)
    );
    let activation = Activation {
        operation_id: "activate-r1".try_into()?,
        expected_active_revision: None,
        revision_id: r1.revision_id.clone(),
        expected_authority_revision: fixture.service.current().map_err(failure)?.revision,
        installation_id: fixture.installation.clone(),
    };
    let activated = result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Activate(fixture.tenant.clone(), activation.clone()),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(activated.resource_revision.as_ref() == Some(&r1.revision_id));
    let mut r2_input = fixture.configuration_input("prepare-r2")?;
    r2_input.document.generation_defaults.max_output_tokens = 64;
    let r2 = retained(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Configuration(fixture.tenant.clone(), r2_input),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(r1.revision_id != r2.revision_id);
    ensure!(
        fixture
            .service
            .active_revision(&fixture.operator, &fixture.tenant)
            .map_err(failure)?
            .revision_id
            .as_ref()
            == Some(&r1.revision_id)
    );
    let mut stale = activation;
    stale.operation_id = "stale-activate-r2".try_into()?;
    stale.revision_id = r2.revision_id.clone();
    stale.expected_authority_revision = fixture.service.current().map_err(failure)?.revision;
    ensure!(
        matches!(fixture.service.execute(fixture.operator.clone(), Command::Activate(fixture.tenant.clone(), stale.clone())).await,
        Ok(StoredResponse::Failure(error)) if error.code == ErrorCode::RevisionConflict)
    );
    stale.operation_id = "activate-r2".try_into()?;
    stale.expected_active_revision = Some(r1.revision_id.clone());
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Activate(fixture.tenant.clone(), stale),
            )
            .await
            .map_err(failure)?,
    )?;
    let rollback = Activation {
        operation_id: "rollback-r1".try_into()?,
        expected_active_revision: Some(r2.revision_id),
        revision_id: r1.revision_id.clone(),
        expected_authority_revision: fixture.service.current().map_err(failure)?.revision,
        installation_id: fixture.installation.clone(),
    };
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Activate(fixture.tenant.clone(), rollback),
            )
            .await
            .map_err(failure)?,
    )?;
    fixture.service.shutdown().await?;
    let restarted = Service::open(fixture.db.clone(), None).await?;
    ensure!(
        restarted
            .active_revision(&fixture.operator, &fixture.tenant)
            .map_err(failure)?
            .revision_id
            == Some(r1.revision_id.clone())
    );
    let recovered = restarted
        .configuration(&fixture.operator, &fixture.tenant, &r1.revision_id)
        .await
        .map_err(failure)?;
    ensure!(recovered.document == input.document && recovered.content_digest == r1.content_digest);
    let capabilities = restarted.capabilities(&fixture.operator).map_err(failure)?;
    ensure!(
        !capabilities.dispatch_enabled
            && !capabilities.strict_receipts
            && !capabilities.revision_bound_dispatch
    );
    restarted.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn stale_authority_cannot_publish_a_configuration() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut input = fixture.configuration_input("stale-draft")?;
    input.expected_authority_revision = 0_u64.try_into()?;
    let result = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(fixture.tenant.clone(), input.clone()),
        )
        .await
        .map_err(failure)?;
    ensure!(
        matches!(result, StoredResponse::Failure(error) if error.code == ErrorCode::RevisionConflict)
    );
    let saved = fixture
        .service
        .operation(&fixture.operator, &input.operation_id)
        .await
        .map_err(failure)?;
    ensure!(saved.resource_revision.is_none());
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn read_pressure_rejects_before_storage_without_fencing_authority() -> Result<()> {
    let fixture = Fixture::new().await?;
    let permits = fixture.service.reads.clone().try_acquire_many_owned(8)?;
    ensure!(
        matches!(fixture.service.operation(&fixture.operator, &"create-tenant".try_into()?).await,
        Err(error) if error.0.code == ErrorCode::CapacityExceeded)
    );
    ensure!(
        matches!(fixture.service.configuration(&fixture.operator, &fixture.tenant, &"absent".try_into()?).await,
        Err(error) if error.0.code == ErrorCode::CapacityExceeded)
    );
    ensure!(fixture.service.current().is_ok());
    drop(permits);
    fixture
        .service
        .operation(&fixture.operator, &"create-tenant".try_into()?)
        .await
        .map_err(failure)?;
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn competing_activations_publish_only_one_observed_authority() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut revisions = Vec::new();
    for id in ["first-config", "second-config"] {
        let input = fixture.configuration_input(id)?;
        revisions.push(
            retained(
                fixture
                    .service
                    .execute(
                        fixture.operator.clone(),
                        Command::Configuration(fixture.tenant.clone(), input),
                    )
                    .await
                    .map_err(failure)?,
            )?
            .revision_id,
        );
    }
    let authority = fixture.service.current().map_err(failure)?.revision;
    let command = |index: usize| -> Result<Command> {
        Ok(Command::Activate(
            fixture.tenant.clone(),
            Activation {
                operation_id: format!("competing-activation-{index}").try_into()?,
                expected_active_revision: None,
                revision_id: revisions[index].clone(),
                expected_authority_revision: authority,
                installation_id: fixture.installation.clone(),
            },
        ))
    };
    let (first, second) = tokio::join!(
        fixture
            .service
            .execute(fixture.operator.clone(), command(0)?),
        fixture
            .service
            .execute(fixture.operator.clone(), command(1)?),
    );
    let responses = [first.map_err(failure)?, second.map_err(failure)?];
    let applied: Vec<_> = responses
        .iter()
        .filter_map(|response| match response {
            StoredResponse::Operation(result) => result.resource_revision.as_ref(),
            _ => None,
        })
        .collect();
    ensure!(applied.len() == 1);
    ensure!(
        responses
            .iter()
            .filter(|response| matches!(response,
        StoredResponse::Failure(error) if error.code == ErrorCode::RevisionConflict))
            .count()
            == 1
    );
    ensure!(
        fixture
            .service
            .active_revision(&fixture.operator, &fixture.tenant)
            .map_err(failure)?
            .revision_id
            .as_ref()
            == applied.first().copied()
    );
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn configuration_references_never_fall_back_to_a_foreign_or_global_route() -> Result<()> {
    use bitrouter_ai::tenancy::{
        authority::{OwnerKind, ResourceKind},
        configuration::ResourceRef,
    };
    let fixture = Fixture::new().await?;
    for (owner, expected) in [
        ("tenant-b", ErrorCode::Forbidden),
        ("tenant-a", ErrorCode::NotFound),
    ] {
        let mut input = fixture.configuration_input(&format!("foreign-config-{owner}"))?;
        input.document.routes.push(ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: owner.try_into()?,
            resource_kind: ResourceKind::Route,
            resource_id: "openai:gpt-4".try_into()?,
            revision_id: "missing".try_into()?,
        });
        ensure!(
            matches!(fixture.service.execute(fixture.operator.clone(), Command::Configuration(fixture.tenant.clone(), input)).await,
            Err(error) if error.0.code == expected)
        );
    }
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn empty_configuration_reads_and_replay_require_current_whole_tenant_scope() -> Result<()> {
    use bitrouter_ai::tenancy::authority::{OwnerKind, ResourceKey, ResourceKind};
    let fixture = Fixture::new().await?;
    let credential = fixture.issue("configuration-editor").await?;
    let caller = fixture
        .service
        .authenticate(credential.secret.as_deref().context("missing secret")?)
        .map_err(failure)?;
    let input = fixture.configuration_input("tenant-draft")?;
    let configuration = retained(
        fixture
            .service
            .execute(
                caller.clone(),
                Command::Configuration(fixture.tenant.clone(), input.clone()),
            )
            .await
            .map_err(failure)?,
    )?;
    fixture
        .service
        .configuration(&caller, &fixture.tenant, &configuration.revision_id)
        .await
        .map_err(failure)?;
    let narrowed = BindingPut {
        operation_id: "narrow-config-editor".try_into()?,
        expected_revision: fixture.binding_result.resource_revision.clone(),
        principal_id: fixture.principal.clone(),
        role: TenantRole::TenantAdmin,
        scope: GrantScope::Resources {
            resources: vec![ResourceKey {
                owner_kind: OwnerKind::Tenant,
                owner_id: fixture.tenant.as_str().try_into()?,
                resource_kind: ResourceKind::Route,
                resource_id: "one-route".try_into()?,
            }],
        },
        expires_at: credential.expires_at,
        expected_management_owner: Some(PrincipalId::for_control_credential("test-operator")?),
        installation_id: fixture.installation.clone(),
    };
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Binding(fixture.tenant.clone(), fixture.binding.clone(), narrowed),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(
        matches!(fixture.service.configuration(&caller, &fixture.tenant, &configuration.revision_id).await, Err(error) if error.0.code == ErrorCode::Forbidden)
    );
    ensure!(
        matches!(fixture.service.active_revision(&caller, &fixture.tenant), Err(error) if error.0.code == ErrorCode::Forbidden)
    );
    ensure!(
        matches!(fixture.service.execute(caller.clone(), Command::Configuration(fixture.tenant.clone(), input.clone())).await, Err(error) if error.0.code == ErrorCode::Forbidden)
    );
    ensure!(
        matches!(fixture.service.operation(&caller, &input.operation_id).await, Err(error) if error.0.code == ErrorCode::Forbidden)
    );
    fixture.service.shutdown().await?;
    Ok(())
}
