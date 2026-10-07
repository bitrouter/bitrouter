use bitrouter_ai::auth::file::backend::FileCredentialStore;
use bitrouter_ai::auth::oauth::{REFRESH_WINDOW, RefreshGrant};
use bitrouter_ai::auth::store::OAuthSession;
use bitrouter_ai::providers::supergrok::{
    CLIENT_ID, PROVIDER_ID, SuperGrokAuthApplier, TOKEN_ENDPOINT,
};
use std::sync::Arc;
fn supergrok_with_endpoint(
    path: impl Into<std::path::PathBuf>,
    client: reqwest::Client,
    client_id: impl Into<String>,
    endpoint: impl Into<String>,
) -> TestResult<SuperGrokAuthApplier> {
    Ok(SuperGrokAuthApplier::new(OAuthSession::new(
        Arc::new(FileCredentialStore::new(path.into())?),
        Arc::new(RefreshGrant::new(client, endpoint, client_id)),
        REFRESH_WINDOW,
    )))
}
fn supergrok_applier(path: impl Into<std::path::PathBuf>) -> TestResult<SuperGrokAuthApplier> {
    supergrok_with_endpoint(path, reqwest::Client::new(), CLIENT_ID, TOKEN_ENDPOINT)
}
type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

use std::path::PathBuf;

use bitrouter_ai::types::ApiProtocol;
use wiremock::MockServer;

use bitrouter_ai::auth::AuthApplier;
use bitrouter_ai::auth::credentials::{Credential, OAuthToken};
use bitrouter_ai::auth::file::snapshot::CredentialStore;
use bitrouter_ai::auth::store::DEFAULT_ACCOUNT;
use bitrouter_ai::error::ModelError;
use bitrouter_ai::target::ModelTarget;

fn tmp_store_path() -> TestResult<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitrouter-supergrok-test-{}-{id}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("creds.json"))
}

fn supergrok_target(label: Option<&str>) -> ModelTarget {
    ModelTarget {
        provider_name: PROVIDER_ID.to_string(),
        service_id: "grok-build-0.1".to_string(),
        api_base: "https://api.x.ai/v1".to_string(),
        api_key: String::new(),
        credential_priority: Default::default(),
        api_protocol: ApiProtocol::Responses,
        compatibility: Default::default(),
        account_label: label.map(String::from),
        auth_scheme: Default::default(),
    }
}

#[tokio::test]
async fn applies_bearer_from_stored_oauth()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = tmp_store_path()?;
    {
        let mut store = CredentialStore::load(&path)?;
        store.set(
            PROVIDER_ID,
            DEFAULT_ACCOUNT,
            Credential::from_oauth_token(OAuthToken {
                access_token: "grok-jwt-fresh".into(),
                expires_at: 0, // non-expiring → no refresh attempt
                refresh_token: Some("r".into()),
            }),
        )?;
    }
    let applier = supergrok_applier(&path)?;
    let req = reqwest::Client::new()
        .post("https://api.x.ai/v1/responses")
        .build()?;
    let authed = applier.apply(req, &supergrok_target(None)).await?;
    assert_eq!(
        authed
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer grok-jwt-fresh")
    );
    Ok(())
}

#[tokio::test]
async fn fails_when_no_credential_stored()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = tmp_store_path()?;
    let applier = supergrok_applier(&path)?;
    let req = reqwest::Client::new()
        .post("https://api.x.ai/v1/responses")
        .build()?;
    let err = applier
        .apply(req, &supergrok_target(None))
        .await
        .err()
        .ok_or("operation unexpectedly succeeded")?;
    assert!(
        err.to_string().contains("explicitly authorize"),
        "expected helpful hint, got: {err}"
    );
    Ok(())
}

#[tokio::test]
async fn rejects_api_key_credential()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = tmp_store_path()?;
    {
        let mut store = CredentialStore::load(&path)?;
        store.set(PROVIDER_ID, DEFAULT_ACCOUNT, Credential::api_key("xai-..."))?;
    }
    let applier = supergrok_applier(&path)?;
    let req = reqwest::Client::new()
        .post("https://api.x.ai/v1/responses")
        .build()?;
    let err = applier
        .apply(req, &supergrok_target(None))
        .await
        .err()
        .ok_or("operation unexpectedly succeeded")?;
    assert!(
        err.to_string().contains("subscription OAuth"),
        "expected API-key rejection, got: {err}"
    );
    Ok(())
}

#[tokio::test]
async fn fresh_token_skips_refresh() -> TestResult<()> {
    let path = tmp_store_path()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    {
        let mut store = CredentialStore::load(&path)?;
        store.set(
            PROVIDER_ID,
            DEFAULT_ACCOUNT,
            Credential::from_oauth_token(OAuthToken {
                access_token: "still-fresh".into(),
                expires_at: now + 3600,
                refresh_token: Some("ignored".into()),
            }),
        )?;
    }
    // Point refresh at a wiremock with no mounts → any hit 404s and fails.
    let server = MockServer::start().await;
    let applier = supergrok_with_endpoint(
        &path,
        reqwest::Client::new(),
        "client-1",
        format!("{}/oauth/token", server.uri()),
    )?;
    let req = reqwest::Client::new()
        .post("https://api.x.ai/v1/responses")
        .build()?;
    let authed = applier.apply(req, &supergrok_target(None)).await?;
    assert_eq!(
        authed
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer still-fresh")
    );
    Ok(())
}
#[tokio::test]
async fn fresh_calls_observe_selected_slot_replacement_and_do_not_fallback_after_failure()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("tokens.json");
    let mut file = CredentialStore::load(&path)?;
    let token = |access: &str| {
        Credential::Oauth(OAuthToken {
            access_token: access.into(),
            expires_at: 0,
            refresh_token: None,
        })
    };
    file.set(PROVIDER_ID, "selected", token("old"))?;
    let applier = supergrok_applier(&path)?;
    let mut target = supergrok_target(Some("selected"));
    target.api_key = "fallback".into();
    target.credential_priority = bitrouter_ai::target::CredentialPriority::Fallback;
    let request = || {
        reqwest::Client::new()
            .post("https://example.invalid")
            .build()
    };
    for access in ["old", "new-login"] {
        file.set(PROVIDER_ID, "selected", token(access))?;
        let applied = applier.apply(request()?, &target).await?;
        assert_eq!(
            applied
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some(format!("Bearer {access}").as_str())
        );
    }
    file.set(
        PROVIDER_ID,
        "selected",
        Credential::api_key("invalid-stored"),
    )?;
    assert!(matches!(
        applier.apply(request()?, &target).await,
        Err(ModelError::Provider { status: 401, .. })
    ));
    Ok(())
}
