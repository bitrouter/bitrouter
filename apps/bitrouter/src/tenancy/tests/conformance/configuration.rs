use super::*;
use sea_orm::ConnectionTrait;
use serde_json::json;

async fn provision(pool: bool) -> Result<(Fixture, ResourceRef, ResourceRef)> {
    let mut fixture = Fixture::new().await?;
    fixture.service.shutdown().await?;
    let resources =
        crate::tenancy::resources::Resources::with_key(fixture.installation.clone(), [17; 32])?;
    let reference = |kind, id: &str| -> Result<ResourceRef> {
        Ok(ResourceRef {
            owner_kind: if pool {
                OwnerKind::OperatorPool
            } else {
                OwnerKind::Tenant
            },
            owner_id: if pool {
                "conformance-pool"
            } else {
                fixture.tenant.as_str()
            }
            .try_into()?,
            resource_kind: kind,
            resource_id: id.try_into()?,
            revision_id: "v1".try_into()?,
        })
    };
    let route = reference(ResourceKind::Route, "literal-route")?;
    let slot = reference(ResourceKind::Credential, "static-slot")?;
    resources.provision_bytes(&fixture.db, &serde_json::to_vec(&json!({
        "format":1,"installation_id":fixture.installation,"resources":[
            {"reference":slot,"definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"conformance-only-secret"}}}},
            {"reference":route,"definition":{"kind":"route","selector":"mock:model","policy":null,
                "routing":{"providers":{"mock":{"api_base":"https://example.invalid/v1","models":[{"id":"model"}]}}},
                "credentials":[{"provider":"mock","account":"default","credential":slot}]}}
        ]
    }))?).await?;
    let snapshot = crate::tenancy::store::load(&fixture.db, Utc::now()).await?;
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        None,
        crate::tenancy::actor::OperationLimits::default(),
        snapshot,
        Arc::new(resources),
    )
    .await?;
    Ok((fixture, route, slot))
}

async fn draft(
    fixture: &Fixture,
    document: Configuration,
    id: &str,
) -> Result<ConfigurationRevision> {
    let mut input = fixture.configuration_input(id)?;
    input.document = document;
    match fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(fixture.tenant.clone(), input),
        )
        .await
        .map_err(failure)?
    {
        StoredResponse::Configuration(value) => Ok(value),
        _ => anyhow::bail!("operator could not prepare valid conformance document"),
    }
}

fn activation(fixture: &Fixture, revision: &RevisionId, id: &str) -> Result<Command> {
    let active = fixture
        .service
        .active_revision(&fixture.operator, &fixture.tenant)
        .map_err(failure)?;
    Ok(Command::Activate(
        fixture.tenant.clone(),
        Activation {
            operation_id: id.try_into()?,
            expected_active_revision: active.revision_id,
            expected_authority_revision: active.authority_revision,
            revision_id: revision.clone(),
            installation_id: fixture.installation.clone(),
        },
    ))
}

async fn count(fixture: &Fixture) -> Result<i64> {
    let row = fixture
        .db
        .query_one(sea_orm::Statement::from_string(
            fixture.db.get_database_backend(),
            "SELECT COUNT(*) AS n FROM tenancy_configurations".to_owned(),
        ))
        .await?
        .context("configuration count")?;
    Ok(row.try_get("", "n")?)
}

fn allowed(route: &ResourceRef, slot: &ResourceRef) -> GrantScope {
    GrantScope::Tenant {
        operator_pools: vec![route.key(), slot.key()],
    }
}

async fn issue_scoped(fixture: &Fixture, scope: GrantScope, id: &str) -> Result<CredentialIssued> {
    let mut input = fixture.issuance(id)?;
    input.scope = scope;
    issued(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(fixture.tenant.clone(), input),
            )
            .await
            .map_err(failure)?,
    )
}

async fn active_is(fixture: &Fixture, revision: &RevisionId) -> Result<()> {
    ensure!(
        fixture
            .service
            .active_revision(&fixture.operator, &fixture.tenant)
            .map_err(failure)?
            .revision_id
            .as_ref()
            == Some(revision)
    );
    Ok(())
}

#[tokio::test]
async fn tenant_wide_without_pool_cannot_compile_pool_route() -> Result<()> {
    let (fixture, route, _) = provision(true).await?;
    let checked = async {
        let credential = fixture.issue("ordinary-tenant-scope").await?;
        let identity = caller(&fixture, &credential)?;
        let mut input = fixture.configuration_input("ungranted-pool-draft")?;
        input.document.routes.push(route);
        let before = fixture.service.current().map_err(failure)?;
        let rows = count(&fixture).await?;
        denied(
            &fixture,
            identity,
            Command::Configuration(fixture.tenant.clone(), input),
            ErrorCode::Forbidden,
        )
        .await?;
        let after = fixture.service.current().map_err(failure)?;
        ensure!(after.revision == before.revision && after.operation_ids == before.operation_ids);
        ensure!(count(&fixture).await? == rows);
        Ok(())
    }
    .await;
    finish(fixture, checked).await
}

#[tokio::test]
async fn restricted_scope_cannot_activate_empty_snapshot() -> Result<()> {
    let (fixture, route, slot) = provision(false).await?;
    let checked = async {
        let empty = fixture.configuration_input("document")?.document;
        let mut nonempty = empty.clone();
        nonempty.routes.push(route.clone());
        let initial = draft(&fixture, nonempty, "initial-nonempty").await?;
        result(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    activation(&fixture, &initial.revision_id, "activate-initial")?,
                )
                .await
                .map_err(failure)?,
        )?;
        let empty = draft(&fixture, empty, "empty-candidate").await?;
        let scope = GrantScope::Resources {
            resources: vec![route.key(), slot.key()],
        };
        binding(
            &fixture,
            "narrow-active-manager",
            scope.clone(),
            TenantRole::TenantAdmin,
            expiry()?,
        )
        .await?;
        let credential = issue_scoped(&fixture, scope, "narrow-active-key").await?;
        let before = fixture.service.current().map_err(failure)?;
        denied(
            &fixture,
            caller(&fixture, &credential)?,
            activation(&fixture, &empty.revision_id, "forbidden-empty")?,
            ErrorCode::Forbidden,
        )
        .await?;
        let after = fixture.service.current().map_err(failure)?;
        ensure!(after.revision == before.revision && after.operation_ids == before.operation_ids);
        active_is(&fixture, &initial.revision_id).await?;
        // The same draft is valid and the same current CAS works for genuine installation authority.
        result(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    activation(&fixture, &empty.revision_id, "operator-empty")?,
                )
                .await
                .map_err(failure)?,
        )?;
        active_is(&fixture, &empty.revision_id).await
    }
    .await;
    finish(fixture, checked).await
}

#[tokio::test]
async fn restricted_scope_cannot_activate_changed_tenant_limits() -> Result<()> {
    for learning in [false, true] {
        if learning && !cfg!(feature = "core") {
            continue;
        }
        let (fixture, route, slot) = provision(false).await?;
        let checked = async {
            let mut document = fixture.configuration_input("document")?.document;
            document.routes.push(route.clone());
            let initial = draft(&fixture, document.clone(), "limits-initial").await?;
            result(
                fixture
                    .service
                    .execute(
                        fixture.operator.clone(),
                        activation(&fixture, &initial.revision_id, "limits-active")?,
                    )
                    .await
                    .map_err(failure)?,
            )?;
            if learning {
                document.learning.max_evaluations = 1;
            } else {
                document.limits.active_executions = 0;
            }
            let candidate = draft(&fixture, document, "limits-candidate").await?;
            let scope = GrantScope::Resources {
                resources: vec![route.key(), slot.key()],
            };
            binding(
                &fixture,
                "limits-narrow",
                scope.clone(),
                TenantRole::TenantAdmin,
                expiry()?,
            )
            .await?;
            let credential = issue_scoped(&fixture, scope, "limits-key").await?;
            let before = fixture.service.current().map_err(failure)?;
            denied(
                &fixture,
                caller(&fixture, &credential)?,
                activation(&fixture, &candidate.revision_id, "forbidden-limits")?,
                ErrorCode::Forbidden,
            )
            .await?;
            let after = fixture.service.current().map_err(failure)?;
            ensure!(
                after.revision == before.revision && after.operation_ids == before.operation_ids
            );
            active_is(&fixture, &initial.revision_id).await?;
            result(
                fixture
                    .service
                    .execute(
                        fixture.operator.clone(),
                        activation(&fixture, &candidate.revision_id, "operator-limits")?,
                    )
                    .await
                    .map_err(failure)?,
            )?;
            active_is(&fixture, &candidate.revision_id).await
        }
        .await;
        finish(fixture, checked).await?;
    }
    Ok(())
}

#[tokio::test]
async fn activation_requires_pool_intersection_in_credential_and_binding() -> Result<()> {
    let (fixture, route, slot) = provision(true).await?;
    let checked = async {
        let mut document = fixture.configuration_input("document")?.document;
        document.routes.push(route.clone());
        let retained = draft(&fixture, document, "pool-candidate").await?;
        let pool = allowed(&route, &slot);
        binding(
            &fixture,
            "grant-current-pool",
            pool.clone(),
            TenantRole::TenantAdmin,
            expiry()?,
        )
        .await?;
        let without = issue_scoped(
            &fixture,
            crate::tenancy::mutation::tenant_scope(),
            "credential-no-pool",
        )
        .await?;
        let with = issue_scoped(&fixture, pool.clone(), "credential-with-pool").await?;
        let before = fixture.service.current().map_err(failure)?;
        denied(
            &fixture,
            caller(&fixture, &without)?,
            activation(&fixture, &retained.revision_id, "missing-credential-pool")?,
            ErrorCode::Forbidden,
        )
        .await?;
        ensure!(fixture.service.current().map_err(failure)?.revision == before.revision);
        binding(
            &fixture,
            "remove-current-pool",
            crate::tenancy::mutation::tenant_scope(),
            TenantRole::TenantAdmin,
            expiry()?,
        )
        .await?;
        let before = fixture.service.current().map_err(failure)?;
        denied(
            &fixture,
            caller(&fixture, &with)?,
            activation(&fixture, &retained.revision_id, "missing-binding-pool")?,
            ErrorCode::Forbidden,
        )
        .await?;
        let after = fixture.service.current().map_err(failure)?;
        ensure!(after.revision == before.revision && after.operation_ids == before.operation_ids);
        ensure!(
            fixture
                .service
                .active_revision(&fixture.operator, &fixture.tenant)
                .map_err(failure)?
                .revision_id
                .is_none()
        );
        binding(
            &fixture,
            "restore-current-pool",
            pool,
            TenantRole::TenantAdmin,
            expiry()?,
        )
        .await?;
        result(
            fixture
                .service
                .execute(
                    caller(&fixture, &with)?,
                    activation(&fixture, &retained.revision_id, "both-pool-grants")?,
                )
                .await
                .map_err(failure)?,
        )?;
        active_is(&fixture, &retained.revision_id).await
    }
    .await;
    finish(fixture, checked).await
}
