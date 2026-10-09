use bitrouter_ai::auth::AuthApplier;
use bitrouter_ai::auth::store::DEFAULT_ACCOUNT;
use bitrouter_ai::error::{ModelError, Result};
use bitrouter_ai::target::ModelTarget;
use reqwest::header::HeaderValue;
use std::sync::Arc;
type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
use async_trait::async_trait;
use bitrouter::providers::claude_code::store::ClaudeStore;
use bitrouter::providers::import::claude_code::ClaudeCodeStore;
use bitrouter_ai::auth::oauth::{REFRESH_WINDOW, RefreshGrant};
use bitrouter_ai::auth::store::{CredentialStore as AccountStore, OAuthSession};
use bitrouter_ai::providers::anthropic::headers;
use bitrouter_ai::providers::claude_code::{ClaudeCodeAuthApplier, PROVIDER_ID};
use std::time::Duration;
fn claude_with_store(store: Arc<dyn AccountStore>) -> TestResult<ClaudeCodeAuthApplier> {
    let registration =
        bitrouter_ai::providers::login::find("anthropic").ok_or("missing Claude registration")?;
    Ok(ClaudeCodeAuthApplier::new(OAuthSession::new(
        store,
        Arc::new(RefreshGrant::new(
            reqwest::Client::new(),
            registration.auth.token_endpoint,
            registration.auth.client_id,
        )),
        REFRESH_WINDOW,
    )))
}
fn claude_applier(path: impl Into<std::path::PathBuf>) -> TestResult<ClaudeCodeAuthApplier> {
    claude_with_store(Arc::new(ClaudeStore::new(path.into(), None)?))
}
fn claude_with_endpoint(
    path: impl Into<std::path::PathBuf>,
    client: reqwest::Client,
    client_id: impl Into<String>,
    endpoint: impl Into<String>,
    live: Option<ClaudeCodeStore>,
) -> TestResult<ClaudeCodeAuthApplier> {
    Ok(ClaudeCodeAuthApplier::new(OAuthSession::new(
        Arc::new(ClaudeStore::new(path.into(), live)?),
        Arc::new(RefreshGrant::new(client, endpoint, client_id)),
        REFRESH_WINDOW,
    )))
}

use std::path::PathBuf;

use bitrouter_ai::types::ApiProtocol;
use wiremock::MockServer;

use bitrouter_ai::auth::credentials::Credential;
use bitrouter_ai::auth::credentials::OAuthToken;
use bitrouter_ai::auth::file::snapshot::CredentialStore;

fn tmp_store_path() -> TestResult<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitrouter-claude-code-test-{}-{id}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("creds.json"))
}

fn cc_target(label: Option<&str>) -> ModelTarget {
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

fn standard_request() -> TestResult<reqwest::Request> {
    Ok(reqwest::Client::new()
        .post("https://api.anthropic.com/v1/messages")
        .build()?)
}

/// A request from genuine Claude Code, including its agent-profile beta.
fn cc_request() -> TestResult<reqwest::Request> {
    let mut req = standard_request()?;
    req.headers_mut().insert(
        "anthropic-beta",
        HeaderValue::from_static("claude-code-20250219"),
    );
    Ok(req)
}

#[tokio::test]
async fn explicit_route_adds_agent_profile_to_standard_anthropic_request() -> TestResult {
    let path = tmp_store_path()?;
    let applier = claude_applier(&path)?.with_fallback_token(Some(OAuthToken {
        access_token: "sk-ant-oat-env".into(),
        expires_at: 0,
        refresh_token: None,
    }));
    let mut req = standard_request()?;
    req.headers_mut()
        .insert("x-api-key", HeaderValue::from_static("downstream-key"));

    let authed = applier.apply(req, &cc_target(None)).await?;
    let headers = authed.headers();
    let beta = headers
        .get("anthropic-beta")
        .and_then(|value| value.to_str().ok())
        .ok_or("missing beta header")?;
    assert!(beta.contains("claude-code-20250219"));
    assert!(beta.contains("oauth-2025-04-20"));
    assert_eq!(
        headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer sk-ant-oat-env")
    );
    assert!(headers.get("x-api-key").is_none());
    assert_eq!(
        headers
            .get(reqwest::header::USER_AGENT)
            .and_then(|value| value.to_str().ok()),
        Some(headers::CLAUDE_CODE_USER_AGENT)
    );
    assert_eq!(
        headers.get("x-app").and_then(|value| value.to_str().ok()),
        Some(headers::CLAUDE_CODE_X_APP)
    );
    Ok(())
}

#[tokio::test]
async fn errors_when_no_session() -> TestResult {
    let path = tmp_store_path()?;
    let applier = claude_applier(&path)?;
    let req = standard_request()?;
    let err = applier
        .apply(req, &cc_target(None))
        .await
        .err()
        .ok_or("operation unexpectedly succeeded")?;
    let msg = err.to_string();
    assert!(
        msg.contains("explicitly authorize it"),
        "expected helpful hint, got: {msg}"
    );
    Ok(())
}

#[tokio::test]
async fn env_oauth_token_applies_bearer_without_store_credential() -> TestResult {
    let path = tmp_store_path()?;
    let applier = claude_applier(&path)?.with_fallback_token(Some(OAuthToken {
        access_token: "sk-ant-oat-env".into(),
        expires_at: 0,
        refresh_token: None,
    }));
    let mut req = cc_request()?;
    req.headers_mut()
        .insert("x-api-key", HeaderValue::from_static("stale-key"));

    let authed = applier.apply(req, &cc_target(None)).await?;

    let headers = authed.headers();
    assert_eq!(
        headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer sk-ant-oat-env")
    );
    assert!(headers.get("x-api-key").is_none());
    assert!(
        headers
            .get("anthropic-beta")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|beta| beta.contains("oauth-2025-04-20"))
    );
    Ok(())
}

#[tokio::test]
async fn stored_oauth_token_applies_bearer_and_strips_x_api_key() -> TestResult {
    let path = tmp_store_path()?;
    {
        let mut store = CredentialStore::load(&path)?;
        store.set(
            PROVIDER_ID,
            DEFAULT_ACCOUNT,
            Credential::from_oauth_token(OAuthToken {
                access_token: "sk-ant-oat-fresh".into(),
                expires_at: 0, // non-expiring → no refresh attempt
                refresh_token: Some("r".into()),
            }),
        )?;
    }
    let applier = claude_applier(&path)?;
    let mut req = cc_request()?;
    // Pretend the protocol adapter already set x-api-key; the OAuth
    // path must strip it.
    req.headers_mut()
        .insert("x-api-key", HeaderValue::from_static("stale-key"));
    let authed = applier.apply(req, &cc_target(None)).await?;
    let h = authed.headers();
    assert_eq!(
        h.get(reqwest::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer sk-ant-oat-fresh")
    );
    assert!(h.get("x-api-key").is_none());
    let beta = h
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .ok_or("missing beta header")?;
    assert!(beta.contains("oauth-2025-04-20"));
    assert!(beta.contains("claude-code-20250219"));
    assert_eq!(
        h.get(reqwest::header::USER_AGENT)
            .and_then(|v| v.to_str().ok()),
        Some(headers::CLAUDE_CODE_USER_AGENT)
    );
    assert_eq!(
        h.get("x-app").and_then(|v| v.to_str().ok()),
        Some(headers::CLAUDE_CODE_X_APP)
    );
    Ok(())
}

#[tokio::test]
async fn oauth_merges_client_anthropic_beta_instead_of_overwriting() -> TestResult {
    // Real Claude Code traffic appends feature betas
    // (`context-management-…`, interleaved-thinking) next to the matching
    // request-body fields. The applier must keep them while adding the
    // OAuth-required betas — overwriting strips them and the upstream 400s
    // ("Extra inputs are not permitted") on the now-orphaned body field.
    let path = tmp_store_path()?;
    {
        let mut store = CredentialStore::load(&path)?;
        store.set(
            PROVIDER_ID,
            DEFAULT_ACCOUNT,
            Credential::from_oauth_token(OAuthToken {
                access_token: "sk-ant-oat".into(),
                expires_at: 0,
                refresh_token: Some("r".into()),
            }),
        )?;
    }
    let applier = claude_applier(&path)?;
    let mut req = cc_request()?;
    req.headers_mut().insert(
        "anthropic-beta",
        HeaderValue::from_static(
            "claude-code-20250219,context-management-2025-06-27,interleaved-thinking-2025-05-14",
        ),
    );
    let authed = applier.apply(req, &cc_target(None)).await?;
    let beta = authed
        .headers()
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .ok_or("missing beta header")?;
    assert!(
        beta.contains("oauth-2025-04-20"),
        "required beta dropped: {beta}"
    );
    assert!(
        beta.contains("claude-code-20250219"),
        "required beta dropped: {beta}"
    );
    assert!(
        beta.contains("context-management-2025-06-27"),
        "client beta dropped: {beta}"
    );
    assert!(
        beta.contains("interleaved-thinking-2025-05-14"),
        "client beta dropped: {beta}"
    );
    Ok(())
}

#[tokio::test]
async fn fresh_oauth_token_skips_refresh() -> TestResult {
    // A fresh stored token (1h
    // ahead of the 60s refresh window) is reused directly. The end-
    // to-end refresh round-trip is covered in
    // `oauth::refresh::tests::refresh_returns_new_access_token`.
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
    // Point the refresh endpoint at a wiremock that fails the test
    // if it's hit at all (no mounted responder → wiremock 404s).
    let server = MockServer::start().await;
    let applier = claude_with_endpoint(
        &path,
        reqwest::Client::new(),
        "client-1",
        format!("{}/oauth/token", server.uri()),
        None,
    )?;
    let req = cc_request()?;
    let authed = applier.apply(req, &cc_target(None)).await?;
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
async fn prepare_body_adds_agent_identity_without_losing_client_system() -> TestResult {
    let path = tmp_store_path()?;
    let applier = claude_applier(&path)?;
    let identity = headers::CLAUDE_AGENT_SYSTEM_PROMPT;

    let mut absent = serde_json::json!({ "model": "claude", "messages": [] });
    applier.prepare_body(&mut absent, &cc_target(None)).await?;
    assert_eq!(absent["system"][0]["text"], identity);

    let mut string = serde_json::json!({ "system": "be terse", "messages": [] });
    applier.prepare_body(&mut string, &cc_target(None)).await?;
    assert_eq!(string["system"][0]["text"], identity);
    assert_eq!(string["system"][1]["text"], "be terse");

    let original_block = serde_json::json!({
        "type": "text",
        "text": "follow the client rules",
        "cache_control": { "type": "ephemeral" }
    });
    let mut blocks = serde_json::json!({
        "system": [original_block.clone()],
        "messages": []
    });
    applier.prepare_body(&mut blocks, &cc_target(None)).await?;
    assert_eq!(blocks["system"][0]["text"], identity);
    assert_eq!(blocks["system"][1], original_block);

    let mut genuine = serde_json::json!({
        "system": [
            { "type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.215; cc_entrypoint=sdk-cli;" },
            { "type": "text", "text": identity },
            { "type": "text", "text": "keep this" }
        ],
        "messages": []
    });
    let before = genuine.clone();
    applier.prepare_body(&mut genuine, &cc_target(None)).await?;
    assert_eq!(genuine, before, "genuine Claude Code must stay idempotent");
    Ok(())
}

#[tokio::test]
async fn prepare_body_unwraps_litellm_extra_body_and_drops_session_id() -> TestResult {
    let path = tmp_store_path()?;
    let applier = claude_applier(&path)?;
    let mut body = serde_json::json!({
        "model": "claude-fable-5",
        "messages": [],
        "top_k": 7,
        "extra_body": {
            "session_id": "terminus-private-session",
            "top_k": 99,
            "custom_extension": { "enabled": true }
        }
    });

    applier.prepare_body(&mut body, &cc_target(None)).await?;

    assert!(body.get("extra_body").is_none());
    assert!(body.get("session_id").is_none());
    assert_eq!(body["top_k"], 7, "top-level client value must win");
    assert_eq!(body["custom_extension"]["enabled"], true);
    assert_eq!(
        body["system"][0]["text"],
        headers::CLAUDE_AGENT_SYSTEM_PROMPT
    );
    Ok(())
}

/// Write a `.credentials.json` in a fresh temp dir and return its path.
fn tmp_claude_creds(contents: &str) -> TestResult<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bitrouter-claude-code-cc-{}-{id}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(".credentials.json");
    std::fs::write(&path, contents)?;
    Ok(path)
}

fn seed_marker(store_path: &std::path::Path) -> TestResult {
    let mut store = CredentialStore::load(store_path)?;
    store.set(PROVIDER_ID, DEFAULT_ACCOUNT, Credential::ClaudeCodeCli)?;
    Ok(())
}

#[tokio::test]
async fn claude_code_cli_marker_applies_bearer_from_live_store() -> TestResult {
    // Marker in bitrouter's store + a non-expiring live Claude Code session
    // (no `expiresAt` → never refreshed) → the live access token is applied
    // as a Bearer and any stale x-api-key is stripped.
    let path = tmp_store_path()?;
    seed_marker(&path)?;
    let creds = tmp_claude_creds(
        r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat-live","refreshToken":"r"}}"#,
    )?;
    let applier = claude_with_endpoint(
        &path,
        reqwest::Client::new(),
        "client-1",
        "https://example.com/oauth/token",
        Some(ClaudeCodeStore::file_only(&creds)),
    )?;
    let mut req = cc_request()?;
    req.headers_mut()
        .insert("x-api-key", HeaderValue::from_static("stale"));
    let authed = applier.apply(req, &cc_target(None)).await?;
    let h = authed.headers();
    assert_eq!(
        h.get(reqwest::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer sk-ant-oat-live")
    );
    assert!(h.get("x-api-key").is_none());
    assert!(
        h.get("anthropic-beta")
            .and_then(|v| v.to_str().ok())
            .ok_or("missing beta header")?
            .contains("oauth-2025-04-20")
    );
    Ok(())
}

#[tokio::test]
async fn claude_code_cli_marker_missing_session_errors() -> TestResult {
    // Marker present but no live Claude Code session → a helpful 401 that
    // points the user at `claude auth login`, not a silent fall-through.
    let path = tmp_store_path()?;
    seed_marker(&path)?;
    let absent = std::env::temp_dir().join("bitrouter-claude-code-cc-absent/none.json");
    let applier = claude_with_endpoint(
        &path,
        reqwest::Client::new(),
        "client-1",
        "https://example.com/oauth/token",
        Some(ClaudeCodeStore::file_only(&absent)),
    )?;
    let req = cc_request()?;
    let err = applier
        .apply(req, &cc_target(None))
        .await
        .err()
        .ok_or("operation unexpectedly succeeded")?;
    assert!(
        err.to_string().contains("renew the permitted CLI session"),
        "expected a login hint, got: {err}"
    );
    Ok(())
}

#[tokio::test]
async fn claude_code_cli_marker_expiring_token_triggers_refresh() -> TestResult {
    // An expiring live token must drive a refresh attempt rather than serve
    // the stale token. Pointing the endpoint at an insecure (http) URL makes
    // `refresh` fail fast with a typed error, proving the needs_refresh
    // branch is taken. (The happy-path refresh and write-back are covered by
    // `oauth::refresh::tests` and `import::claude_code::tests` respectively;
    // an http MockServer can't exercise them because `refresh` requires
    // https.)
    let path = tmp_store_path()?;
    seed_marker(&path)?;
    let creds = tmp_claude_creds(
        r#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"RT","expiresAt":1000}}"#,
    )?;
    let applier = claude_with_endpoint(
        &path,
        reqwest::Client::new(),
        "client-1",
        "http://insecure.example.com/oauth/token",
        Some(ClaudeCodeStore::file_only(&creds)),
    )?;
    let req = cc_request()?;
    let err = applier
        .apply(req, &cc_target(None))
        .await
        .err()
        .ok_or("operation unexpectedly succeeded")?;
    assert!(
        err.to_string().contains("refresh"),
        "expected a refresh error proving the refresh path ran, got: {err}"
    );
    Ok(())
}

#[tokio::test]
async fn claude_code_cli_marker_non_expiring_is_reread_live_each_request() -> TestResult {
    // A non-expiring live token must NOT be cached for the process lifetime:
    // when `claude` rotates the on-disk token, the next request must see the
    // new value, keeping bitrouter in lockstep with the single source of
    // truth.
    let path = tmp_store_path()?;
    seed_marker(&path)?;
    let creds =
        tmp_claude_creds(r#"{"claudeAiOauth":{"accessToken":"first","refreshToken":"r"}}"#)?;
    let applier = claude_with_endpoint(
        &path,
        reqwest::Client::new(),
        "client-1",
        "https://example.com/oauth/token",
        Some(ClaudeCodeStore::file_only(&creds)),
    )?;
    let bearer = |req: reqwest::Request| {
        req.headers()
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let first = applier.apply(cc_request()?, &cc_target(None)).await?;
    assert_eq!(bearer(first).as_deref(), Some("Bearer first"));
    // Claude Code rotates its stored token.
    std::fs::write(
        &creds,
        r#"{"claudeAiOauth":{"accessToken":"second","refreshToken":"r"}}"#,
    )?;
    let second = applier.apply(cc_request()?, &cc_target(None)).await?;
    assert_eq!(
        bearer(second).as_deref(),
        Some("Bearer second"),
        "a non-expiring marker token must be re-read live, not served from a stale cache"
    );
    Ok(())
}
#[tokio::test]
async fn stored_claude_oauth_wins_over_allowed_environment_and_invalid_slot_fails()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use bitrouter_ai::auth::store::{CredentialKey, MemoryCredentialStore};
    let store = MemoryCredentialStore::new([
        (
            CredentialKey {
                provider: PROVIDER_ID.into(),
                account: "valid".into(),
            },
            Credential::Oauth(OAuthToken {
                access_token: "stored".into(),
                expires_at: 0,
                refresh_token: None,
            }),
        ),
        (
            CredentialKey {
                provider: PROVIDER_ID.into(),
                account: "wrong".into(),
            },
            Credential::api_key("wrong-kind"),
        ),
    ]);
    let applier = claude_with_store(Arc::new(store))?.with_fallback_token(Some(OAuthToken {
        access_token: "environment".into(),
        expires_at: 0,
        refresh_token: None,
    }));
    let applied = applier
        .apply(standard_request()?, &cc_target(Some("valid")))
        .await?;
    assert_eq!(
        applied
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer stored")
    );
    assert!(matches!(
        applier
            .apply(standard_request()?, &cc_target(Some("wrong")))
            .await,
        Err(ModelError::Provider { status: 401, .. })
    ));
    let mut explicit = cc_target(Some("wrong"));
    explicit.api_key = "explicit".into();
    assert_eq!(
        applier
            .apply(standard_request()?, &explicit)
            .await?
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer explicit")
    );
    Ok(())
}

struct HeldRefresh {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait]
impl bitrouter_ai::auth::store::OAuthRefresher for HeldRefresh {
    async fn refresh(&self, _current: &OAuthToken) -> Result<OAuthToken> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(OAuthToken {
            access_token: "rotated-cli-access".into(),
            expires_at: 0,
            refresh_token: Some("rotated-cli-refresh".into()),
        })
    }
}
#[tokio::test]
async fn cancelled_claude_call_commits_to_live_cli_source_for_another_applier()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("adoptions.json");
    let cli = directory.path().join("cli.json");
    let mut file = CredentialStore::load(&path)?;
    file.set(PROVIDER_ID, DEFAULT_ACCOUNT, Credential::ClaudeCodeCli)?;
    std::fs::write(&cli, br#"{"claudeAiOauth":{"accessToken":"expired","refreshToken":"old-refresh","expiresAt":1000}}"#)?;
    let refresh = Arc::new(HeldRefresh {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let make =
        || -> std::result::Result<ClaudeCodeAuthApplier, bitrouter_ai::auth::store::StoreError> {
            let backend = ClaudeStore::new(path.clone(), Some(ClaudeCodeStore::file_only(&cli)))?;
            Ok(ClaudeCodeAuthApplier::new(OAuthSession::new(
                Arc::new(backend),
                refresh.clone(),
                REFRESH_WINDOW,
            )))
        };
    let first = make()?;
    let second = make()?;
    let request = standard_request()?;
    let call = tokio::spawn(async move { first.apply(request, &cc_target(None)).await });
    tokio::time::timeout(Duration::from_secs(5), refresh.entered.notified()).await?;
    call.abort();
    assert!(call.await.is_err_and(|e| e.is_cancelled()));
    refresh.release.notify_one();
    let request = tokio::time::timeout(
        Duration::from_secs(5),
        second.apply(standard_request()?, &cc_target(None)),
    )
    .await??;
    assert_eq!(
        request
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer rotated-cli-access")
    );
    let live = ClaudeCodeStore::file_only(cli)
        .read()?
        .ok_or("missing CLI session")?;
    assert_eq!(
        live.token.refresh_token.as_deref(),
        Some("rotated-cli-refresh")
    );
    assert_eq!(
        file.get_any(PROVIDER_ID, DEFAULT_ACCOUNT),
        Some(&Credential::ClaudeCodeCli)
    );
    Ok(())
}
