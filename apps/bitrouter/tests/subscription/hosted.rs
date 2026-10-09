use std::sync::Arc;

use bitrouter_ai::auth::AuthApplier;
use bitrouter_ai::error::ModelError;
use bitrouter_ai::target::ModelTarget;
use bitrouter_ai::types::ApiProtocol;
use chrono::{Duration, Utc};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use bitrouter_ai::providers::hosted::PROVIDER_ID;
use bitrouter_ai::providers::hosted::applier::BitrouterAuthApplier;
use bitrouter_ai::providers::hosted::credentials::{Credentials, StoredCredential};

use bitrouter::cloud::account::manager::CredentialManager;

fn tmp_creds_path(label: &str) -> anyhow::Result<(tempfile::TempDir, std::path::PathBuf)> {
    let directory = tempfile::Builder::new()
        .prefix(&format!("bitrouter-hosted-applier-{label}-"))
        .tempdir()?;
    let path = directory.path().join("account-credentials.json");
    Ok((directory, path))
}

fn target_with_api_key(key: &str) -> ModelTarget {
    ModelTarget {
        provider_name: PROVIDER_ID.to_owned(),
        service_id: "gpt-4o".to_owned(),
        api_base: "https://api.bitrouter.ai/v1".to_owned(),
        api_key: key.to_owned(),
        credential_priority: Default::default(),
        api_protocol: ApiProtocol::ChatCompletions,
        compatibility: Default::default(),
        account_label: None,
        auth_scheme: Default::default(),
    }
}

fn empty_request() -> anyhow::Result<reqwest::Request> {
    Ok(reqwest::Client::new()
        .post("https://api.bitrouter.ai/v1/chat/completions")
        .build()?)
}

fn target_for_origin(origin: &str) -> ModelTarget {
    let mut target = target_with_api_key("");
    target.api_base = origin.to_owned();
    target
}

fn oauth_credential(
    authorization_server: &str,
    namespace_id: Option<&str>,
    access_token: &str,
    refresh_token: &str,
) -> Credentials {
    Credentials {
        access_token: access_token.to_owned(),
        refresh_token: Some(refresh_token.to_owned()),
        expires_at: Utc::now() + Duration::hours(1),
        refresh_token_expires_at: None,
        token_type: "Bearer".to_owned(),
        scope: "inference:invoke".to_owned(),
        client_id: "bitrouter-cli".to_owned(),
        authorization_server: authorization_server.to_owned(),
        namespace_id: namespace_id.map(str::to_owned),
        subject: Some("user-42".to_owned()),
    }
}

#[tokio::test]
async fn preserves_request_authorization_header() -> anyhow::Result<()> {
    let (_directory, path) = tmp_creds_path("raw-authorization")?;
    std::fs::write(&path, b"malformed")?;
    let manager = CredentialManager::with_client(path, reqwest::Client::new());
    let applier = applier(Arc::new(manager));
    let mut request = empty_request()?;
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer raw-request-token"),
    );
    let applied = applier
        .apply(request, &target_with_api_key("brk_config.secret"))
        .await?;
    assert_eq!(applied.headers()[AUTHORIZATION], "Bearer raw-request-token");
    Ok(())
}

#[tokio::test]
async fn explicit_target_key_bypasses_bad_oauth_store() -> anyhow::Result<()> {
    let (_directory, path) = tmp_creds_path("explicit-bypass")?;
    std::fs::write(&path, b"malformed")?;
    let manager = CredentialManager::with_client(path, reqwest::Client::new());
    let applier = applier(Arc::new(manager));
    let applied = applier
        .apply(empty_request()?, &target_with_api_key("brk_config.secret"))
        .await?;
    assert_eq!(applied.headers()[AUTHORIZATION], "Bearer brk_config.secret");
    Ok(())
}

#[tokio::test]
async fn applies_stored_api_key_for_request_origin() -> anyhow::Result<()> {
    let (_directory, path) = tmp_creds_path("stored-api-key")?;
    let manager = Arc::new(CredentialManager::with_client(path, reqwest::Client::new()));
    manager
        .save(StoredCredential::api_key(
            "brk_stored.secret".to_owned(),
            "https://api.bitrouter.ai".to_owned(),
        ))
        .await?;
    let applier = applier(manager);
    let applied = applier
        .apply(empty_request()?, &target_with_api_key(""))
        .await?;
    assert_eq!(applied.headers()[AUTHORIZATION], "Bearer brk_stored.secret");
    Ok(())
}

#[tokio::test]
async fn oauth_authority_survives_token_rotation_for_same_issuer_and_namespace()
-> anyhow::Result<()> {
    let origin = "https://issuer.example";
    let (_directory, path) = tmp_creds_path("oauth-rotation-authority")?;
    let manager = Arc::new(CredentialManager::with_client(path, reqwest::Client::new()));
    let applier = applier(Arc::clone(&manager));
    let target = target_for_origin(origin);
    manager
        .save(StoredCredential::from(oauth_credential(
            origin,
            Some("ns-one"),
            "access-before",
            "refresh-before",
        )))
        .await?;
    let before = applier.continuation_authority(&target).await?;
    manager
        .save(StoredCredential::from(oauth_credential(
            origin,
            Some("ns-one"),
            "access-after",
            "refresh-after",
        )))
        .await?;
    let after = applier.continuation_authority(&target).await?;
    assert_eq!(before, after);
    Ok(())
}

#[tokio::test]
async fn oauth_authority_separates_namespaces_for_same_issuer() -> anyhow::Result<()> {
    let origin = "https://issuer.example";
    let (_first_directory, first_path) = tmp_creds_path("oauth-first-namespace-authority")?;
    let first_manager = Arc::new(CredentialManager::with_client(
        first_path,
        reqwest::Client::new(),
    ));
    first_manager
        .save(StoredCredential::from(oauth_credential(
            origin,
            Some("ns-one"),
            "access-first",
            "refresh-first",
        )))
        .await?;
    let first = applier(first_manager)
        .continuation_authority(&target_for_origin(origin))
        .await?;
    let (_second_directory, second_path) = tmp_creds_path("oauth-second-namespace-authority")?;
    let second_manager = Arc::new(CredentialManager::with_client(
        second_path,
        reqwest::Client::new(),
    ));
    second_manager
        .save(StoredCredential::from(oauth_credential(
            origin,
            Some("ns-two"),
            "access-second",
            "refresh-second",
        )))
        .await?;
    let second = applier(second_manager)
        .continuation_authority(&target_for_origin(origin))
        .await?;
    assert_ne!(first, second);
    Ok(())
}

#[tokio::test]
async fn oauth_authority_separates_issuers_for_same_namespace() -> anyhow::Result<()> {
    let first_origin = "https://issuer-one.example";
    let second_origin = "https://issuer-two.example";
    let (_first_directory, first_path) = tmp_creds_path("oauth-first-issuer-authority")?;
    let first_manager = Arc::new(CredentialManager::with_client(
        first_path,
        reqwest::Client::new(),
    ));
    first_manager
        .save(StoredCredential::from(oauth_credential(
            first_origin,
            Some("ns-one"),
            "access-first",
            "refresh-first",
        )))
        .await?;
    let first = applier(first_manager)
        .continuation_authority(&target_for_origin(first_origin))
        .await?;
    let (_second_directory, second_path) = tmp_creds_path("oauth-second-issuer-authority")?;
    let second_manager = Arc::new(CredentialManager::with_client(
        second_path,
        reqwest::Client::new(),
    ));
    second_manager
        .save(StoredCredential::from(oauth_credential(
            second_origin,
            Some("ns-one"),
            "access-second",
            "refresh-second",
        )))
        .await?;
    let second = applier(second_manager)
        .continuation_authority(&target_for_origin(second_origin))
        .await?;
    assert_ne!(first, second);
    Ok(())
}

#[tokio::test]
async fn oauth_without_namespace_has_no_continuation_authority() -> anyhow::Result<()> {
    let origin = "https://issuer.example";
    let (_directory, path) = tmp_creds_path("oauth-missing-namespace")?;
    let manager = Arc::new(CredentialManager::with_client(path, reqwest::Client::new()));
    manager
        .save(StoredCredential::from(oauth_credential(
            origin,
            None,
            "access-token",
            "refresh-token",
        )))
        .await?;
    assert!(
        applier(manager)
            .continuation_authority(&target_for_origin(origin))
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn explicit_authority_tracks_the_effective_api_key() -> anyhow::Result<()> {
    let (_directory, path) = tmp_creds_path("explicit-authority")?;
    let applier = applier(Arc::new(CredentialManager::with_client(
        path,
        reqwest::Client::new(),
    )));
    let base = applier
        .continuation_authority(&target_with_api_key("brk_base.secret"))
        .await?;
    let mut overridden = target_with_api_key("brk_base.secret");
    overridden.api_key = "brk_override.secret".to_owned();
    let override_authority = applier.continuation_authority(&overridden).await?;
    let effective_key = applier
        .continuation_authority(&target_with_api_key("brk_override.secret"))
        .await?;
    assert_ne!(base, override_authority);
    assert_eq!(override_authority, effective_key);
    Ok(())
}

#[tokio::test]
async fn missing_credential_maps_to_onboarding_401() -> anyhow::Result<()> {
    let (_directory, path) = tmp_creds_path("missing")?;
    let manager = Arc::new(CredentialManager::with_client(path, reqwest::Client::new()));
    let error = match applier(manager)
        .apply(empty_request()?, &target_with_api_key(""))
        .await
    {
        Ok(_) => anyhow::bail!("missing credential unexpectedly authenticated a request"),
        Err(error) => error,
    };
    match error {
        ModelError::Provider { status, message } => {
            assert_eq!(status, 401);
            assert_eq!(message, onboarding_hint());
        }
        other => anyhow::bail!("expected upstream 401, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn corrupt_store_maps_to_internal_error() -> anyhow::Result<()> {
    let (_directory, path) = tmp_creds_path("corrupt")?;
    std::fs::write(&path, b"not-json")?;
    let error = match applier(Arc::new(CredentialManager::with_client(
        path,
        reqwest::Client::new(),
    )))
    .apply(empty_request()?, &target_with_api_key(""))
    .await
    {
        Ok(_) => anyhow::bail!("corrupt credential store unexpectedly authenticated a request"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        ModelError::CredentialStorage {
            failure: bitrouter_ai::auth::store::StoreError::Unavailable
        }
    ));
    Ok(())
}

#[tokio::test]
async fn metadata_failure_maps_to_upstream_502() -> anyhow::Result<()> {
    let server = MockServer::start().await;
    let origin = server.uri();
    Mock::given(method("GET"))
        .and(wm_path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let (_directory, path) = tmp_creds_path("metadata-failure")?;
    let manager = Arc::new(CredentialManager::with_client(path, reqwest::Client::new()));
    manager
        .save(StoredCredential::from(Credentials {
            access_token: "stale-access".to_owned(),
            refresh_token: Some("refresh-token".to_owned()),
            expires_at: Utc::now() + Duration::seconds(10),
            refresh_token_expires_at: None,
            token_type: "Bearer".to_owned(),
            scope: "inference:invoke".to_owned(),
            client_id: "bitrouter-cli".to_owned(),
            authorization_server: origin.clone(),
            namespace_id: Some("ns-test".to_owned()),
            subject: None,
        }))
        .await?;
    let request = reqwest::Client::new().post(&origin).build()?;
    let error = match applier(manager)
        .apply(request, &target_for_origin(&origin))
        .await
    {
        Ok(_) => anyhow::bail!("metadata failure unexpectedly authenticated a request"),
        Err(error) => error,
    };
    assert!(matches!(error, ModelError::Provider { status: 502, .. }));
    Ok(())
}

#[tokio::test]
async fn refresh_failure_maps_to_upstream_401() -> anyhow::Result<()> {
    let server = MockServer::start().await;
    let origin = server.uri();
    Mock::given(method("GET"))
        .and(wm_path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": origin,
            "device_authorization_endpoint": format!("{origin}/oauth/device_authorization"),
            "token_endpoint": format!("{origin}/oauth/token"),
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(wm_path("/oauth/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "invalid_grant",
        })))
        .mount(&server)
        .await;
    let (_directory, path) = tmp_creds_path("refresh-failure")?;
    let manager = Arc::new(CredentialManager::with_client(path, reqwest::Client::new()));
    manager
        .save(StoredCredential::from(Credentials {
            access_token: "stale-access".to_owned(),
            refresh_token: Some("refresh-token".to_owned()),
            expires_at: Utc::now() + Duration::seconds(10),
            refresh_token_expires_at: None,
            token_type: "Bearer".to_owned(),
            scope: "inference:invoke".to_owned(),
            client_id: "bitrouter-cli".to_owned(),
            authorization_server: origin.clone(),
            namespace_id: Some("ns-test".to_owned()),
            subject: None,
        }))
        .await?;
    let request = reqwest::Client::new().post(&origin).build()?;
    let error = match applier(manager)
        .apply(request, &target_for_origin(&origin))
        .await
    {
        Ok(_) => anyhow::bail!("refresh failure unexpectedly authenticated a request"),
        Err(error) => error,
    };
    assert!(matches!(error, ModelError::Provider { status: 401, .. }));
    Ok(())
}
#[tokio::test]
async fn fallback_never_hides_a_stored_origin_error_and_stored_key_wins() -> anyhow::Result<()> {
    let (_directory, path) = tmp_creds_path("fallback-priority")?;
    let manager = Arc::new(CredentialManager::with_client(path, reqwest::Client::new()));
    manager
        .save(StoredCredential::api_key(
            "stored-key".into(),
            "https://api.bitrouter.ai".into(),
        ))
        .await?;
    let applier = applier(manager);
    let mut target = target_with_api_key("fallback-key");
    target.credential_priority = bitrouter_ai::target::CredentialPriority::Fallback;
    let applied = applier.apply(empty_request()?, &target).await?;
    assert_eq!(
        applied
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer stored-key")
    );
    target.api_base = "https://other.invalid".into();
    assert!(matches!(
        applier.continuation_authority(&target).await,
        Err(ModelError::Provider { status: 401, .. })
    ));
    Ok(())
}

fn onboarding_hint() -> String {
    "hosted fixture onboarding".into()
}

fn applier(manager: std::sync::Arc<CredentialManager>) -> BitrouterAuthApplier {
    BitrouterAuthApplier::new(manager.session().clone(), onboarding_hint())
}
