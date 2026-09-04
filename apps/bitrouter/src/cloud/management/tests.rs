//! Wiremock-backed coverage for the application-owned management client.

use std::sync::Arc;

use anyhow::Context;
use bitrouter_providers::hosted::account::credentials::{Credentials, StoredCredential};
use bitrouter_providers::hosted::account::manager::CredentialManager;
use chrono::{Duration, Utc};
use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

fn oauth_credential(base_url: &str, expires_in: Duration) -> StoredCredential {
    StoredCredential::from(Credentials {
        access_token: "oauth-access".to_owned(),
        refresh_token: Some("oauth-refresh".to_owned()),
        expires_at: Utc::now() + expires_in,
        refresh_token_expires_at: None,
        token_type: "Bearer".to_owned(),
        scope: "keys:read keys:write".to_owned(),
        client_id: "bitrouter-cli".to_owned(),
        authorization_server: base_url.to_owned(),
        namespace_id: Some("ns-1".to_owned()),
        subject: Some("user-1".to_owned()),
    })
}

async fn client_with(
    credential: StoredCredential,
) -> anyhow::Result<(tempfile::TempDir, Arc<CredentialManager>, ManagementClient)> {
    let directory = tempfile::tempdir()?;
    let manager = Arc::new(CredentialManager::with_client(
        directory.path().join("account-credentials.json"),
        reqwest::Client::new(),
    ));
    manager.save(credential).await?;
    let client = ManagementClient::from_manager(Arc::clone(&manager)).await?;
    Ok((directory, manager, client))
}

#[tokio::test]
async fn api_key_uses_me_namespace_and_decodes_response() -> anyhow::Result<()> {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/namespaces/me/keys"))
        .and(header("authorization", "Bearer brk_stored.secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "id": "key-1",
                "display_name": "ci",
                "key_prefix": "brk_ci",
                "scopes": ["keys:read"],
                "expires_at": null,
                "last_used_at": null,
                "revoked_at": null,
                "created_at": "2026-05-25T00:00:00Z"
            }]
        })))
        .mount(&server)
        .await;
    let (_directory, _manager, client) = client_with(StoredCredential::api_key(
        "brk_stored.secret".to_owned(),
        server.uri(),
    ))
    .await?;

    let response = client.list_keys().await?;

    let key = response.data.first().context("missing decoded API key")?;
    assert_eq!(key.display_name, "ci");
    assert_eq!(key.scopes, ["keys:read"]);
    Ok(())
}

#[tokio::test]
async fn mint_key_posts_body_and_returns_one_time_secret() -> anyhow::Result<()> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/namespaces/ns-1/keys"))
        .and(header("authorization", "Bearer oauth-access"))
        .and(body_string_contains("\"display_name\":\"ci\""))
        .and(body_string_contains("\"scopes\":[\"policy:read\"]"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "token": "brk_ci.secret",
            "id": "key-42",
            "key_prefix": "brk_ci",
            "display_name": "ci",
            "scopes": ["policy:read"],
            "expires_at": null
        })))
        .mount(&server)
        .await;
    let (_directory, _manager, client) =
        client_with(oauth_credential(&server.uri(), Duration::hours(1))).await?;

    let response = client
        .mint_key(&keys::MintApiKeyRequest {
            display_name: "ci".to_owned(),
            scopes: vec!["policy:read".to_owned()],
            expires_at: None,
        })
        .await?;

    assert_eq!(response.id, "key-42");
    assert_eq!(response.token, "brk_ci.secret");
    Ok(())
}

#[tokio::test]
async fn fresh_oauth_bearer_skips_discovery() -> anyhow::Result<()> {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/namespaces/ns-1/keys"))
        .and(header("authorization", "Bearer oauth-access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
        .mount(&server)
        .await;
    let (_directory, _manager, client) =
        client_with(oauth_credential(&server.uri(), Duration::hours(1))).await?;

    client.list_keys().await?;

    let requests = match server.received_requests().await {
        Some(requests) => requests,
        None => anyhow::bail!("wiremock did not record requests"),
    };
    assert!(
        requests
            .iter()
            .all(|request| request.url.path() != "/.well-known/oauth-authorization-server")
    );
    Ok(())
}

#[tokio::test]
async fn refresh_preserves_namespace_and_persists_rotation() -> anyhow::Result<()> {
    let server = MockServer::start().await;
    let uri = server.uri();
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": uri,
            "device_authorization_endpoint": format!("{uri}/oauth/device"),
            "token_endpoint": format!("{uri}/oauth/token")
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "rotated-access",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "rotated-refresh",
            "scope": "keys:read",
            "namespace_id": "ns-other"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/namespaces/ns-1/keys"))
        .and(header("authorization", "Bearer rotated-access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
        .mount(&server)
        .await;
    let (_directory, manager, client) =
        client_with(oauth_credential(&server.uri(), Duration::seconds(10))).await?;

    client.list_keys().await?;
    let current = manager
        .current()
        .await?
        .context("credential disappeared after refresh")?;
    let oauth = current.oauth().context("OAuth credential changed kind")?;

    assert_eq!(oauth.refresh_token.as_deref(), Some("rotated-refresh"));
    assert_eq!(oauth.namespace_id.as_deref(), Some("ns-1"));
    Ok(())
}

#[tokio::test]
async fn manager_constructor_and_namespace_errors_stay_typed() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let empty = Arc::new(CredentialManager::with_client(
        directory.path().join("empty.json"),
        reqwest::Client::new(),
    ));
    let missing = match ManagementClient::from_manager(empty).await {
        Ok(_) => anyhow::bail!("empty manager unexpectedly built a client"),
        Err(error) => error,
    };
    assert!(matches!(missing, Error::NotSignedIn));

    let server = MockServer::start().await;
    let mut credential = oauth_credential(&server.uri(), Duration::hours(1));
    match &mut credential {
        StoredCredential::Oauth { credential } => credential.namespace_id = None,
        StoredCredential::ApiKey { .. } => anyhow::bail!("test OAuth credential changed kind"),
    }
    let (_directory, _manager, client) = client_with(credential).await?;
    let no_namespace = match client.list_keys().await {
        Ok(_) => anyhow::bail!("namespace-free OAuth request unexpectedly succeeded"),
        Err(error) => error,
    };
    assert!(matches!(no_namespace, Error::NoNamespace));
    Ok(())
}

#[tokio::test]
async fn management_http_errors_keep_wire_taxonomy() -> anyhow::Result<()> {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/v1/namespaces/ns-1/keys/key-x"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": "forbidden",
            "error_description": "missing required scope: keys:write"
        })))
        .mount(&server)
        .await;
    let (_directory, _manager, client) =
        client_with(oauth_credential(&server.uri(), Duration::hours(1))).await?;

    let error = match client.revoke_key("key-x").await {
        Ok(_) => anyhow::bail!("forbidden request unexpectedly succeeded"),
        Err(error) => error,
    };

    match error {
        Error::Forbidden {
            message,
            missing_scope: Some(scope),
        } => {
            assert_eq!(scope, "keys:write");
            assert!(message.contains("keys:write"));
        }
        other => anyhow::bail!("expected typed forbidden response, got {other:?}"),
    }
    Ok(())
}
