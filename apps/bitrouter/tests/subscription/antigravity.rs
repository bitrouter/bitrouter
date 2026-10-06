use bitrouter_ai::auth::oauth::{REFRESH_WINDOW, RefreshGrant};
use bitrouter_ai::auth::store::OAuthSession;
use std::sync::Arc;
type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
fn antigravity_applier(path: impl Into<std::path::PathBuf>) -> TestResult<AntigravityAuthApplier> {
    let http = reqwest::Client::new();
    Ok(AntigravityAuthApplier::new(
        OAuthSession::new(
            Arc::new(FileCredentialStore::new(path.into())?),
            Arc::new(RefreshGrant::new(
                http.clone(),
                "https://fixture.invalid/token",
                "explicit-client",
            )),
            REFRESH_WINDOW,
        ),
        http,
    ))
}

use std::path::PathBuf;

use bitrouter_ai::types::ApiProtocol;

use bitrouter_ai::auth::AuthApplier;
use bitrouter_ai::auth::credentials::Credential;
use bitrouter_ai::auth::credentials::OAuthToken;
use bitrouter_ai::auth::file::backend::FileCredentialStore;
use bitrouter_ai::auth::file::snapshot::CredentialStore;
use bitrouter_ai::auth::store::DEFAULT_ACCOUNT;
use bitrouter_ai::providers::antigravity::{AntigravityAuthApplier, PROVIDER_ID, protocol};
use bitrouter_ai::target::ModelTarget;

fn tmp_store_path() -> TestResult<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitrouter-antigravity-test-{}-{id}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("creds.json"))
}

fn target() -> ModelTarget {
    ModelTarget {
        provider_name: PROVIDER_ID.into(),
        service_id: "gemini-2.5-flash".into(),
        api_base: "https://cloudcode-pa.googleapis.com".into(),
        api_key: String::new(),
        credential_priority: Default::default(),
        api_protocol: ApiProtocol::Custom(protocol::PROTOCOL.into()),
        compatibility: Default::default(),
        account_label: None,
        auth_scheme: Default::default(),
    }
}

#[tokio::test]
async fn apply_fails_without_credential() -> TestResult {
    let applier = antigravity_applier(tmp_store_path()?)?;
    let req = reqwest::Client::new()
        .post("https://cloudcode-pa.googleapis.com/v1internal:generateContent")
        .build()?;
    let err = applier
        .apply(req, &target())
        .await
        .err()
        .ok_or("operation unexpectedly succeeded")?;
    assert!(
        err.to_string()
            .contains("explicitly authorize this account"),
        "expected login hint, got: {err}"
    );
    Ok(())
}

#[tokio::test]
async fn apply_sets_bearer_and_spoof_headers() -> TestResult {
    let path = tmp_store_path()?;
    {
        let mut store = CredentialStore::load(&path)?;
        store.set(
            PROVIDER_ID,
            DEFAULT_ACCOUNT,
            Credential::from_oauth_token(OAuthToken {
                access_token: "ya29.test".into(),
                expires_at: 0, // non-expiring → no refresh
                refresh_token: Some("1//r".into()),
            }),
        )?;
    }
    let applier = antigravity_applier(&path)?;
    let req = reqwest::Client::new()
        .post("https://cloudcode-pa.googleapis.com/v1internal:generateContent")
        .build()?;
    let authed = applier.apply(req, &target()).await?;
    let h = authed.headers();
    assert_eq!(
        h.get(reqwest::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer ya29.test")
    );
    assert!(
        h.get(reqwest::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ua| ua.starts_with("antigravity/"))
    );
    assert!(h.get("client-metadata").is_some());
    assert!(h.get("x-goog-api-key").is_none());
    Ok(())
}
