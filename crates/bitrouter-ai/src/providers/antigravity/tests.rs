use super::*;
use crate::auth::oauth::{REFRESH_WINDOW, RefreshGrant};
use crate::auth::store::MemoryCredentialStore;
use crate::types::ApiProtocol;
fn target() -> ModelTarget {
    ModelTarget {
        provider_name: PROVIDER_ID.into(),
        service_id: "fixture-model".into(),
        api_base: "https://cloudcode-pa.googleapis.com".into(),
        api_key: String::new(),
        credential_priority: Default::default(),
        api_protocol: ApiProtocol::Custom(protocol::PROTOCOL.into()),
        compatibility: Default::default(),
        account_label: None,
        auth_scheme: Default::default(),
    }
}
#[test]
fn user_agent_and_platform_are_well_formed() {
    let ua = user_agent();
    assert!(ua.starts_with("antigravity/"));
    assert!(ua.contains('/'));
    let cm = client_metadata();
    assert!(cm.contains("\"pluginType\":\"GEMINI\""));
}

#[tokio::test]
async fn project_and_authorization_are_bound_to_the_same_credential_and_origin()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    for access in ["first", "second"] {
        Mock::given(method("POST"))
            .and(header("authorization", format!("Bearer {access}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"cloudaicompanionProject":format!("project-{access}")}),
            ))
            .expect(1)
            .mount(&server)
            .await;
    }
    let applier = AntigravityAuthApplier::new(
        OAuthSession::new(
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(RefreshGrant::new(
                reqwest::Client::new(),
                "https://fixture.invalid/token",
                "explicit-client",
            )),
            REFRESH_WINDOW,
        ),
        reqwest::Client::new(),
    );
    let mut target = target();
    target.api_base = server.uri();
    for access in ["first", "first", "second"] {
        target.api_key = access.into();
        let mut body = serde_json::json!({"contents":[]});
        applier.prepare_body(&mut body, &target).await?;
        assert!(body.get("project").is_none());
        let request = reqwest::Client::new()
            .post(format!("{}/model", server.uri()))
            .json(&body)
            .build()?;
        let applied = applier.apply(request, &target).await?;
        let body: serde_json::Value = serde_json::from_slice(
            applied
                .body()
                .and_then(reqwest::Body::as_bytes)
                .ok_or("missing bound body")?,
        )?;
        assert_eq!(body["project"], format!("project-{access}"));
        assert_eq!(
            applied
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some(format!("Bearer {access}").as_str())
        );
    }
    Ok(())
}
