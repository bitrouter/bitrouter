use super::*;

async fn healthy_credential(fixture: &Fixture) -> Result<CredentialIssued> {
    let tenant: TenantId = "healthy-tenant".try_into()?;
    let principal: PrincipalId = "healthy-principal".try_into()?;
    let binding: BindingId = "healthy-binding".try_into()?;
    let installation = fixture.installation.clone();
    let expires = expiry()?;
    for command in [
        Command::Tenant(
            tenant.clone(),
            TenantPut {
                operation_id: "healthy-tenant-put".try_into()?,
                expected_revision: None,
                state: TenantState::Active,
                installation_id: installation.clone(),
            },
        ),
        Command::Principal(
            principal.clone(),
            PrincipalPut {
                operation_id: "healthy-principal-put".try_into()?,
                expected_revision: None,
                issuer: "startup-test".into(),
                subject: "healthy".into(),
                installation_id: installation.clone(),
            },
        ),
        Command::Binding(
            tenant.clone(),
            binding.clone(),
            BindingPut {
                operation_id: "healthy-binding-put".try_into()?,
                expected_revision: None,
                principal_id: principal.clone(),
                role: TenantRole::TenantAdmin,
                scope: crate::tenancy::mutation::tenant_scope(),
                expires_at: expires.clone(),
                expected_management_owner: None,
                installation_id: installation.clone(),
            },
        ),
    ] {
        let applied = result(
            fixture
                .service
                .execute(fixture.operator.clone(), command)
                .await
                .map_err(failure)?,
        )?;
        ensure!(
            applied.status == bitrouter_ai::tenancy::operation::OperationStatus::Applied,
            "healthy authority setup rejected: {:?}",
            applied.error
        );
    }
    issued(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(
                    tenant,
                    CredentialCreate {
                        operation_id: "healthy-credential".try_into()?,
                        expected_authority_revision: fixture
                            .service
                            .current()
                            .map_err(failure)?
                            .revision,
                        principal_id: principal,
                        permissions: TenantRole::TenantAdmin.permissions().to_vec(),
                        scope: crate::tenancy::mutation::tenant_scope(),
                        expires_at: expires,
                        rotate_credential_id: None,
                        binding_id: binding,
                        installation_id: installation,
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )
    .context("healthy credential issuance")
}

async fn rejects_scoped_read(change: &str, orphan: bool) -> Result<()> {
    let fixture = Fixture::new().await?;
    let broken = fixture.issue("corrupt-credential").await?;
    let healthy = healthy_credential(&fixture).await?;
    fixture.service.shutdown().await?;
    let changed = if orphan {
        // Deliberately damage only this stopped fixture. PRAGMA is connection
        // local, so retain one physical connection through disable/mutate/restore.
        let mut connection = fixture.db.get_sqlite_connection_pool().acquire().await?;
        sea_orm::sqlx::query("PRAGMA foreign_keys=OFF")
            .execute(&mut *connection)
            .await?;
        let changed = sea_orm::sqlx::query(change)
            .bind(broken.credential_id.as_str())
            .execute(&mut *connection)
            .await;
        // Restore FK enforcement even when the injected mutation fails.
        sea_orm::sqlx::query("PRAGMA foreign_keys=ON")
            .execute(&mut *connection)
            .await?;
        let enabled: i64 = sea_orm::sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&mut *connection)
            .await?;
        ensure!(
            enabled == 1,
            "fixture did not restore foreign-key enforcement"
        );
        changed?.rows_affected()
    } else {
        fixture
            .db
            .execute(Statement::from_sql_and_values(
                fixture.db.get_database_backend(),
                change,
                [broken.credential_id.as_str().into()],
            ))
            .await?
            .rows_affected()
    };
    ensure!(
        changed == 1,
        "corruption fixture did not change exactly one row"
    );
    let service = Service::open(fixture.db.clone(), None).await?;
    let outcome = async {
        ensure!(service.current().is_ok() && service.admission.receipts_ready());
        let authenticated = service.authenticate_live(healthy.secret.as_deref().context("healthy secret")?).await.map_err(failure)?;
        service.authority.read().map_err(|code| failure(code.into()))?.check(&authenticated.lease, |view| {
            ensure_authorized(view.facts(), &authenticated.caller, &"healthy-tenant".try_into().map_err(|_| ErrorCode::InvalidRequest)?)
        }).map_err(|code| failure(code.into()))?;
        // Address the credential ID as well as its tenant. A damaged key hash
        // must reach the decoder instead of merely missing the token index.
        let denied = service.read_receipt(Caller::Credential(broken.credential_id.clone()),
            fixture.tenant.clone(), crate::tenancy::receipts::read::Request::One("absent-receipt".try_into()?)
        ).await;
        ensure!(matches!(denied, Err(error) if matches!(error.0.code,
            ErrorCode::StorageUnavailable | ErrorCode::AuthorityUnavailable | ErrorCode::Unauthorized)),
            "malformed authorization was accepted: {change}");
        ensure!(service.authenticate_live(broken.secret.as_deref().context("broken secret")?).await.is_err(), "corrupt credential authenticated");
        let healthy_caller = service.authenticate_with_reuse(healthy.secret.as_deref().context("healthy secret")?, false).await.map_err(failure)?;
        service.authority.read().map_err(|code| failure(code.into()))?.check(&healthy_caller.lease, |view| {
            ensure_authorized(view.facts(), &healthy_caller.caller, &"healthy-tenant".try_into().map_err(|_| ErrorCode::InvalidRequest)?)
        }).map_err(|code| failure(code.into()))?;
        ensure!(service.current().is_ok() && service.admission.receipts_ready(), "scoped corruption fenced unrelated traffic");
        Ok(())
    }.await;
    let stopped = service.shutdown().await;
    outcome
        .and(stopped)
        .with_context(|| format!("scoped authority corruption: {change}"))
}

#[tokio::test]
async fn malformed_authority_fields_fail_when_the_credential_is_read() -> Result<()> {
    for change in [
        "UPDATE tenancy_credentials SET created_at='not-a-timestamp' WHERE id=?",
        "UPDATE tenancy_credentials SET key_hash='invalid-digest' WHERE id=?",
        "UPDATE tenancy_credentials SET permissions_json='[\"execution.submit\",\"execution.submit\"]' WHERE id=?",
        "UPDATE tenancy_credentials SET permissions_json='[]' WHERE id=?",
        "UPDATE tenancy_credentials SET audit_reader=1 WHERE id=?",
        "UPDATE tenancy_tenants SET created_at='not-a-timestamp' WHERE id=(SELECT tenant_id FROM tenancy_credentials WHERE id=?)",
        "UPDATE tenancy_principals SET created_at='not-a-timestamp' WHERE id=(SELECT principal_id FROM tenancy_credentials WHERE id=?)",
        "UPDATE tenancy_principals SET issuer='' WHERE id=(SELECT principal_id FROM tenancy_credentials WHERE id=?)",
        "UPDATE tenancy_principals SET subject='' WHERE id=(SELECT principal_id FROM tenancy_credentials WHERE id=?)",
    ] {
        rejects_scoped_read(change, false).await?;
    }
    Ok(())
}

#[tokio::test]
async fn orphaned_authority_links_fail_when_the_credential_is_read() -> Result<()> {
    for change in [
        "DELETE FROM tenancy_principals WHERE id=(SELECT principal_id FROM tenancy_credentials WHERE id=?)",
        "DELETE FROM tenancy_tenants WHERE id=(SELECT tenant_id FROM tenancy_credentials WHERE id=?)",
        "DELETE FROM tenancy_bindings WHERE id=(SELECT binding_id FROM tenancy_credentials WHERE id=?)",
        "UPDATE tenancy_bindings SET principal_id='missing-principal' WHERE id=(SELECT binding_id FROM tenancy_credentials WHERE id=?)",
        "UPDATE tenancy_bindings SET management_owner='missing-manager',management_class='tenant' WHERE id=(SELECT binding_id FROM tenancy_credentials WHERE id=?)",
    ] {
        rejects_scoped_read(change, true).await?;
    }
    Ok(())
}
