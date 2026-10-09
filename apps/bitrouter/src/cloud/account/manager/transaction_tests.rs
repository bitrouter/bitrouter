use std::sync::Arc;
use std::time::Duration as StdDuration;

use bitrouter_ai::auth::store::StoreError;
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::CredentialManager;
use crate::cloud::account::credentials::CredentialsStore;
use crate::cloud::account::transaction::AccountTransaction;
use bitrouter_ai::providers::hosted::credentials::{Credentials, StoredCredential};
use bitrouter_ai::providers::hosted::session::{CredentialError, CredentialSource};

struct RefreshServer {
    endpoint: String,
    started: oneshot::Receiver<()>,
    release: oneshot::Sender<()>,
    work: JoinHandle<anyhow::Result<String>>,
}

// The exchange has observably received the request before caller cancellation,
// file failure or login/logout. No sleeps or mere endpoint status are evidence.
async fn refresh_server(response: Value) -> anyhow::Result<RefreshServer> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/token", listener.local_addr()?);
    let (started_tx, started) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    let work = tokio::spawn(async move {
        tokio::time::timeout(StdDuration::from_secs(10), async move {
            let (mut socket, _) = listener.accept().await?;
            let mut bytes = Vec::new();
            let mut buffer = [0; 1024];
            loop {
                let count = socket.read(&mut buffer).await?;
                anyhow::ensure!(count > 0, "refresh request ended before its body");
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(end) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&bytes[..end])?;
                    let length = headers.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length").then_some(value.trim())
                    }).ok_or_else(|| anyhow::anyhow!("refresh request lacks body length"))?.parse::<usize>()?;
                    if bytes.len() >= end + 4 + length { break; }
                }
            }
            started_tx.send(()).map_err(|_| anyhow::anyhow!("refresh admission observer dropped"))?;
            release_rx.await?;
            let body = serde_json::to_string(&response)?;
            let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            socket.write_all(reply.as_bytes()).await?;
            socket.shutdown().await?;
            Ok(String::from_utf8(bytes)?)
        }).await?
    });
    Ok(RefreshServer {
        endpoint,
        started,
        release,
        work,
    })
}

fn old_credential(issuer: &str) -> Credentials {
    Credentials {
        access_token: "old-access".into(),
        refresh_token: Some("old-refresh".into()),
        expires_at: Utc::now() - Duration::seconds(1),
        refresh_token_expires_at: Some(Utc::now() + Duration::hours(1)),
        token_type: "Bearer".into(),
        scope: "inference:invoke account:read".into(),
        client_id: "persisted-client".into(),
        authorization_server: issuer.into(),
        namespace_id: Some("stored-namespace".into()),
        subject: Some("stored-subject".into()),
    }
}

fn replacement() -> Value {
    json!({"access_token":"new-access", "refresh_token":"new-refresh", "expires_in":3600})
}

async fn seeded_manager(
    endpoint: &str,
) -> anyhow::Result<(tempfile::TempDir, MockServer, Arc<CredentialManager>)> {
    let directory = tempfile::tempdir()?;
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_authorization_endpoint":format!("{}/device",server.uri()), "token_endpoint":endpoint
        }))).expect(1).mount(&server).await;
    let manager = Arc::new(CredentialManager::with_client(
        directory.path().join("account-credentials.json"),
        reqwest::Client::new(),
    ));
    manager.save(old_credential(&server.uri()).into()).await?;
    Ok((directory, server, manager))
}

#[tokio::test]
async fn separate_managers_share_one_rotation_and_preserve_full_metadata() -> anyhow::Result<()> {
    let refresh = refresh_server(replacement()).await?;
    let (directory, server, first) = seeded_manager(&refresh.endpoint).await?;
    let original = first
        .current()
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing seed"))?;
    #[cfg(unix)]
    let second_path = {
        let alias = directory.path().join("credential-alias.json");
        std::os::unix::fs::symlink(first.path(), &alias)?;
        alias
    };
    #[cfg(not(unix))]
    let second_path = first.path().to_path_buf();
    let second = CredentialManager::with_client(second_path, reqwest::Client::new());
    let first_task = tokio::spawn(async move { first.session().resolve_bearer(None, None).await });
    refresh.started.await?;
    let (waiting_tx, waiting_rx) = oneshot::channel();
    let second_task = tokio::spawn(async move {
        waiting_tx
            .send(())
            .map_err(|_| CredentialError::Storage(StoreError::Unavailable))?;
        second.session().resolve_bearer(None, None).await
    });
    waiting_rx.await?;
    refresh
        .release
        .send(())
        .map_err(|_| anyhow::anyhow!("refresh server stopped"))?;
    assert_eq!(first_task.await??.secret(), "new-access");
    assert_eq!(second_task.await??.secret(), "new-access");
    let request = refresh.work.await??;
    assert!(request.contains("client_id=persisted-client"));
    assert!(request.contains("scope=inference%3Ainvoke+account%3Aread"));
    let manager = CredentialManager::with_client(
        directory.path().join("account-credentials.json"),
        reqwest::Client::new(),
    );
    let stored = manager
        .current()
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing rotation"))?;
    let got = stored
        .oauth()
        .ok_or_else(|| anyhow::anyhow!("rotation changed kind"))?;
    let old = original
        .oauth()
        .ok_or_else(|| anyhow::anyhow!("seed changed kind"))?;
    assert_eq!(got.refresh_token.as_deref(), Some("new-refresh"));
    assert_eq!(got.refresh_token_expires_at, old.refresh_token_expires_at);
    assert_eq!(got.scope, old.scope);
    assert_eq!(got.token_type, old.token_type);
    assert_eq!(got.client_id, old.client_id);
    assert_eq!(got.authorization_server, server.uri());
    assert_eq!(got.namespace_id, old.namespace_id);
    assert_eq!(got.subject, old.subject);
    Ok(())
}

#[tokio::test]
async fn cancellation_after_exchange_admission_still_commits_rotation() -> anyhow::Result<()> {
    let mut response = replacement();
    response["refresh_token_expires_in"] = json!(7200);
    response["scope"] = json!("inference:invoke");
    let refresh = refresh_server(response).await?;
    let (_directory, _server, manager) = seeded_manager(&refresh.endpoint).await?;
    let caller = manager.clone();
    let task = tokio::spawn(async move { caller.session().resolve_bearer(None, None).await });
    refresh.started.await?;
    task.abort();
    assert!(task.await.is_err_and(|error| error.is_cancelled()));
    refresh
        .release
        .send(())
        .map_err(|_| anyhow::anyhow!("refresh server stopped"))?;
    refresh.work.await??;
    // `current` obtains the same lease: its return proves owned refresh completed.
    let stored = manager
        .current()
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing committed rotation"))?;
    let credential = stored
        .oauth()
        .ok_or_else(|| anyhow::anyhow!("rotation changed kind"))?;
    assert_eq!(credential.access_token, "new-access");
    assert_eq!(credential.refresh_token.as_deref(), Some("new-refresh"));
    assert_eq!(credential.scope, "inference:invoke");
    assert!(
        credential
            .refresh_token_expires_at
            .is_some_and(|time| time > Utc::now() + Duration::minutes(110))
    );
    assert_eq!(credential.namespace_id.as_deref(), Some("stored-namespace"));
    Ok(())
}

#[tokio::test]
async fn failed_commit_retains_rotation_across_managers_without_second_exchange()
-> anyhow::Result<()> {
    let refresh = refresh_server(replacement()).await?;
    let (_directory, _server, manager) = seeded_manager(&refresh.endpoint).await?;
    let caller = manager.clone();
    let task = tokio::spawn(async move { caller.session().resolve_bearer(None, None).await });
    refresh.started.await?;
    let temporary = manager.path().with_extension("json.tmp");
    std::fs::create_dir(&temporary)?;
    refresh
        .release
        .send(())
        .map_err(|_| anyhow::anyhow!("refresh server stopped"))?;
    refresh.work.await??;
    assert!(matches!(
        task.await?,
        Err(CredentialError::Storage(StoreError::Unavailable))
    ));
    let before = CredentialsStore::load(manager.path())?;
    assert_eq!(
        before
            .current()
            .and_then(StoredCredential::oauth)
            .map(|token| token.access_token.as_str()),
        Some("old-access")
    );
    std::fs::remove_dir(temporary)?;
    let recovered = CredentialManager::with_client(manager.path(), reqwest::Client::new());
    assert_eq!(
        recovered
            .session()
            .resolve_bearer(None, None)
            .await?
            .secret(),
        "new-access"
    );
    let persisted = recovered
        .current()
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing recovered credential"))?;
    assert_eq!(
        persisted
            .oauth()
            .and_then(|token| token.refresh_token.as_deref()),
        Some("new-refresh")
    );
    Ok(())
}

#[tokio::test]
async fn refresh_cannot_overwrite_concurrent_login_or_resurrect_logout() -> anyhow::Result<()> {
    for logout in [false, true] {
        let refresh = refresh_server(replacement()).await?;
        let (_directory, server, manager) = seeded_manager(&refresh.endpoint).await?;
        let caller = manager.clone();
        let task = tokio::spawn(async move { caller.session().resolve_bearer(None, None).await });
        refresh.started.await?;
        let mut external = CredentialsStore::load(manager.path())?;
        if logout {
            external.clear()?;
        } else {
            external.save(StoredCredential::api_key("new-login".into(), server.uri()))?;
        }
        refresh
            .release
            .send(())
            .map_err(|_| anyhow::anyhow!("refresh server stopped"))?;
        refresh.work.await??;
        assert!(matches!(
            task.await?,
            Err(CredentialError::Storage(StoreError::Conflict))
        ));
        if logout {
            assert!(manager.current().await?.is_none());
            assert!(matches!(
                manager.session().resolve_bearer(None, None).await,
                Err(CredentialError::NotSignedIn)
            ));
        } else {
            let current = manager.session().resolve_bearer(None, None).await?;
            assert_eq!(current.secret(), "new-login");
            assert_eq!(current.source(), CredentialSource::StoredApiKey);
        }
    }
    Ok(())
}

#[tokio::test]
async fn waiting_for_a_lease_is_cancellable_without_auth_io() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let manager = Arc::new(CredentialManager::with_client(
        directory.path().join("account-credentials.json"),
        reqwest::Client::new(),
    ));
    manager
        .save(old_credential("https://unreachable.invalid").into())
        .await?;
    let lease = AccountTransaction::begin(manager.path()).await?;
    let (attempted_tx, attempted_rx) = oneshot::channel();
    let caller = manager.clone();
    let task = tokio::spawn(async move {
        attempted_tx
            .send(())
            .map_err(|_| CredentialError::Storage(StoreError::Unavailable))?;
        caller.session().resolve_bearer(None, None).await
    });
    attempted_rx.await?;
    task.abort();
    assert!(task.await.is_err_and(|error| error.is_cancelled()));
    drop(lease);
    assert_eq!(
        manager
            .current()
            .await?
            .and_then(|credential| credential.oauth().map(|token| token.access_token.clone()))
            .as_deref(),
        Some("old-access")
    );
    Ok(())
}

#[tokio::test]
async fn contradictory_refresh_identity_is_retained_but_never_dispatched_or_retried()
-> anyhow::Result<()> {
    for subject_change in [false, true] {
        let mut response = replacement();
        if subject_change {
            use base64::Engine as _;
            let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(br#"{"sub":"other-subject"}"#);
            response["id_token"] = json!(format!("header.{payload}.signature"));
        } else {
            response["namespace_id"] = json!("other-namespace");
        }
        let refresh = refresh_server(response).await?;
        let (_directory, server, manager) = seeded_manager(&refresh.endpoint).await?;
        let caller = manager.clone();
        let task = tokio::spawn(async move { caller.session().resolve_bearer(None, None).await });
        refresh.started.await?;
        refresh
            .release
            .send(())
            .map_err(|_| anyhow::anyhow!("refresh server stopped"))?;
        refresh.work.await??;
        assert!(matches!(task.await?, Err(CredentialError::Refresh(_))));
        let second = CredentialManager::with_client(manager.path(), reqwest::Client::new());
        assert!(matches!(
            second.session().resolve_bearer(None, None).await,
            Err(CredentialError::Refresh(_))
        ));
        // Original persisted binding remains intact; contradictory material is
        // retained only in process memory until the application replaces the login.
        assert_eq!(
            manager
                .current()
                .await?
                .and_then(|stored| stored.oauth().map(|token| token.access_token.clone()))
                .as_deref(),
            Some("old-access")
        );
        second
            .save(StoredCredential::api_key("new-login".into(), server.uri()))
            .await?;
        assert!(matches!(
            second.session().resolve_bearer(None, None).await,
            Err(CredentialError::Storage(StoreError::Conflict))
        ));
        assert_eq!(
            second.session().resolve_bearer(None, None).await?.secret(),
            "new-login"
        );
    }
    Ok(())
}

#[tokio::test]
async fn issuer_paths_have_separate_metadata_cache_entries() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let server = MockServer::start().await;
    let manager = CredentialManager::with_client(
        directory.path().join("account-credentials.json"),
        reqwest::Client::new(),
    );
    for issuer_path in ["first", "second"] {
        Mock::given(method("GET")).and(path(format!("/.well-known/oauth-authorization-server/{issuer_path}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"device_authorization_endpoint":format!("{}/device", server.uri()), "token_endpoint":format!("{}/{issuer_path}/token", server.uri())})))
            .expect(1).mount(&server).await;
        Mock::given(method("POST"))
            .and(path(format!("/{issuer_path}/token")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token":issuer_path, "expires_in":3600})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let credential = old_credential(&format!("{}/{issuer_path}", server.uri()));
        manager.save(credential.into()).await?;
        assert_eq!(
            manager.session().resolve_bearer(None, None).await?.secret(),
            issuer_path
        );
    }
    Ok(())
}

#[tokio::test]
async fn metadata_and_refresh_diagnostics_do_not_expose_server_payloads() -> anyhow::Result<()> {
    for discovery_failure in [false, true] {
        let directory = tempfile::tempdir()?;
        let server = MockServer::start().await;
        let template = if discovery_failure {
            ResponseTemplate::new(500).set_body_string("secret-from-metadata")
        } else {
            ResponseTemplate::new(200).set_body_json(json!({"device_authorization_endpoint":format!("{}/device",server.uri()), "token_endpoint":format!("{}/token",server.uri())}))
        };
        Mock::given(method("GET"))
            .and(path("/.well-known/oauth-authorization-server"))
            .respond_with(template)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(
                json!({"error":"invalid_grant", "error_description":"secret-from-refresh"}),
            ))
            .mount(&server)
            .await;
        let manager = CredentialManager::with_client(
            directory.path().join("account-credentials.json"),
            reqwest::Client::new(),
        );
        manager.save(old_credential(&server.uri()).into()).await?;
        let error = manager
            .session()
            .resolve_bearer(None, None)
            .await
            .err()
            .ok_or_else(|| anyhow::anyhow!("failed auth unexpectedly succeeded"))?;
        assert!(!format!("{error:?} {error}").contains("secret-from"));
        assert!(if discovery_failure {
            matches!(error, CredentialError::Metadata(_))
        } else {
            matches!(error, CredentialError::Refresh(_))
        });
    }
    Ok(())
}
