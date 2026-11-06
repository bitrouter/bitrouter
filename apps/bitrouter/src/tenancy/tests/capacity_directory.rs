//! Exercise the management HTTP contract beyond the old directory ceilings.
use super::*;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request},
};
use bitrouter_ai::{control::ControlScope, tenancy::operation::OperationStatus};
use serde::{Serialize, de::DeserializeOwned};
use tower::ServiceExt;

const TOKEN: &str = "capacity-directory-operator-with-at-least-32-bytes";

fn router(service: Service) -> Result<Router> {
    let auth = crate::remote_control::auth::ControlAuth::from_lookup(
        &crate::config::ControlConfig {
            credentials: vec![crate::config::ControlCredentialConfig {
                id: "test-operator".into(),
                token_env: "DIRECTORY_TEST_OPERATOR".into(),
                scopes: vec![ControlScope::Tenancy],
            }],
            ..Default::default()
        },
        |_| Some(TOKEN.into()),
    )?;
    Ok(crate::tenancy::http::router(service, auth))
}

async fn call<T: DeserializeOwned>(
    router: &Router,
    method: Method,
    path: &str,
    body: &impl Serialize,
) -> Result<T> {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(body)?))?,
        )
        .await?;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024).await?;
    ensure!(
        status.is_success(),
        "management {path} returned {status}: {}",
        String::from_utf8_lossy(&bytes)
    );
    Ok(serde_json::from_slice(&bytes)?)
}

#[tokio::test]
async fn directory_growth_keeps_http_issuance_replay_revocation_and_restart() -> Result<()> {
    let fixture = Fixture::new().await?;
    let api = router(fixture.service.clone())?;
    for index in 0..33 {
        let response: OperationResult = call(
            &api,
            Method::PUT,
            &format!("/control/v1/tenants/extra-{index}"),
            &TenantPut {
                operation_id: format!("tenant-{index}").try_into()?,
                expected_revision: None,
                state: TenantState::Active,
                installation_id: fixture.installation.clone(),
            },
        )
        .await?;
        ensure!(response.status == OperationStatus::Applied);
    }
    for index in 0..129 {
        let response: OperationResult = call(
            &api,
            Method::PUT,
            &format!("/control/v1/principals/extra-{index}"),
            &PrincipalPut {
                operation_id: format!("principal-{index}").try_into()?,
                expected_revision: None,
                issuer: "capacity-test".into(),
                subject: format!("subject-{index}"),
                installation_id: fixture.installation.clone(),
            },
        )
        .await?;
        ensure!(response.status == OperationStatus::Applied);
    }
    for index in 0..257 {
        let response: OperationResult = call(
            &api,
            Method::PUT,
            &format!(
                "/control/v1/tenants/{}/bindings/extra-{index}",
                fixture.tenant
            ),
            &BindingPut {
                operation_id: format!("binding-{index}").try_into()?,
                expected_revision: None,
                principal_id: fixture.principal.clone(),
                role: TenantRole::TenantAdmin,
                scope: crate::tenancy::mutation::tenant_scope(),
                expires_at: expiry()?,
                expected_management_owner: None,
                installation_id: fixture.installation.clone(),
            },
        )
        .await?;
        ensure!(response.status == OperationStatus::Applied);
    }
    let path = format!("/control/v1/tenants/{}/credentials", fixture.tenant);
    let mut first = None;
    let mut last = None;
    for index in 0..257 {
        let input = fixture.issuance(&format!("credential-{index}")).await?;
        let credential: CredentialIssued = call(&api, Method::POST, &path, &input).await?;
        ensure!(credential.secret.is_some());
        if first.is_none() {
            first = Some((input.clone(), credential.clone()));
        }
        last = Some(credential);
    }
    let (input, original) = first.context("first credential missing")?;
    let replay: CredentialIssued = call(&api, Method::POST, &path, &input).await?;
    ensure!(replay.secret.is_none() && replay.credential_id == original.credential_id);
    let last = last.context("last credential missing")?;
    for credential in [&original, &last] {
        fixture
            .service
            .authenticate_live(credential.secret.as_deref().context("secret")?)
            .await
            .map_err(failure)?;
    }
    let revoke = Mutation {
        operation_id: "revoke-after-growth".try_into()?,
        expected_revision: original.credential_revision.clone(),
        installation_id: fixture.installation.clone(),
    };
    let result: OperationResult = call(
        &api,
        Method::DELETE,
        &format!("{path}/{}", original.credential_id),
        &revoke,
    )
    .await?;
    ensure!(result.status == OperationStatus::Applied);
    fixture.service.shutdown().await?;
    let restarted = Service::open(fixture.db.clone(), None).await?;
    ensure!(
        matches!(restarted.authenticate_live(original.secret.as_deref().context("secret")?).await,
        Err(error) if error.0.code == ErrorCode::Unauthorized)
    );
    restarted
        .authenticate_live(last.secret.as_deref().context("secret")?)
        .await
        .map_err(failure)?;
    let replay: CredentialIssued =
        call(&router(restarted.clone())?, Method::POST, &path, &input).await?;
    ensure!(replay.secret.is_none() && replay.credential_id == original.credential_id);
    restarted.shutdown().await?;
    Ok(())
}
