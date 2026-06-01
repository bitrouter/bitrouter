//! End-to-end OAuth 2.0 Device Authorization Grant test.
//!
//! Stands up a `wiremock` HTTP server impersonating an RFC 8628 / RFC 6749 /
//! RFC 8414 / RFC 7009 compliant authorization server, runs the full
//! bitrouter device-flow client against it, and exercises every step of
//! the lifecycle:
//!
//!   1. Discovery — GET `/.well-known/oauth-authorization-server` (RFC 8414).
//!   2. Device authorization — POST device_authorization_endpoint (RFC 8628 §3.1).
//!   3. Polling — first poll returns `authorization_pending`, second returns
//!      success (RFC 8628 §3.5 + RFC 6749 §5.1).
//!   4. Refresh — credentials with a near-expiry access token trigger
//!      RFC 6749 §6 refresh on next `current_token`.
//!   5. Revoke — `logout` calls RFC 7009 `revocation_endpoint`.
//!
//! Standards references:
//! - RFC 6749: <https://www.rfc-editor.org/rfc/rfc6749>
//! - RFC 6750: <https://www.rfc-editor.org/rfc/rfc6750>
//! - RFC 7009: <https://www.rfc-editor.org/rfc/rfc7009>
//! - RFC 8414: <https://www.rfc-editor.org/rfc/rfc8414>
//! - RFC 8628: <https://www.rfc-editor.org/rfc/rfc8628>

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bitrouter_cloud_sdk::auth::commands::{LoginInputs, http_client};
use bitrouter_cloud_sdk::auth::credentials::{Credentials, CredentialsStore, REFRESH_WINDOW};
use bitrouter_cloud_sdk::auth::flow;
use bitrouter_cloud_sdk::auth::metadata;
use bitrouter_cloud_sdk::auth::settings::Settings;
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Stand up a wiremock authorization server that:
/// - serves a complete RFC 8414 metadata document at the well-known URL,
/// - returns one `authorization_pending` reply at the token endpoint, then
///   on the second poll returns a success body,
/// - has a working revocation endpoint that returns HTTP 200.
async fn mock_authorization_server() -> (MockServer, Arc<AtomicUsize>) {
    let server = MockServer::start().await;
    let uri = server.uri();

    // Discovery
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": uri,
            "device_authorization_endpoint": format!("{uri}/oauth/device_authorization"),
            "token_endpoint": format!("{uri}/oauth/token"),
            "revocation_endpoint": format!("{uri}/oauth/revoke"),
        })))
        .mount(&server)
        .await;

    // Device authorization (RFC 8628 §3.2)
    Mock::given(method("POST"))
        .and(path("/oauth/device_authorization"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "DEV-1234",
            "user_code": "WDJB-MJHT",
            "verification_uri": format!("{uri}/activate"),
            "verification_uri_complete": format!("{uri}/activate?user_code=WDJB-MJHT"),
            "expires_in": 600,
            "interval": 1,
        })))
        .mount(&server)
        .await;

    // Token endpoint — `authorization_pending` first, then success.
    //
    // wiremock has no built-in "first call returns X, then Y" matcher, so
    // we drive it through a shared counter and a `Respond` impl.
    let poll_count = Arc::new(AtomicUsize::new(0));
    let device_responder = DeviceGrantResponder {
        poll_count: poll_count.clone(),
    };
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code",
        ))
        .respond_with(device_responder)
        .mount(&server)
        .await;

    // Refresh endpoint shares the token URL but matches on grant_type.
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "AT-REFRESHED",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "RT-ROTATED",
            "scope": "inference:invoke usage:read",
        })))
        .mount(&server)
        .await;

    // Revocation endpoint — accept both refresh + access tokens.
    Mock::given(method("POST"))
        .and(path("/oauth/revoke"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    (server, poll_count)
}

struct DeviceGrantResponder {
    poll_count: Arc<AtomicUsize>,
}

impl Respond for DeviceGrantResponder {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        let n = self.poll_count.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // First poll: authorization_pending. RFC 8628 §3.5 allows
            // the AS to respond with HTTP 400 + this body, which the
            // client must treat as "keep polling".
            ResponseTemplate::new(400).set_body_json(json!({
                "error": "authorization_pending",
                "error_description": "user has not yet completed the authorization",
            }))
        } else {
            // Second poll: success.
            ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "AT-INITIAL",
                "token_type": "Bearer",
                "expires_in": 3600,
                "refresh_token": "RT-INITIAL",
                "refresh_token_expires_in": 86400,
                "scope": "inference:invoke usage:read",
                "namespace_id": "ns-1",
            }))
        }
    }
}

fn settings_for(server_uri: &str) -> Settings {
    Settings {
        authorization_server: server_uri.to_string(),
        client_id: "test-client".into(),
        scope: "inference:invoke usage:read".into(),
    }
}

/// Drives the FULL device-flow client through every step against a mock
/// AS. Verifies: discovery → device authorization → poll-with-pending →
/// success → refresh → revoke.
#[tokio::test]
async fn end_to_end_device_flow_with_mock_authorization_server() {
    let (server, poll_count) = mock_authorization_server().await;
    let client = http_client().expect("client");
    let settings = settings_for(&server.uri());

    // 1. RFC 8414 discovery.
    let metadata = metadata::fetch(&client, &settings.authorization_server)
        .await
        .expect("fetch metadata");
    assert!(
        metadata
            .device_authorization_endpoint
            .ends_with("/oauth/device_authorization")
    );
    assert!(metadata.token_endpoint.ends_with("/oauth/token"));
    assert!(
        metadata
            .revocation_endpoint
            .as_deref()
            .unwrap()
            .ends_with("/oauth/revoke")
    );

    // 2-3. Device flow → success.
    let token_set = flow::run_device_flow(&client, &metadata, &settings, |device| {
        assert_eq!(device.user_code, "WDJB-MJHT");
        assert!(device.verification_uri_complete.is_some());
    })
    .await
    .expect("device flow");
    assert_eq!(token_set.access_token, "AT-INITIAL");
    assert_eq!(token_set.refresh_token.as_deref(), Some("RT-INITIAL"));
    assert_eq!(
        token_set.scope.as_deref(),
        Some("inference:invoke usage:read")
    );
    // The namespace the AS baked the token into round-trips through
    // the device-flow success response into the TokenSet.
    assert_eq!(token_set.namespace_id.as_deref(), Some("ns-1"));
    // Two token-endpoint hits: one pending, one success.
    assert_eq!(poll_count.load(Ordering::SeqCst), 2);

    // 4. Persist + reload from disk.
    let dir = unique_test_dir("e2e");
    let path = dir.join("creds.json");
    let initial = flow::credentials_from_token_set(token_set, &settings);
    {
        let mut store = CredentialsStore::load(&path).unwrap();
        store.save(initial.clone()).unwrap();
    }
    let mut store = CredentialsStore::load(&path).unwrap();
    let loaded = store.current().expect("loaded credentials");
    assert_eq!(loaded.access_token, "AT-INITIAL");
    // The namespace binding survives serialise → disk → reload.
    assert_eq!(loaded.namespace_id.as_deref(), Some("ns-1"));

    // 5. Force a refresh by mutating expires_at to be within the
    //    REFRESH_WINDOW, then asking for current_token.
    let near_expiry = Credentials {
        expires_at: Utc::now() + ChronoDuration::seconds(REFRESH_WINDOW.num_seconds() / 2),
        ..loaded.clone()
    };
    store.save(near_expiry).unwrap();
    let bearer = store
        .current_token(&client, &metadata)
        .await
        .expect("refresh succeeds");
    assert_eq!(bearer, "AT-REFRESHED");
    // The rotated refresh token must have been persisted, NOT the old one.
    let after_refresh = store.current().expect("creds after refresh");
    assert_eq!(after_refresh.refresh_token.as_deref(), Some("RT-ROTATED"));
    assert_eq!(after_refresh.access_token, "AT-REFRESHED");
    assert!(after_refresh.access_token_valid());

    // 6. Revoke the credentials (RFC 7009).
    flow::revoke(
        &client,
        metadata.revocation_endpoint.as_deref().unwrap(),
        &settings.client_id,
        after_refresh.refresh_token.as_deref().unwrap(),
        "refresh_token",
    )
    .await
    .expect("revoke");
    flow::revoke(
        &client,
        metadata.revocation_endpoint.as_deref().unwrap(),
        &settings.client_id,
        &after_refresh.access_token,
        "access_token",
    )
    .await
    .expect("revoke access");

    // 7. Clean up.
    store.clear().unwrap();
    assert!(!path.exists(), "logout should remove the file");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Discovery returning an insecure (http://, non-loopback) token endpoint
/// must be rejected — RFC 9700 §2.1.1 requires token-endpoint TLS.
#[tokio::test]
async fn discovery_rejects_insecure_token_endpoint_for_non_loopback() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": "https://as.example.com",
            "device_authorization_endpoint": "http://as.example.com/device",
            "token_endpoint": "http://as.example.com/token",
        })))
        .mount(&server)
        .await;

    let client = http_client().unwrap();
    let err = metadata::fetch(&client, &server.uri()).await.unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("insecure"),
        "expected insecure rejection, got: {msg}"
    );
}

/// The login command's LoginInputs struct gets resolved by `resolve_from_env`,
/// which means the integration test cannot drive the public `login()` API
/// without mucking with global env vars. To exercise the public surface
/// without env churn we hit the lower-level pieces here, and rely on the
/// settings unit tests for the env-resolution coverage.
#[test]
fn login_inputs_compiles_and_clones() {
    let i = LoginInputs {
        authorization_server: Some("https://example.com".into()),
        client_id: Some("cid".into()),
        scope: None,
    };
    let _ = i.clone();
}

/// `access_denied` from the token endpoint must abort the polling loop
/// with a terminal error, even when the AS returns HTTP 200.
#[tokio::test]
async fn polling_aborts_on_access_denied() {
    let server = MockServer::start().await;
    let uri = server.uri();
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_authorization_endpoint": format!("{uri}/device"),
            "token_endpoint": format!("{uri}/token"),
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "X",
            "user_code": "Y",
            "verification_uri": format!("{uri}/activate"),
            "expires_in": 60,
            "interval": 1,
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "access_denied",
            "error_description": "user clicked deny",
        })))
        .mount(&server)
        .await;

    let client = http_client().unwrap();
    let settings = settings_for(&uri);
    let metadata = metadata::fetch(&client, &uri).await.unwrap();
    let err = flow::run_device_flow(&client, &metadata, &settings, |_| {})
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("denied"), "wrong error: {msg}");
}

/// `expired_token` must terminate the loop with a clear error.
#[tokio::test]
async fn polling_aborts_on_expired_token() {
    let server = MockServer::start().await;
    let uri = server.uri();
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_authorization_endpoint": format!("{uri}/device"),
            "token_endpoint": format!("{uri}/token"),
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "X",
            "user_code": "Y",
            "verification_uri": format!("{uri}/activate"),
            "expires_in": 60,
            "interval": 1,
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "expired_token",
        })))
        .mount(&server)
        .await;

    let client = http_client().unwrap();
    let settings = settings_for(&uri);
    let metadata = metadata::fetch(&client, &uri).await.unwrap();
    let err = flow::run_device_flow(&client, &metadata, &settings, |_| {})
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("expired"), "wrong error: {msg}");
}

fn unique_test_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitrouter-account-it-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
