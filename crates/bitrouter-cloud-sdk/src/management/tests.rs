//! Wiremock-backed tests for [`crate::management::ManagementClient`].
//!
//! The pattern mirrors `crate::provider::applier::tests`: stand up one
//! `MockServer`, point both the AS metadata endpoint and the `/v1/*`
//! routes at it, persist a fresh credentials file in a temp dir, build
//! the client with [`ManagementClient::with_parts`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::auth::credentials::{Credentials, CredentialsStore};

fn tmp_creds_path(label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitrouter-cloud-mgmt-{label}-{}-{id}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("account-credentials.json")
}

fn fresh_creds(as_url: &str) -> Credentials {
    Credentials {
        access_token: "AT".into(),
        refresh_token: Some("RT".into()),
        expires_at: Utc::now() + ChronoDuration::seconds(3600),
        refresh_token_expires_at: None,
        token_type: "Bearer".into(),
        scope: "keys:read keys:write policy:read policy:write".into(),
        client_id: "bitrouter-cli".into(),
        authorization_server: as_url.to_owned(),
        subject: Some("u-1".into()),
    }
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

async fn metadata_mock(server: &MockServer) {
    let uri = server.uri();
    Mock::given(method("GET"))
        .and(wm_path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": uri,
            "device_authorization_endpoint": format!("{uri}/oauth/device_authorization"),
            "token_endpoint": format!("{uri}/oauth/token"),
        })))
        .mount(server)
        .await;
}

fn build_client(server: &MockServer, creds_path: &PathBuf) -> ManagementClient {
    let store = CredentialsStore::load(creds_path).unwrap();
    ManagementClient::with_parts(server.uri(), http_client(), store)
}

#[tokio::test]
async fn list_keys_attaches_bearer_and_decodes_body() {
    let server = MockServer::start().await;
    metadata_mock(&server).await;
    Mock::given(method("GET"))
        .and(wm_path("/v1/keys"))
        .and(header("authorization", "Bearer AT"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "id": "k_1",
                "display_name": "ci",
                "key_prefix": "brk_ci",
                "scopes": ["keys:read", "policy:read"],
                "expires_at": null,
                "last_used_at": null,
                "revoked_at": null,
                "created_at": "2026-05-25T00:00:00Z"
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let path = tmp_creds_path("list-keys");
    let mut store = CredentialsStore::load(&path).unwrap();
    store.save(fresh_creds(&server.uri())).unwrap();
    let client = build_client(&server, &path);

    let resp = client.list_keys().await.unwrap();
    assert_eq!(resp.data.len(), 1);
    assert_eq!(resp.data[0].display_name, "ci");
    assert_eq!(resp.data[0].scopes, vec!["keys:read", "policy:read"]);
}

#[tokio::test]
async fn mint_key_posts_json_body_and_returns_secret() {
    let server = MockServer::start().await;
    metadata_mock(&server).await;
    Mock::given(method("POST"))
        .and(wm_path("/v1/keys"))
        .and(header("authorization", "Bearer AT"))
        .and(body_string_contains("\"display_name\":\"ci\""))
        .and(body_string_contains("\"scopes\":[\"policy:read\"]"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "token": "brk_ci.secret",
            "id": "k_42",
            "key_prefix": "brk_ci",
            "display_name": "ci",
            "scopes": ["policy:read"],
            "expires_at": null,
        })))
        .expect(1)
        .mount(&server)
        .await;
    let path = tmp_creds_path("mint");
    let mut store = CredentialsStore::load(&path).unwrap();
    store.save(fresh_creds(&server.uri())).unwrap();
    let client = build_client(&server, &path);

    let resp = client
        .mint_key(&keys::MintApiKeyRequest {
            display_name: "ci".into(),
            scopes: vec!["policy:read".into()],
            expires_at: None,
        })
        .await
        .unwrap();
    assert_eq!(resp.token, "brk_ci.secret");
    assert_eq!(resp.id, "k_42");
}

#[tokio::test]
async fn forbidden_with_scope_message_is_parsed() {
    let server = MockServer::start().await;
    metadata_mock(&server).await;
    Mock::given(method("DELETE"))
        .and(wm_path("/v1/keys/k_x"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": "forbidden",
            "error_description": "missing required scope: keys:write",
        })))
        .mount(&server)
        .await;
    let path = tmp_creds_path("forbidden");
    let mut store = CredentialsStore::load(&path).unwrap();
    store.save(fresh_creds(&server.uri())).unwrap();
    let client = build_client(&server, &path);

    let err = client.revoke_key("k_x").await.unwrap_err();
    match err {
        Error::Forbidden {
            ref message,
            missing_scope: Some(ref scope),
        } => {
            assert_eq!(scope, "keys:write");
            assert!(message.contains("keys:write"), "message: {message}");
        }
        other => panic!("expected Forbidden with missing_scope, got {other:?}"),
    }
}

#[tokio::test]
async fn not_found_maps_to_typed_variant() {
    let server = MockServer::start().await;
    metadata_mock(&server).await;
    Mock::given(method("GET"))
        .and(wm_path("/v1/policies/ghost"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": "not_found",
            "error_description": "policy not found",
        })))
        .mount(&server)
        .await;
    let path = tmp_creds_path("notfound");
    let mut store = CredentialsStore::load(&path).unwrap();
    store.save(fresh_creds(&server.uri())).unwrap();
    let client = build_client(&server, &path);

    let err = client.get_policy("ghost").await.unwrap_err();
    assert!(matches!(err, Error::NotFound { .. }));
}

#[tokio::test]
async fn missing_credentials_file_returns_not_signed_in() {
    let path = tmp_creds_path("nosign");
    let err = ManagementClient::from_credentials_path(path).unwrap_err();
    assert!(matches!(err, Error::NotSignedIn));
}

#[tokio::test]
async fn metadata_fetched_once_across_multiple_calls() {
    let server = MockServer::start().await;
    // expect(1) on the metadata mock proves the cache is honoured.
    Mock::given(method("GET"))
        .and(wm_path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": server.uri(),
            "device_authorization_endpoint": format!("{}/oauth/device_authorization", server.uri()),
            "token_endpoint": format!("{}/oauth/token", server.uri()),
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(wm_path("/v1/keys"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
        .mount(&server)
        .await;
    let path = tmp_creds_path("cache");
    let mut store = CredentialsStore::load(&path).unwrap();
    store.save(fresh_creds(&server.uri())).unwrap();
    let client = build_client(&server, &path);

    for _ in 0..3 {
        let _ = client.list_keys().await.unwrap();
    }
    // The MockServer's drop-time assertions fire here.
    drop(server);
}

#[tokio::test]
async fn refreshes_bearer_within_refresh_window_then_calls_management() {
    let server = MockServer::start().await;
    metadata_mock(&server).await;
    Mock::given(method("POST"))
        .and(wm_path("/oauth/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "refreshed",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "rotated-rt",
            "scope": "keys:read",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(wm_path("/v1/keys"))
        .and(header("authorization", "Bearer refreshed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
        .expect(1)
        .mount(&server)
        .await;

    let path = tmp_creds_path("refresh");
    let mut store = CredentialsStore::load(&path).unwrap();
    // Within the refresh window — bearer() will exchange.
    store
        .save(Credentials {
            access_token: "stale".into(),
            refresh_token: Some("rt-original".into()),
            expires_at: Utc::now() + ChronoDuration::seconds(10),
            refresh_token_expires_at: None,
            token_type: "Bearer".into(),
            scope: "keys:read".into(),
            client_id: "bitrouter-cli".into(),
            authorization_server: server.uri(),
            subject: None,
        })
        .unwrap();
    let client = build_client(&server, &path);

    let _ = client.list_keys().await.unwrap();
    let reloaded = CredentialsStore::load(&path).unwrap();
    assert_eq!(
        reloaded.current().unwrap().refresh_token.as_deref(),
        Some("rotated-rt"),
    );
}

#[tokio::test]
async fn unknown_error_code_falls_back_to_server_variant() {
    let server = MockServer::start().await;
    metadata_mock(&server).await;
    Mock::given(method("GET"))
        .and(wm_path("/v1/billing/balance"))
        .respond_with(ResponseTemplate::new(502).set_body_json(json!({
            "error": "bad_gateway",
            "error_description": "upstream stripe failure",
        })))
        .mount(&server)
        .await;
    let path = tmp_creds_path("server-err");
    let mut store = CredentialsStore::load(&path).unwrap();
    store.save(fresh_creds(&server.uri())).unwrap();
    let client = build_client(&server, &path);

    let err = client.billing_balance().await.unwrap_err();
    match err {
        Error::Server { status, message } => {
            assert_eq!(status, 502);
            assert_eq!(message, "upstream stripe failure");
        }
        other => panic!("expected Server, got {other:?}"),
    }
}
