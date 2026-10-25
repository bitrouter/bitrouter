use super::*;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use bitrouter_ai::control::ControlScope;
use tower::ServiceExt;

#[tokio::test]
async fn ambiguous_http_inputs_and_host_read_tokens_never_admit_mutations() -> Result<()> {
    let fixture = Fixture::new().await?;
    let operator_token = "tenancy-http-operator-token-32-bytes-minimum";
    let reader_token = "host-read-token-with-at-least-32-bytes";
    let operators = crate::remote_control::auth::ControlAuth::from_lookup(
        &crate::config::ControlConfig {
            credentials: vec![
                crate::config::ControlCredentialConfig {
                    id: "test-operator".into(),
                    token_env: "TENANCY_TEST_OPERATOR".into(),
                    scopes: vec![ControlScope::Tenancy],
                },
                crate::config::ControlCredentialConfig {
                    id: "host-reader".into(),
                    token_env: "TENANCY_TEST_READER".into(),
                    scopes: vec![ControlScope::Read],
                },
            ],
            ..Default::default()
        },
        |name| {
            Some(
                if name == "TENANCY_TEST_OPERATOR" {
                    operator_token
                } else {
                    reader_token
                }
                .into(),
            )
        },
    )?;
    let router = super::super::http::router(fixture.service.clone(), operators);
    let input = serde_json::to_string(&TenantPut {
        operation_id: "must-not-admit".try_into()?,
        expected_revision: None,
        state: TenantState::Active,
        installation_id: fixture.installation.clone(),
    })?;
    let bearer = format!("Bearer {operator_token}");
    let read_bearer = format!("Bearer {reader_token}");
    let cases = [
        (vec![], input.clone(), StatusCode::UNAUTHORIZED),
        (
            vec![("authorization", read_bearer.as_str())],
            input.clone(),
            StatusCode::FORBIDDEN,
        ),
        (
            vec![
                ("authorization", bearer.as_str()),
                ("authorization", read_bearer.as_str()),
            ],
            input.clone(),
            StatusCode::BAD_REQUEST,
        ),
        (
            vec![
                ("authorization", bearer.as_str()),
                ("x-api-key", operator_token),
            ],
            input.clone(),
            StatusCode::BAD_REQUEST,
        ),
        (
            vec![
                ("authorization", bearer.as_str()),
                ("bitrouter-installation-id", "another-installation"),
            ],
            input.clone(),
            StatusCode::CONFLICT,
        ),
        (
            vec![
                ("authorization", bearer.as_str()),
                ("bitrouter-installation-id", fixture.installation.as_str()),
                ("bitrouter-installation-id", fixture.installation.as_str()),
            ],
            input.clone(),
            StatusCode::BAD_REQUEST,
        ),
        (
            vec![
                ("authorization", bearer.as_str()),
                ("content-encoding", "gzip"),
            ],
            input.clone(),
            StatusCode::BAD_REQUEST,
        ),
        (
            vec![
                ("authorization", bearer.as_str()),
                ("origin", "https://foreign.invalid"),
            ],
            input.clone(),
            StatusCode::FORBIDDEN,
        ),
        (
            vec![("authorization", bearer.as_str())],
            input.replacen('{', "{\"operation_id\":\"duplicate\",", 1),
            StatusCode::BAD_REQUEST,
        ),
        (
            vec![("authorization", bearer.as_str())],
            input.replace("\"expected_revision\":null,", ""),
            StatusCode::BAD_REQUEST,
        ),
        (
            vec![("authorization", bearer.as_str())],
            " ".repeat(2 * 1024 * 1024 + 1),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
    ];
    let before = fixture.service.current().map_err(failure)?.operation_ids;
    for (headers, body, expected) in cases {
        let mut request = Request::builder()
            .method("PUT")
            .uri("/control/v1/tenants/new-tenant")
            .header("content-type", "application/json");
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::from(body))?)
            .await?;
        ensure!(
            response.status() == expected,
            "unexpected HTTP rejection status: {} instead of {expected}",
            response.status()
        );
        ensure!(
            response
                .headers()
                .get("cache-control")
                .is_some_and(|value| value == "no-store")
        );
        let bytes = axum::body::to_bytes(response.into_body(), 4096).await?;
        let error: bitrouter_ai::tenancy::operation::Error = serde_json::from_slice(&bytes)?;
        ensure!(
            error.dispatch_state == bitrouter_ai::tenancy::operation::DispatchState::NotStarted
        );
        ensure!(!String::from_utf8_lossy(&bytes).contains(operator_token));
    }
    ensure!(fixture.service.current().map_err(failure)?.operation_ids == before);
    fixture.service.shutdown().await?;
    Ok(())
}
