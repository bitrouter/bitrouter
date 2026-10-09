use bitrouter_ai::auth::AuthApplier;
use bitrouter_ai::auth::credentials::Credential;
use bitrouter_ai::auth::store::DEFAULT_ACCOUNT;
use bitrouter_ai::error::ModelError;
use bitrouter_ai::target::ModelTarget;
use std::sync::Arc;
type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
use bitrouter_ai::auth::file::backend::FileCredentialStore;
use bitrouter_ai::providers::anthropic::{AnthropicApiKeyApplier, PROVIDER_ID, headers};
fn anthropic_applier(path: impl Into<std::path::PathBuf>) -> TestResult<AnthropicApiKeyApplier> {
    Ok(AnthropicApiKeyApplier::new(Arc::new(
        FileCredentialStore::new(path.into())?,
    )))
}

use std::path::PathBuf;

use bitrouter_ai::types::ApiProtocol;

use bitrouter_ai::auth::credentials::OAuthToken;
use bitrouter_ai::auth::file::snapshot::CredentialStore;

fn tmp_store_path() -> TestResult<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitrouter-anthropic-test-{}-{id}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("creds.json"))
}

fn anthropic_target(label: Option<&str>) -> ModelTarget {
    ModelTarget {
        provider_name: PROVIDER_ID.to_string(),
        service_id: "claude-opus-4-7".to_string(),
        api_base: "https://api.anthropic.com/v1".to_string(),
        api_key: String::new(),
        credential_priority: Default::default(),
        api_protocol: ApiProtocol::Messages,
        compatibility: Default::default(),
        account_label: label.map(String::from),
        auth_scheme: Default::default(),
    }
}

fn anthropic_target_with_env_key(key: &str) -> ModelTarget {
    let mut t = anthropic_target(None);
    t.api_key = key.to_string();
    t.credential_priority = bitrouter_ai::target::CredentialPriority::Fallback;
    t
}

#[tokio::test]
async fn fallthrough_uses_target_api_key_when_store_is_empty() -> TestResult {
    let path = tmp_store_path()?;
    let applier = anthropic_applier(&path)?;
    let req = reqwest::Client::new()
        .post("https://api.anthropic.com/v1/messages")
        .build()?;
    let target = anthropic_target_with_env_key("sk-ant-api03-env");
    let authed = applier.apply(req, &target).await?;
    let h = authed.headers();
    assert_eq!(
        h.get("x-api-key").and_then(|v| v.to_str().ok()),
        Some("sk-ant-api03-env")
    );
    assert_eq!(
        h.get("anthropic-version").and_then(|v| v.to_str().ok()),
        Some(headers::ANTHROPIC_VERSION)
    );
    assert!(h.get(reqwest::header::AUTHORIZATION).is_none());
    Ok(())
}

#[tokio::test]
async fn errors_when_no_credential_anywhere() -> TestResult {
    let path = tmp_store_path()?;
    let applier = anthropic_applier(&path)?;
    let req = reqwest::Client::new()
        .post("https://api.anthropic.com/v1/messages")
        .build()?;
    let err = applier
        .apply(req, &anthropic_target(None))
        .await
        .err()
        .ok_or("operation unexpectedly succeeded")?;
    let msg = err.to_string();
    assert!(
        msg.contains("explicitly provide or store an API-key credential"),
        "expected helpful hint, got: {msg}"
    );
    Ok(())
}

#[tokio::test]
async fn stored_api_key_overrides_target_fallthrough() -> TestResult {
    let path = tmp_store_path()?;
    // Seed an API key in the store.
    {
        let mut store = CredentialStore::load(&path)?;
        store.set(
            PROVIDER_ID,
            DEFAULT_ACCOUNT,
            Credential::api_key("sk-ant-api03-from-store"),
        )?;
    }
    let applier = anthropic_applier(&path)?;
    let req = reqwest::Client::new()
        .post("https://api.anthropic.com/v1/messages")
        .build()?;
    let authed = applier
        .apply(req, &anthropic_target_with_env_key("env-key-shadowed"))
        .await?;
    assert_eq!(
        authed
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok()),
        Some("sk-ant-api03-from-store")
    );
    Ok(())
}

#[tokio::test]
async fn multi_account_lookup_uses_target_label() -> TestResult {
    let path = tmp_store_path()?;
    {
        let mut store = CredentialStore::load(&path)?;
        store.set(PROVIDER_ID, "pro-max", Credential::api_key("for-pro-max"))?;
        store.set(PROVIDER_ID, "work-key", Credential::api_key("for-work"))?;
    }
    let applier = anthropic_applier(&path)?;
    let req = reqwest::Client::new()
        .post("https://api.anthropic.com/v1/messages")
        .build()?;
    let authed = applier
        .apply(req, &anthropic_target(Some("pro-max")))
        .await?;
    assert_eq!(
        authed
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok()),
        Some("for-pro-max")
    );
    let req2 = reqwest::Client::new()
        .post("https://api.anthropic.com/v1/messages")
        .build()?;
    let authed2 = applier
        .apply(req2, &anthropic_target(Some("work-key")))
        .await?;
    assert_eq!(
        authed2
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok()),
        Some("for-work")
    );
    Ok(())
}

#[tokio::test]
async fn api_key_prepare_body_leaves_system_untouched() -> TestResult {
    let path = tmp_store_path()?;
    {
        let mut store = CredentialStore::load(&path)?;
        store.set(
            PROVIDER_ID,
            DEFAULT_ACCOUNT,
            Credential::api_key("sk-ant-api03-x"),
        )?;
    }
    let applier = anthropic_applier(&path)?;
    let mut body = serde_json::json!({ "system": "user prompt", "messages": [] });
    applier
        .prepare_body(&mut body, &anthropic_target(None))
        .await?;
    assert_eq!(body["system"], serde_json::json!("user prompt"));
    Ok(())
}
#[tokio::test]
async fn explicit_key_wins_and_wrong_stored_kind_never_uses_fallback()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use bitrouter_ai::auth::store::{CredentialKey, MemoryCredentialStore};
    let key = CredentialKey {
        provider: PROVIDER_ID.into(),
        account: DEFAULT_ACCOUNT.into(),
    };
    let store = MemoryCredentialStore::new([(
        key,
        Credential::Oauth(OAuthToken {
            access_token: "stored-oauth".into(),
            expires_at: 0,
            refresh_token: None,
        }),
    )]);
    let applier = AnthropicApiKeyApplier::new(std::sync::Arc::new(store));
    let mut target = anthropic_target_with_env_key("explicit-key");
    target.credential_priority = bitrouter_ai::target::CredentialPriority::Explicit;
    let request = || {
        reqwest::Client::new()
            .post("https://example.invalid")
            .build()
    };
    assert_eq!(
        applier
            .apply(request()?, &target)
            .await?
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok()),
        Some("explicit-key")
    );
    target.credential_priority = bitrouter_ai::target::CredentialPriority::Fallback;
    assert!(matches!(
        applier.apply(request()?, &target).await,
        Err(ModelError::Provider { status: 401, .. })
    ));
    Ok(())
}
