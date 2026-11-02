use super::*;
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKey, ResourceKind},
    configuration::ResourceRef,
    resources::{Definition, ResourcePut, ResourceRevoke},
};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use std::sync::Arc;

async fn keyed_fixture() -> Result<Fixture> {
    let mut fixture = Fixture::new().await?;
    restart(&mut fixture).await?;
    Ok(fixture)
}

async fn restart(fixture: &mut Fixture) -> Result<()> {
    fixture.service.shutdown().await?;
    let initial = super::super::store::load(&fixture.db, Utc::now()).await?;
    fixture.service = Service::start_resources(
        fixture.db.clone(),
        None,
        Default::default(),
        initial,
        Arc::new(super::super::resources::Resources::with_key(
            fixture.installation.clone(),
            [43; 32],
        )?),
        None,
        None,
    )
    .await?;
    Ok(())
}

fn credential(fixture: &Fixture, operation: &str) -> Result<ResourcePut> {
    Ok(ResourcePut {
        format: 1,
        operation_id: operation.try_into()?,
        installation_id: fixture.installation.clone(),
        key: ResourceKey {
            owner_kind: OwnerKind::Tenant,
            owner_id: fixture.tenant.as_str().try_into()?,
            resource_kind: ResourceKind::Credential,
            resource_id: "slot".try_into()?,
        },
        expected_revision: None,
        definition: Definition::Credential {
            secret: "WRITE_ONLY_PROVIDER_SECRET".into(),
        },
    })
}

async fn put(fixture: &Fixture, caller: Caller, input: ResourcePut) -> Result<OperationResult> {
    result(
        fixture
            .service
            .execute(caller, Command::Resource(fixture.tenant.clone(), input))
            .await
            .map_err(failure)?,
    )
}

fn reference(input: &ResourcePut, result: &OperationResult) -> Result<ResourceRef> {
    Ok(ResourceRef {
        owner_kind: input.key.owner_kind,
        owner_id: input.key.owner_id.clone(),
        resource_kind: input.key.resource_kind,
        resource_id: input.key.resource_id.clone(),
        revision_id: result
            .resource_revision
            .clone()
            .context("applied revision")?,
    })
}

#[tokio::test]
async fn dynamic_credentials_rotate_replay_and_revoke_across_restart_without_plaintext()
-> Result<()> {
    let mut fixture = keyed_fixture().await?;
    let input = credential(&fixture, "create-slot")?;
    let created = put(&fixture, fixture.operator.clone(), input.clone()).await?;
    let original = reference(&input, &created)?;
    let replay = put(&fixture, fixture.operator.clone(), input.clone()).await?;
    ensure!(created.resource_revision == replay.resource_revision);
    let mut conflict = input.clone();
    conflict.definition = Definition::Credential {
        secret: "different-secret".into(),
    };
    let rejected = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Resource(fixture.tenant.clone(), conflict),
        )
        .await;
    ensure!(matches!(rejected, Err(error) if error.0.code == ErrorCode::OperationConflict));

    let mut rotate = input.clone();
    rotate.operation_id = "rotate-slot".try_into()?;
    rotate.expected_revision = Some(original.revision_id.clone());
    rotate.definition = Definition::Credential {
        secret: "ROTATED_WRITE_ONLY_SECRET".into(),
    };
    let rotated = put(&fixture, fixture.operator.clone(), rotate.clone()).await?;
    let current = reference(&rotate, &rotated)?;
    ensure!(current != original);
    let status = fixture
        .service
        .resource_status(&fixture.operator, &fixture.tenant, &original)
        .await
        .map_err(failure)?;
    ensure!(!status.revoked && status.head_revision.as_ref() == Some(&current.revision_id));

    let revoke = ResourceRevoke {
        format: 1,
        operation_id: "revoke-original".try_into()?,
        installation_id: fixture.installation.clone(),
        reference: original.clone(),
        expected_authority_revision: status.authority_revision,
    };
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::RevokeResource(fixture.tenant.clone(), revoke.clone()),
            )
            .await
            .map_err(failure)?,
    )?;
    for table in ["tenancy_resource_revisions", "tenancy_operations"] {
        let column = if table == "tenancy_operations" {
            "response_json"
        } else {
            "payload_json"
        };
        for row in fixture
            .db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!("SELECT {column} AS payload FROM {table}"),
            ))
            .await?
        {
            let payload: String = row.try_get("", "payload")?;
            ensure!(!payload.contains("WRITE_ONLY") && !payload.contains("ROTATED"));
        }
    }
    restart(&mut fixture).await?;
    let status = fixture
        .service
        .resource_status(&fixture.operator, &fixture.tenant, &original)
        .await
        .map_err(failure)?;
    ensure!(status.revoked && status.head_revision.as_ref() == Some(&current.revision_id));
    let replay = put(&fixture, fixture.operator.clone(), rotate).await?;
    ensure!(replay.resource_revision == rotated.resource_revision);
    let replay = result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::RevokeResource(fixture.tenant.clone(), revoke),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(replay.resource_revision.as_ref() == Some(&original.revision_id));
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn resource_cas_has_one_winner_and_tenant_cannot_mutate_foreign_or_pool_keys() -> Result<()> {
    let fixture = keyed_fixture().await?;
    let first = credential(&fixture, "contender-a")?;
    let second = credential(&fixture, "contender-b")?;
    let (a, b) = tokio::join!(
        fixture.service.execute(
            fixture.operator.clone(),
            Command::Resource(fixture.tenant.clone(), first)
        ),
        fixture.service.execute(
            fixture.operator.clone(),
            Command::Resource(fixture.tenant.clone(), second)
        )
    );
    let successful = |result: &std::result::Result<
        StoredResponse,
        super::super::error::Failure,
    >| matches!(result, Ok(StoredResponse::Operation(result)) if result.status == bitrouter_ai::tenancy::operation::OperationStatus::Applied);
    ensure!(usize::from(successful(&a)) + usize::from(successful(&b)) == 1);
    for result in [a, b] {
        if !successful(&result) {
            ensure!(
                matches!(result, Ok(StoredResponse::Failure(error)) if error.code == ErrorCode::RevisionConflict)
            );
        }
    }
    let issued = fixture.issue("tenant-access").await?;
    let caller = fixture
        .service
        .authenticate(issued.secret.as_deref().context("issued secret")?)
        .map_err(failure)?;
    for (kind, owner) in [
        (OwnerKind::Tenant, "another-tenant"),
        (OwnerKind::OperatorPool, "pool"),
    ] {
        let mut input = credential(&fixture, "foreign-resource")?;
        input.key.owner_kind = kind;
        input.key.owner_id = owner.try_into()?;
        let result = fixture
            .service
            .execute(
                caller.clone(),
                Command::Resource(fixture.tenant.clone(), input),
            )
            .await;
        ensure!(matches!(result, Err(error) if error.0.code == ErrorCode::Forbidden));
    }
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn dynamic_routes_compile_owned_credentials_and_require_operator_approved_origins()
-> Result<()> {
    let fixture = keyed_fixture().await?;
    let input = credential(&fixture, "create-provider-slot")?;
    let created = put(&fixture, fixture.operator.clone(), input.clone()).await?;
    let slot = reference(&input, &created)?;
    let issued = fixture.issue("tenant-route-editor").await?;
    let caller = fixture
        .service
        .authenticate(issued.secret.as_deref().context("issued secret")?)
        .map_err(failure)?;
    let mut route = input;
    route.operation_id = "create-route".try_into()?;
    route.key.resource_kind = ResourceKind::Route;
    route.key.resource_id = "public-route".try_into()?;
    route.definition = Definition::Route {
        routing: serde_json::json!({"providers":{"mock":{"api_base":"https://example.invalid/v1", "models":[{"id":"model-a"}]}}}),
        selector: "mock:model-a".into(),
        policy: None,
        credentials: vec![bitrouter_ai::tenancy::resources::CredentialBinding {
            provider: "mock".into(),
            account: "default".into(),
            credential: slot.clone(),
        }],
    };
    let denied = fixture
        .service
        .execute(
            caller,
            Command::Resource(fixture.tenant.clone(), route.clone()),
        )
        .await;
    ensure!(matches!(denied, Err(error) if error.0.code == ErrorCode::Forbidden));
    let created = put(&fixture, fixture.operator.clone(), route.clone()).await?;
    let route_ref = reference(&route, &created)?;
    let mut document = fixture.configuration_input("dynamic-config")?;
    document.document.routes.push(route_ref);
    fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(fixture.tenant.clone(), document),
        )
        .await
        .map_err(failure)?;
    let revoke = ResourceRevoke {
        format: 1,
        operation_id: "revoke-slot".try_into()?,
        installation_id: fixture.installation.clone(),
        reference: slot,
        expected_authority_revision: fixture.service.current().map_err(failure)?.revision,
    };
    fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::RevokeResource(fixture.tenant.clone(), revoke),
        )
        .await
        .map_err(failure)?;
    let mut document = fixture.configuration_input("revoked-config")?;
    document.document.routes.push(reference(&route, &created)?);
    let denied = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(fixture.tenant.clone(), document),
        )
        .await;
    ensure!(matches!(denied, Err(error) if error.0.code == ErrorCode::Forbidden));
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn resource_mutations_and_reads_recheck_narrowed_permissions_and_suspension() -> Result<()> {
    let fixture = keyed_fixture().await?;
    let input = credential(&fixture, "managed-slot")?;
    let created = put(&fixture, fixture.operator.clone(), input.clone()).await?;
    let slot = reference(&input, &created)?;
    let mut issuance = fixture.issuance("editor-key")?;
    issuance.permissions = TenantRole::Editor.permissions().to_vec();
    let key = issued(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(fixture.tenant.clone(), issuance),
            )
            .await
            .map_err(failure)?,
    )?;
    let caller = fixture
        .service
        .authenticate(key.secret.as_deref().context("key secret")?)
        .map_err(failure)?;
    let mut rotate = input.clone();
    rotate.operation_id = "editor-cannot-rotate".try_into()?;
    rotate.expected_revision = Some(slot.revision_id.clone());
    ensure!(
        matches!(fixture.service.execute(caller.clone(), Command::Resource(fixture.tenant.clone(), rotate)).await, Err(error) if error.0.code == ErrorCode::Forbidden)
    );
    fixture
        .service
        .resource_status(&caller, &fixture.tenant, &slot)
        .await
        .map_err(failure)?;
    let narrowing = BindingPut {
        operation_id: "narrow-resource-reader".try_into()?,
        expected_revision: fixture.binding_result.resource_revision.clone(),
        principal_id: fixture.principal.clone(),
        role: TenantRole::Viewer,
        scope: GrantScope::Resources {
            resources: vec![ResourceKey {
                resource_id: "another-slot".try_into()?,
                ..input.key.clone()
            }],
        },
        expires_at: expiry()?,
        expected_management_owner: Some(match &fixture.operator {
            Caller::Installation(id) => id.clone(),
            _ => anyhow::bail!("operator expected"),
        }),
        installation_id: fixture.installation.clone(),
    };
    fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Binding(fixture.tenant.clone(), fixture.binding.clone(), narrowing),
        )
        .await
        .map_err(failure)?;
    ensure!(
        matches!(fixture.service.resource_status(&caller, &fixture.tenant, &slot).await, Err(error) if error.0.code == ErrorCode::Forbidden)
    );
    let state = fixture.service.current().map_err(failure)?;
    let tenant = state.tenants.get(&fixture.tenant).context("tenant state")?;
    fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Tenant(
                fixture.tenant.clone(),
                TenantPut {
                    operation_id: "suspend-resource-owner".try_into()?,
                    installation_id: fixture.installation.clone(),
                    expected_revision: Some(tenant.revision.clone()),
                    state: TenantState::Suspended,
                },
            ),
        )
        .await
        .map_err(failure)?;
    let mut input = input;
    input.operation_id = "suspended-resource-write".try_into()?;
    input.key.resource_id = "new-slot".try_into()?;
    ensure!(
        matches!(fixture.service.execute(fixture.operator.clone(), Command::Resource(fixture.tenant.clone(), input)).await, Err(error) if error.0.code == ErrorCode::TenantInactive)
    );
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn resource_storage_fault_rolls_back_payload_lifecycle_and_operation_together() -> Result<()>
{
    let mut fixture = keyed_fixture().await?;
    let input = credential(&fixture, "atomic-resource")?;
    fixture.db.execute(Statement::from_string(DatabaseBackend::Sqlite,
        "CREATE TRIGGER fail_lifecycle BEFORE INSERT ON tenancy_resource_lifecycle BEGIN SELECT RAISE(ABORT, 'injected resource failure'); END".to_owned())).await?;
    let failed = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Resource(fixture.tenant.clone(), input.clone()),
        )
        .await;
    ensure!(failed.is_err());
    ensure!(fixture.service.current().is_err());
    for table in ["tenancy_resource_revisions", "tenancy_resource_lifecycle"] {
        let row = fixture
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!("SELECT count(*) AS n FROM {table}"),
            ))
            .await?
            .context("count")?;
        ensure!(row.try_get::<i64>("", "n")? == 0);
    }
    let Caller::Installation(principal) = &fixture.operator else {
        anyhow::bail!("operator expected")
    };
    ensure!(
        super::super::store::operation(
            &fixture.db,
            &fixture.installation,
            principal,
            &input.operation_id
        )
        .await?
        .is_none()
    );
    fixture
        .db
        .execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "DROP TRIGGER fail_lifecycle".to_owned(),
        ))
        .await?;
    restart(&mut fixture).await?;
    let created = put(&fixture, fixture.operator.clone(), input.clone()).await?;
    let replay = put(&fixture, fixture.operator.clone(), input).await?;
    ensure!(replay.resource_revision == created.resource_revision);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn lost_resource_reply_still_commits_once_during_shutdown() -> Result<()> {
    let mut fixture = keyed_fixture().await?;
    let input = credential(&fixture, "resource-lost-reply")?;
    let (reply, receive) = tokio::sync::oneshot::channel();
    fixture
        .service
        .commands
        .try_send(
            super::super::actor::Message {
                caller: fixture.operator.clone(),
                command: Command::Resource(fixture.tenant.clone(), input.clone()),
                reply,
            }
            .into(),
        )
        .map_err(|_| anyhow::anyhow!("admission"))?;
    drop(receive);
    fixture.service.shutdown().await?;
    restart(&mut fixture).await?;
    let recorded = fixture
        .service
        .operation(&fixture.operator, &input.operation_id)
        .await
        .map_err(failure)?;
    ensure!(recorded.status == bitrouter_ai::tenancy::operation::OperationStatus::Applied);
    let result = put(&fixture, fixture.operator.clone(), input.clone()).await?;
    ensure!(recorded.resource_revision == result.resource_revision);
    let row = fixture
        .db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT count(*) AS n FROM tenancy_resource_revisions".to_owned(),
        ))
        .await?
        .context("count")?;
    ensure!(row.try_get::<i64>("", "n")? == 1);
    let status = fixture
        .service
        .resource_status(
            &fixture.operator,
            &fixture.tenant,
            &reference(&input, &result)?,
        )
        .await
        .map_err(failure)?;
    ensure!(!status.revoked);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn rejected_resource_cas_is_durable_and_cannot_be_rebound_after_restart() -> Result<()> {
    let mut fixture = keyed_fixture().await?;
    let first = credential(&fixture, "first-slot")?;
    let created = put(&fixture, fixture.operator.clone(), first).await?;
    let stale = credential(&fixture, "stale-slot")?;
    let rejected = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Resource(fixture.tenant.clone(), stale.clone()),
        )
        .await
        .map_err(failure)?;
    ensure!(
        matches!(rejected, StoredResponse::Failure(error) if error.code == ErrorCode::RevisionConflict)
    );
    let recorded = fixture
        .service
        .operation(&fixture.operator, &stale.operation_id)
        .await
        .map_err(failure)?;
    ensure!(recorded.status == bitrouter_ai::tenancy::operation::OperationStatus::Rejected);
    ensure!(
        recorded
            .error
            .is_some_and(|error| error.code == ErrorCode::RevisionConflict)
    );
    restart(&mut fixture).await?;
    let replay = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Resource(fixture.tenant.clone(), stale.clone()),
        )
        .await
        .map_err(failure)?;
    ensure!(
        matches!(replay, StoredResponse::Failure(error) if error.code == ErrorCode::RevisionConflict)
    );
    let mut changed = stale;
    changed.expected_revision = created.resource_revision;
    changed.definition = Definition::Credential {
        secret: "changed-rejected-secret".into(),
    };
    ensure!(
        matches!(fixture.service.execute(fixture.operator.clone(), Command::Resource(fixture.tenant.clone(), changed)).await, Err(error) if error.0.code == ErrorCode::OperationConflict)
    );
    fixture.service.shutdown().await?;
    Ok(())
}
