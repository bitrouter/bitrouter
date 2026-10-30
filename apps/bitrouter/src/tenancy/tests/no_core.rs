//! Disabling core removes the evaluation transport from the protected router.
use super::*;
use axum::{body::Body, http::Request};
use tower::ServiceExt;

#[tokio::test]
async fn gateway_only_profile_does_not_mount_owned_evaluation_routes() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let fixture = Fixture::new().await?;
        let issued = fixture.issue("gateway-only-ledger-absence").await?;
        let auth = crate::remote_control::auth::ControlAuth::from_lookup(
            &crate::config::ControlConfig {
                credentials: vec![crate::config::ControlCredentialConfig {
                    id: "test-operator".into(),
                    token_env: "GATEWAY_ONLY_TEST_OPERATOR".into(),
                    scopes: vec![bitrouter_ai::control::ControlScope::Tenancy],
                }],
                ..Default::default()
            },
            |_| Some("gateway-only-test-operator-at-least-32".into()),
        )?;
        let router = crate::tenancy::http::router(fixture.service.clone(), auth);
        for (method, suffix) in [
            ("POST", "subjects"),
            ("GET", "subjects"),
            ("GET", "subjects/subject"),
            ("POST", "results"),
            ("POST", "snapshots"),
            ("GET", "snapshots/root"),
            ("GET", "status"),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(format!(
                            "/control/v1/tenants/{}/evals/{suffix}",
                            fixture.tenant
                        ))
                        .header(
                            "authorization",
                            format!("Bearer {}", issued.secret.as_deref().context("secret")?),
                        )
                        .header("bitrouter-installation-id", fixture.installation.as_str())
                        .body(Body::empty())?,
                )
                .await?;
            ensure!(response.status() == axum::http::StatusCode::NOT_FOUND);
        }
        fixture.service.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
