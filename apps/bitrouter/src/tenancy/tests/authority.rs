use super::*;

#[tokio::test]
async fn operator_token_rotation_and_restart_preserve_managed_identity() -> Result<()> {
    use bitrouter_ai::control::ControlScope;
    let fixture = Fixture::new().await?;
    let caller = |id: &str, token: &str| -> Result<Caller> {
        let auth = crate::remote_control::auth::ControlAuth::from_lookup(
            &crate::config::ControlConfig {
                credentials: vec![crate::config::ControlCredentialConfig {
                    id: id.into(),
                    token_env: "ROTATED_OPERATOR_TOKEN".into(),
                    scopes: vec![ControlScope::Tenancy],
                }],
                ..Default::default()
            },
            |_| Some(token.into()),
        )?;
        let authenticated = auth
            .authenticate(token.as_bytes())
            .context("operator rejected")?;
        Ok(Caller::Installation(PrincipalId::for_control_credential(
            authenticated.id(),
        )?))
    };
    let operation = "create-binding".try_into()?;
    let old = fixture
        .service
        .operation(&caller("test-operator", &"a".repeat(32))?, &operation)
        .await
        .map_err(failure)?;
    fixture.service.shutdown().await?;
    let restarted = Service::open(fixture.db.clone(), None).await?;
    let rotated = caller("test-operator", &"b".repeat(32))?;
    let recovered = restarted
        .operation(&rotated, &operation)
        .await
        .map_err(failure)?;
    ensure!(recovered.resource_revision == old.resource_revision);
    let renamed = caller("renamed-operator", &"c".repeat(32))?;
    ensure!(
        matches!(restarted.operation(&renamed, &operation).await, Err(error) if error.0.code == ErrorCode::NotFound)
    );
    let mut update = BindingPut {
        operation_id: "renamed-update".try_into()?,
        expected_revision: old.resource_revision,
        principal_id: fixture.principal.clone(),
        role: TenantRole::Viewer,
        scope: super::super::mutation::tenant_scope(),
        expires_at: expiry()?,
        expected_management_owner: Some(PrincipalId::for_control_credential("test-operator")?),
        installation_id: fixture.installation.clone(),
    };
    ensure!(
        matches!(restarted.execute(renamed, Command::Binding(fixture.tenant.clone(), fixture.binding.clone(), update.clone())).await, Ok(StoredResponse::Failure(error)) if error.code == ErrorCode::Forbidden)
    );
    update.operation_id = "rotated-update".try_into()?;
    result(
        restarted
            .execute(
                rotated,
                Command::Binding(fixture.tenant, fixture.binding, update),
            )
            .await
            .map_err(failure)?,
    )?;
    restarted.shutdown().await?;
    Ok(())
}
use bitrouter_ai::tenancy::authority::{CredentialState, OwnerKind, ResourceKey, ResourceKind};

#[tokio::test]
async fn rotation_cannot_broaden_and_publishes_replacement_with_revocation() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut initial = fixture.issuance("limited-key")?;
    initial.permissions = vec![Permission::ResourceRead];
    let original = issued(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(fixture.tenant.clone(), initial.clone()),
            )
            .await
            .map_err(failure)?,
    )?;
    let original_secret = original.secret.as_deref().context("first secret missing")?;
    let mut rotation = fixture.issuance("broaden-rotation")?;
    rotation.rotate_credential_id = Some(original.credential_id.clone());
    let rejected = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Issue(fixture.tenant.clone(), rotation.clone()),
        )
        .await
        .map_err(failure)?;
    ensure!(
        matches!(rejected, StoredResponse::Failure(error) if error.code == ErrorCode::Forbidden)
    );
    ensure!(fixture.service.authenticate(original_secret).is_ok());
    rotation.operation_id = "bounded-rotation".try_into()?;
    rotation.permissions = initial.permissions;
    let replacement = issued(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(fixture.tenant.clone(), rotation.clone()),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(fixture.service.authenticate(original_secret).is_err());
    ensure!(
        fixture
            .service
            .authenticate(
                replacement
                    .secret
                    .as_deref()
                    .context("replacement secret missing")?
            )
            .is_ok()
    );
    let old = fixture
        .service
        .credential(&fixture.operator, &fixture.tenant, &original.credential_id)
        .await
        .map_err(failure)?;
    ensure!(old.state == CredentialState::Revoked);
    let replay = issued(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(fixture.tenant.clone(), rotation),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(replay.secret.is_none());
    ensure!(replay.credential_id == replacement.credential_id);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn pool_authority_requires_a_live_installation_managed_binding() -> Result<()> {
    let fixture = Fixture::new().await?;
    let pool = ResourceKey {
        owner_kind: OwnerKind::OperatorPool,
        owner_id: "shared-pool".try_into()?,
        resource_kind: ResourceKind::Route,
        resource_id: "shared-route".try_into()?,
    };
    let scope = GrantScope::Tenant {
        operator_pools: vec![pool.clone()],
    };
    let Caller::Installation(owner) = &fixture.operator else {
        anyhow::bail!("installation operator missing");
    };
    let mut binding = BindingPut {
        operation_id: "allow-pool".try_into()?,
        expected_revision: fixture.binding_result.resource_revision.clone(),
        principal_id: fixture.principal.clone(),
        role: TenantRole::TenantAdmin,
        scope: scope.clone(),
        expires_at: expiry()?,
        expected_management_owner: Some(owner.clone()),
        installation_id: fixture.installation.clone(),
    };
    let updated = result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Binding(
                    fixture.tenant.clone(),
                    fixture.binding.clone(),
                    binding.clone(),
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let mut grant = fixture.issuance("pool-key")?;
    grant.scope = scope.clone();
    let credential = issued(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(fixture.tenant.clone(), grant),
            )
            .await
            .map_err(failure)?,
    )?;
    let caller = fixture
        .service
        .authenticate(credential.secret.as_deref().context("secret missing")?)
        .map_err(failure)?;
    let requested = GrantScope::Resources {
        resources: vec![pool],
    };
    ensure!(
        fixture
            .service
            .current()
            .map_err(failure)?
            .authorize(
                &caller,
                &fixture.tenant,
                Permission::ResourceRead,
                &requested,
                Utc::now(),
            )
            .is_ok()
    );
    let detached = result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Detach(
                    fixture.tenant.clone(),
                    fixture.binding.clone(),
                    Mutation {
                        operation_id: "detach-pool-manager".try_into()?,
                        expected_revision: updated
                            .resource_revision
                            .context("binding revision missing")?,
                        installation_id: fixture.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(
        fixture
            .service
            .current()
            .map_err(failure)?
            .authorize(
                &caller,
                &fixture.tenant,
                Permission::ResourceRead,
                &requested,
                Utc::now(),
            )
            .is_err()
    );
    binding.operation_id = "stale-reconciler".try_into()?;
    binding.expected_revision = detached.resource_revision.clone();
    let stale = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Binding(
                fixture.tenant.clone(),
                fixture.binding.clone(),
                binding.clone(),
            ),
        )
        .await
        .map_err(failure)?;
    ensure!(
        matches!(stale, StoredResponse::Failure(error) if error.code == ErrorCode::RevisionConflict)
    );
    binding.operation_id = "manual-unmanaged-edit".try_into()?;
    binding.expected_management_owner = None;
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Binding(fixture.tenant.clone(), fixture.binding.clone(), binding),
            )
            .await
            .map_err(failure)?,
    )?;
    let state = fixture.service.current().map_err(failure)?;
    ensure!(
        state
            .bindings
            .get(&(fixture.tenant.clone(), fixture.binding.clone()))
            .context("binding missing")?
            .managed_by
            .is_none()
    );
    ensure!(
        state
            .authorize(
                &caller,
                &fixture.tenant,
                Permission::ResourceRead,
                &requested,
                Utc::now()
            )
            .is_err()
    );
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn delegated_binding_expiry_cannot_outlive_the_issuing_credential() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut grant = fixture.issuance("short-lived-admin")?;
    grant.expires_at = (Utc::now() + chrono::Duration::hours(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        .try_into()?;
    let credential = issued(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(fixture.tenant.clone(), grant),
            )
            .await
            .map_err(failure)?,
    )?;
    let caller = fixture
        .service
        .authenticate(credential.secret.as_deref().context("secret missing")?)
        .map_err(failure)?;
    let mut binding = BindingPut {
        operation_id: "excessive-expiry".try_into()?,
        expected_revision: None,
        principal_id: fixture.principal.clone(),
        role: TenantRole::Viewer,
        scope: super::super::mutation::tenant_scope(),
        expires_at: expiry()?,
        expected_management_owner: None,
        installation_id: fixture.installation.clone(),
    };
    let denied = fixture
        .service
        .execute(
            caller.clone(),
            Command::Binding(
                fixture.tenant.clone(),
                "delegated".try_into()?,
                binding.clone(),
            ),
        )
        .await;
    ensure!(matches!(denied, Err(error) if error.0.code == ErrorCode::Forbidden));
    binding.operation_id = "bounded-expiry".try_into()?;
    binding.expires_at = credential.expires_at;
    result(
        fixture
            .service
            .execute(
                caller,
                Command::Binding(fixture.tenant.clone(), "delegated".try_into()?, binding),
            )
            .await
            .map_err(failure)?,
    )?;
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn saved_result_access_tracks_current_scope_and_survives_valid_rotation() -> Result<()> {
    let fixture = Fixture::new().await?;
    let credential = fixture.issue("issuer-key").await?;
    let caller = fixture
        .service
        .authenticate(credential.secret.as_deref().context("secret missing")?)
        .map_err(failure)?;
    let scope = |id: &str| -> Result<GrantScope> {
        Ok(GrantScope::Resources {
            resources: vec![ResourceKey {
                owner_kind: OwnerKind::Tenant,
                owner_id: fixture.tenant.as_str().try_into()?,
                resource_kind: ResourceKind::Route,
                resource_id: id.try_into()?,
            }],
        })
    };
    let submitted = BindingPut {
        operation_id: "tenant-managed-result".try_into()?,
        expected_revision: None,
        principal_id: fixture.principal.clone(),
        role: TenantRole::Viewer,
        scope: scope("route-a")?,
        expires_at: credential.expires_at.clone(),
        expected_management_owner: None,
        installation_id: fixture.installation.clone(),
    };
    let target: BindingId = "delegated-viewer".try_into()?;
    let applied = result(
        fixture
            .service
            .execute(
                caller.clone(),
                Command::Binding(fixture.tenant.clone(), target.clone(), submitted.clone()),
            )
            .await
            .map_err(failure)?,
    )?;
    fixture
        .service
        .operation(&caller, &submitted.operation_id)
        .await
        .map_err(failure)?;
    let Caller::Installation(owner) = &fixture.operator else {
        anyhow::bail!("installation operator missing");
    };
    let mut reduction = BindingPut {
        operation_id: "reduce-result-reader".try_into()?,
        expected_revision: fixture.binding_result.resource_revision.clone(),
        principal_id: fixture.principal.clone(),
        role: TenantRole::TenantAdmin,
        scope: scope("route-b")?,
        expires_at: credential.expires_at.clone(),
        expected_management_owner: Some(owner.clone()),
        installation_id: fixture.installation.clone(),
    };
    let reduced = result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Binding(
                    fixture.tenant.clone(),
                    fixture.binding.clone(),
                    reduction.clone(),
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let denied = fixture
        .service
        .operation(&caller, &submitted.operation_id)
        .await;
    ensure!(matches!(denied, Err(error) if error.0.code == ErrorCode::Forbidden));
    for role in [TenantRole::Viewer, TenantRole::Editor] {
        let mut replay = submitted.clone();
        replay.role = role;
        let denied = fixture
            .service
            .execute(
                caller.clone(),
                Command::Binding(fixture.tenant.clone(), target.clone(), replay),
            )
            .await;
        ensure!(matches!(denied, Err(error) if error.0.code == ErrorCode::Forbidden));
    }
    reduction.operation_id = "restore-result-reader".try_into()?;
    reduction.expected_revision = reduced.resource_revision;
    reduction.scope = super::super::mutation::tenant_scope();
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Binding(fixture.tenant.clone(), fixture.binding.clone(), reduction),
            )
            .await
            .map_err(failure)?,
    )?;
    let mut rotation = fixture.issuance("rotate-result-reader")?;
    rotation.rotate_credential_id = Some(credential.credential_id);
    let rotated = issued(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(fixture.tenant.clone(), rotation),
            )
            .await
            .map_err(failure)?,
    )?;
    let new_caller = fixture
        .service
        .authenticate(
            rotated
                .secret
                .as_deref()
                .context("replacement secret missing")?,
        )
        .map_err(failure)?;
    let state = fixture.service.current().map_err(failure)?;
    let recovered = fixture
        .service
        .operation(&new_caller, &submitted.operation_id)
        .await
        .map_err(failure)?;
    ensure!(recovered.resource_revision == applied.resource_revision);
    let replay = result(
        fixture
            .service
            .execute(
                new_caller,
                Command::Binding(fixture.tenant.clone(), target, submitted),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(replay.resource_revision == applied.resource_revision);
    ensure!(fixture.service.current().map_err(failure)?.revision == state.revision);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn tenant_credentials_cannot_recover_installation_class_results() -> Result<()> {
    let fixture = Fixture::new().await?;
    let credential = fixture.issue("tenant-reader").await?;
    let caller = fixture
        .service
        .authenticate(credential.secret.as_deref().context("secret missing")?)
        .map_err(failure)?;
    let operation = "create-principal".try_into()?;
    fixture
        .service
        .operation(&fixture.operator, &operation)
        .await
        .map_err(failure)?;
    let denied = fixture.service.operation(&caller, &operation).await;
    ensure!(
        matches!(denied, Err(error) if matches!(error.0.code, ErrorCode::NotFound | ErrorCode::Forbidden))
    );
    let reserved = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Principal(
                PrincipalId::for_control_credential("test-operator")?,
                PrincipalPut {
                    operation_id: "cannot-downgrade-operator".try_into()?,
                    expected_revision: None,
                    issuer: "tenant-directory".into(),
                    subject: "operator-alias".into(),
                    installation_id: fixture.installation.clone(),
                },
            ),
        )
        .await;
    ensure!(
        matches!(reserved, Ok(StoredResponse::Failure(error)) if error.code == ErrorCode::InvalidRequest)
    );
    fixture.service.shutdown().await?;
    Ok(())
}
