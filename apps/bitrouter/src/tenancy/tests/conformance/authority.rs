use super::*;
use axum::{
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode},
};
use bitrouter_ai::{control::ControlScope, tenancy::operation::OperationStatus};
use tower::ServiceExt;

#[tokio::test]
async fn credential_create_expiry_respects_binding_and_issuer_ceilings() -> Result<()> {
    let fixture = Fixture::new().await?;
    let checked = async {
        let mut excessive = fixture.issuance("beyond-recipient")?;
        excessive.expires_at = (Utc::now() + chrono::Duration::days(2))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            .try_into()?;
        let before = fixture.service.current().map_err(failure)?;
        denied(
            &fixture,
            fixture.operator.clone(),
            Command::Issue(fixture.tenant.clone(), excessive),
            ErrorCode::Forbidden,
        )
        .await?;
        let after = fixture.service.current().map_err(failure)?;
        ensure!(
            after.revision == before.revision
                && after.credentials.len() == before.credentials.len()
        );

        let mut short = fixture.issuance("short-issuer")?;
        short.expires_at = (Utc::now() + chrono::Duration::hours(1))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            .try_into()?;
        let issuer = issued(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Issue(fixture.tenant.clone(), short),
                )
                .await
                .map_err(failure)?,
        )?;
        let identity = caller(&fixture, &issuer)?;
        let before = fixture.service.current().map_err(failure)?;
        let excessive = fixture.issuance("beyond-issuer")?;
        denied(
            &fixture,
            identity.clone(),
            Command::Issue(fixture.tenant.clone(), excessive),
            ErrorCode::Forbidden,
        )
        .await?;
        let after = fixture.service.current().map_err(failure)?;
        ensure!(
            after.revision == before.revision
                && after.credentials.len() == before.credentials.len()
        );
        let mut bounded = fixture.issuance("at-issuer-ceiling")?;
        bounded.expires_at = issuer.expires_at.clone();
        let child = issued(
            fixture
                .service
                .execute(identity, Command::Issue(fixture.tenant.clone(), bounded))
                .await
                .map_err(failure)?,
        )?;
        ensure!(child.expires_at == issuer.expires_at && caller(&fixture, &child).is_ok());
        Ok(())
    }
    .await;
    finish(fixture, checked).await
}

#[tokio::test]
async fn tenant_payload_replay_cannot_change_state() -> Result<()> {
    let fixture = Fixture::new().await?;
    let checked = async {
        let operation = "create-tenant".try_into()?;
        let before = fixture.service.current().map_err(failure)?;
        let original = fixture
            .service
            .operation(&fixture.operator, &operation)
            .await
            .map_err(failure)?;
        denied(
            &fixture,
            fixture.operator.clone(),
            Command::Tenant(
                fixture.tenant.clone(),
                TenantPut {
                    operation_id: operation.clone(),
                    expected_revision: None,
                    state: TenantState::Suspended,
                    installation_id: fixture.installation.clone(),
                },
            ),
            ErrorCode::OperationConflict,
        )
        .await?;
        let after = fixture.service.current().map_err(failure)?;
        let observed = fixture
            .service
            .operation(&fixture.operator, &operation)
            .await
            .map_err(failure)?;
        ensure!(after.revision == before.revision && after.operation_ids == before.operation_ids);
        ensure!(after.tenants.get(&fixture.tenant).context("tenant")?.state == TenantState::Active);
        ensure!(serde_json::to_value(observed)? == serde_json::to_value(original)?);
        Ok(())
    }
    .await;
    finish(fixture, checked).await
}

#[tokio::test]
async fn tenant_operator_cannot_mutate_lifecycle_or_principal() -> Result<()> {
    let fixture = Fixture::new().await?;
    let checked = async {
        binding(
            &fixture,
            "operator-role",
            crate::tenancy::mutation::tenant_scope(),
            TenantRole::TenantOperator,
            expiry()?,
        )
        .await?;
        let mut grant = fixture.issuance("tenant-operator")?;
        grant.permissions = TenantRole::TenantOperator.permissions().to_vec();
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
        let identity = caller(&fixture, &credential)?;
        let before = fixture.service.current().map_err(failure)?;
        let revision = before
            .tenants
            .get(&fixture.tenant)
            .context("tenant")?
            .revision
            .clone();
        for command in [
            Command::Tenant(
                fixture.tenant.clone(),
                TenantPut {
                    operation_id: "tenant-operator-stop".try_into()?,
                    expected_revision: Some(revision),
                    state: TenantState::Suspended,
                    installation_id: fixture.installation.clone(),
                },
            ),
            Command::Principal(
                "not-an-installation-principal".try_into()?,
                PrincipalPut {
                    operation_id: "tenant-operator-principal".try_into()?,
                    expected_revision: None,
                    issuer: "external".into(),
                    subject: "not-adopted".into(),
                    installation_id: fixture.installation.clone(),
                },
            ),
        ] {
            denied(&fixture, identity.clone(), command, ErrorCode::Forbidden).await?;
        }
        let after = fixture.service.current().map_err(failure)?;
        ensure!(after.revision == before.revision && after.operation_ids == before.operation_ids);
        ensure!(after.tenants.get(&fixture.tenant).context("tenant")?.state == TenantState::Active);
        ensure!(
            !after
                .principals
                .contains_key(&"not-an-installation-principal".try_into()?)
        );
        Ok(())
    }
    .await;
    finish(fixture, checked).await
}

#[tokio::test]
async fn lost_committed_http_reply_is_queryable_only_by_original_operator() -> Result<()> {
    let fixture = Fixture::new().await?;
    let checked = async {
        let token = "conformance-original-operator-token-32";
        let foreign = "conformance-other-operator-token-32";
        let auth = crate::remote_control::auth::ControlAuth::from_lookup(
            &crate::config::ControlConfig {
                credentials: vec![
                    crate::config::ControlCredentialConfig {
                        id: "test-operator".into(),
                        token_env: "ORIGINAL".into(),
                        scopes: vec![ControlScope::Tenancy],
                    },
                    crate::config::ControlCredentialConfig {
                        id: "other-operator".into(),
                        token_env: "OTHER".into(),
                        scopes: vec![ControlScope::Tenancy],
                    },
                ],
                ..Default::default()
            },
            |name| Some(if name == "ORIGINAL" { token } else { foreign }.into()),
        )?;
        let router = crate::tenancy::http::router(fixture.service.clone(), auth);
        let request = |method, path: &str, bearer: &str, body: Vec<u8>| -> Result<_> {
            Ok(Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {bearer}"))
                .header("bitrouter-installation-id", fixture.installation.as_str())
                .header("content-type", "application/json")
                .body(Body::from(body))?)
        };
        let input = PrincipalPut {
            operation_id: "lost-principal-result".try_into()?,
            expected_revision: None,
            issuer: "explicit-directory".into(),
            subject: "subject-lost".into(),
            installation_id: fixture.installation.clone(),
        };
        let body = serde_json::to_vec(&input)?;
        let response = router
            .clone()
            .oneshot(request(
                Method::PUT,
                "/control/v1/principals/lost-principal",
                token,
                body.clone(),
            )?)
            .await?;
        ensure!(response.status() == StatusCode::OK);
        // The real handler committed and formed its response. The client loses
        // that response without decoding the result; no synthetic receipt/job.
        drop(response);
        let committed = fixture.service.current().map_err(failure)?;
        let path = "/control/v1/tenancy/operations/lost-principal-result";
        let response = router
            .clone()
            .oneshot(request(Method::GET, path, token, Vec::new())?)
            .await?;
        ensure!(response.status() == StatusCode::OK);
        let recovered: OperationResult =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await?)?;
        ensure!(
            recovered.operation_id == input.operation_id
                && recovered.status == OperationStatus::Applied
        );
        ensure!(recovered.resource_id.as_ref().map(|id| id.as_str()) == Some("lost-principal"));
        let response = router
            .clone()
            .oneshot(request(Method::GET, path, foreign, Vec::new())?)
            .await?;
        ensure!(response.status() == StatusCode::NOT_FOUND);
        let error: bitrouter_ai::tenancy::operation::Error =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await?)?;
        ensure!(error.code == ErrorCode::NotFound);
        let response = router
            .oneshot(request(
                Method::PUT,
                "/control/v1/principals/lost-principal",
                token,
                body,
            )?)
            .await?;
        ensure!(response.status() == StatusCode::OK);
        let replay: OperationResult =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await?)?;
        ensure!(serde_json::to_value(replay)? == serde_json::to_value(recovered)?);
        let after = fixture.service.current().map_err(failure)?;
        ensure!(
            after.revision == committed.revision && after.operation_ids == committed.operation_ids
        );
        Ok(())
    }
    .await;
    finish(fixture, checked).await
}
