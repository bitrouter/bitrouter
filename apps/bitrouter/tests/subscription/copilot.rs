use bitrouter_ai::auth::file::backend::FileCredentialStore;
use bitrouter_ai::providers::copilot::exchange::TOKEN_EXCHANGE_URL;
use bitrouter_ai::providers::copilot::{CopilotAuthApplier, PROVIDER_ID};
use std::sync::Arc;
fn copilot_with_url(
    client: reqwest::Client,
    url: impl Into<String>,
    path: impl Into<std::path::PathBuf>,
) -> TestResult<CopilotAuthApplier> {
    Ok(CopilotAuthApplier::new(
        client,
        url,
        Arc::new(FileCredentialStore::new(path.into())?),
    ))
}
fn copilot_applier(path: impl Into<std::path::PathBuf>) -> TestResult<CopilotAuthApplier> {
    copilot_with_url(reqwest::Client::new(), TOKEN_EXCHANGE_URL, path)
}
type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

use std::path::PathBuf;

use bitrouter_ai::types::ApiProtocol;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use bitrouter_ai::auth::AuthApplier;
use bitrouter_ai::auth::credentials::{Credential, OAuthToken};
use bitrouter_ai::auth::file::snapshot::CredentialStore;
use bitrouter_ai::auth::store::DEFAULT_ACCOUNT;
use bitrouter_ai::error::ModelError;
use bitrouter_ai::target::ModelTarget;

fn tmp_token_store_path() -> TestResult<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitrouter-copilot-test-{}-{id}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("tokens.json"))
}

fn copilot_target() -> ModelTarget {
    ModelTarget {
        provider_name: PROVIDER_ID.to_string(),
        service_id: "claude-sonnet-4.6".to_string(),
        api_base: "https://api.githubcopilot.com".to_string(),
        api_key: String::new(),
        credential_priority: Default::default(),
        api_protocol: ApiProtocol::Messages,
        compatibility: Default::default(),
        account_label: None,
        auth_scheme: Default::default(),
    }
}

#[tokio::test]
async fn fails_when_no_oauth_token_stored()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let store_path = tmp_token_store_path()?;
    let applier = copilot_applier(&store_path)?;
    let req = reqwest::Client::new()
        .post("https://api.githubcopilot.com/v1/messages")
        .build()?;
    let err = applier
        .apply(req, &copilot_target())
        .await
        .err()
        .ok_or("operation unexpectedly succeeded")?;
    let msg = err.to_string();
    assert!(
        msg.contains("explicitly authorize"),
        "expected helpful hint, got: {msg}"
    );
    Ok(())
}

#[tokio::test]
async fn exchanges_github_token_and_injects_headers() -> TestResult<()> {
    // Seed a stored GitHub OAuth token.
    let store_path = tmp_token_store_path()?;
    let mut store = CredentialStore::load(&store_path)?;
    store.set(
        PROVIDER_ID,
        DEFAULT_ACCOUNT,
        Credential::from_oauth_token(OAuthToken {
            access_token: "ghu_test_github_oauth".into(),
            expires_at: 0,
            refresh_token: None,
        }),
    )?;

    // Mock the GitHub → Copilot token exchange endpoint.
    let server = MockServer::start().await;
    let future_expiry = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs()
        + 3600;
    Mock::given(method("GET"))
        .and(path("/copilot_internal/v2/token"))
        .and(header("authorization", "token ghu_test_github_oauth"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "tid=copilot-bearer;exp=zzz",
            "expires_at": future_expiry,
            "refresh_in": 1500,
            "chat_enabled": true
        })))
        .expect(1)
        .mount(&server)
        .await;

    let applier = copilot_with_url(
        reqwest::Client::new(),
        format!("{}/copilot_internal/v2/token", server.uri()),
        &store_path,
    )?;
    let req = reqwest::Client::new()
        .post("https://api.githubcopilot.com/v1/messages")
        .build()?;
    let authed = applier.apply(req, &copilot_target()).await?;
    let headers = authed.headers();
    assert_eq!(
        headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer tid=copilot-bearer;exp=zzz")
    );
    assert!(headers.get("editor-version").is_some());
    assert!(headers.get("copilot-integration-id").is_some());
    Ok(())
}

#[tokio::test]
async fn cached_fresh_token_skips_exchange()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = tmp_token_store_path()?;
    let mut store = CredentialStore::load(&path)?;
    store.set(
        PROVIDER_ID,
        DEFAULT_ACCOUNT,
        Credential::Oauth(OAuthToken {
            access_token: "ghu-selected".into(),
            expires_at: 0,
            refresh_token: None,
        }),
    )?;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(header("authorization", "token ghu-selected"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"token":"tid=cached","expires_at":u64::MAX})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let applier = copilot_with_url(reqwest::Client::new(), server.uri(), path)?;
    for _ in 0..2 {
        assert_eq!(
            applier.obtain_copilot_token(&copilot_target()).await?.token,
            "tid=cached"
        );
    }
    Ok(())
}
#[tokio::test]
async fn cache_tracks_selected_github_source_and_logout_never_reuses_it()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("tokens.json");
    let mut file = CredentialStore::load(&path)?;
    let credential = |access: &str| {
        Credential::Oauth(OAuthToken {
            access_token: access.into(),
            expires_at: 0,
            refresh_token: None,
        })
    };
    file.set(PROVIDER_ID, DEFAULT_ACCOUNT, credential("other-account"))?;
    let server = MockServer::start().await;
    for access in ["selected-one", "selected-two"] {
        Mock::given(method("GET"))
            .and(header("authorization", format!("token {access}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"token":format!("copilot-{access}"),"expires_at":u64::MAX}),
            ))
            .expect(1)
            .mount(&server)
            .await;
    }
    let applier = copilot_with_url(reqwest::Client::new(), server.uri(), &path)?;
    let mut target = copilot_target();
    target.account_label = Some("selected".into());
    let mut authorities = Vec::new();
    for access in ["selected-one", "selected-two"] {
        file.set(PROVIDER_ID, "selected", credential(access))?;
        authorities.push(applier.continuation_authority(&target).await?);
        assert_eq!(
            applier.obtain_copilot_token(&target).await?.token,
            format!("copilot-{access}")
        );
    }
    assert_ne!(authorities[0], authorities[1]);
    file.remove(PROVIDER_ID, "selected")?;
    assert!(matches!(
        applier.obtain_copilot_token(&target).await,
        Err(ModelError::Provider { status: 401, .. })
    ));
    Ok(())
}
