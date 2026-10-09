use async_trait::async_trait;
use bitrouter_ai::auth::file::backend::FileCredentialStore;
use bitrouter_ai::auth::oauth::{REFRESH_WINDOW, RefreshGrant};
use bitrouter_ai::auth::store::{CredentialStore as AccountStore, OAuthRefresher, OAuthSession};
use bitrouter_ai::providers::codex::{OpenAiCodexAuthApplier, PROVIDER_ID, headers};
use reqwest::header::HeaderValue;
use std::sync::Arc;
use std::time::Duration;
fn codex_with_refresher(
    store: Arc<dyn AccountStore>,
    refresher: Arc<dyn OAuthRefresher>,
) -> OpenAiCodexAuthApplier {
    OpenAiCodexAuthApplier::new(OAuthSession::new(store, refresher, REFRESH_WINDOW))
}
fn codex_with_endpoint(
    path: impl Into<std::path::PathBuf>,
    client: reqwest::Client,
    client_id: impl Into<String>,
    endpoint: impl Into<String>,
) -> TestResult<OpenAiCodexAuthApplier> {
    Ok(codex_with_refresher(
        Arc::new(FileCredentialStore::new(path.into())?),
        Arc::new(RefreshGrant::new(client, endpoint, client_id)),
    ))
}
fn codex_applier(path: impl Into<std::path::PathBuf>) -> TestResult<OpenAiCodexAuthApplier> {
    codex_with_endpoint(
        path,
        reqwest::Client::new(),
        "fixture-client",
        "https://auth.openai.com/oauth/token",
    )
}
type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
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
    let dir =
        std::env::temp_dir().join(format!("bitrouter-codex-test-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("creds.json"))
}

fn codex_target(label: Option<&str>) -> ModelTarget {
    ModelTarget {
        provider_name: PROVIDER_ID.to_string(),
        service_id: "gpt-5-codex".to_string(),
        api_base: "https://chatgpt.com/backend-api/codex".to_string(),
        api_key: String::new(),
        credential_priority: Default::default(),
        api_protocol: ApiProtocol::Responses,
        compatibility: Default::default(),
        account_label: label.map(String::from),
        auth_scheme: Default::default(),
    }
}

fn make_jwt_with_account(account_id: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode("{}");
    let payload = URL_SAFE_NO_PAD.encode(format!(
            r#"{{"exp":1700000000,"https://api.openai.com/auth":{{"chatgpt_account_id":"{account_id}"}}}}"#
        ));
    let sig = URL_SAFE_NO_PAD.encode("sig");
    format!("{header}.{payload}.{sig}")
}

#[tokio::test]
async fn applies_bearer_account_id_and_integration_headers() -> TestResult<()> {
    let path = tmp_store_path()?;
    let jwt = make_jwt_with_account("acct-bitrouter");
    {
        let mut store = CredentialStore::load(&path)?;
        store.set(
            PROVIDER_ID,
            DEFAULT_ACCOUNT,
            Credential::from_oauth_token(OAuthToken {
                access_token: jwt.clone(),
                expires_at: 0, // non-expiring → no refresh attempt
                refresh_token: Some("r".into()),
            }),
        )?;
    }
    let server = MockServer::start().await;
    let applier = codex_with_endpoint(
        &path,
        reqwest::Client::new(),
        "client-1",
        format!("{}/oauth/token", server.uri()),
    )?;
    let req = reqwest::Client::new()
        .post("https://chatgpt.com/backend-api/codex/responses")
        .build()?;
    let authed = applier.apply(req, &codex_target(None)).await?;
    let h = authed.headers();
    assert_eq!(
        h.get(reqwest::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some(format!("Bearer {jwt}").as_str())
    );
    assert_eq!(
        h.get("chatgpt-account-id").and_then(|v| v.to_str().ok()),
        Some("acct-bitrouter")
    );
    assert!(h.get("openai-beta").is_none());
    assert_eq!(
        h.get("x-openai-internal-codex-responses-lite")
            .and_then(|v| v.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        h.get("originator").and_then(|v| v.to_str().ok()),
        Some(headers::ORIGINATOR)
    );
    assert_eq!(
        h.get(reqwest::header::USER_AGENT)
            .and_then(|v| v.to_str().ok()),
        Some(headers::USER_AGENT)
    );
    Ok(())
}

#[tokio::test]
async fn fails_when_no_credential_stored()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = tmp_store_path()?;
    let applier = codex_applier(&path)?;
    let req = reqwest::Client::new()
        .post("https://chatgpt.com/backend-api/codex/responses")
        .build()?;
    let err = applier
        .apply(req, &codex_target(None))
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
async fn rejects_api_key_credential()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = tmp_store_path()?;
    {
        let mut store = CredentialStore::load(&path)?;
        store.set(PROVIDER_ID, DEFAULT_ACCOUNT, Credential::api_key("sk-..."))?;
    }
    let applier = codex_applier(&path)?;
    let req = reqwest::Client::new()
        .post("https://chatgpt.com/backend-api/codex/responses")
        .build()?;
    let err = applier
        .apply(req, &codex_target(None))
        .await
        .err()
        .ok_or("operation unexpectedly succeeded")?;
    let msg = err.to_string();
    assert!(
        msg.contains("subscription OAuth"),
        "expected API-key rejection, got: {msg}"
    );
    Ok(())
}

#[tokio::test]
async fn omits_account_id_header_when_jwt_lacks_claim()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = tmp_store_path()?;
    // Plain non-JWT string — claim decode fails gracefully and the
    // applier still sets the Bearer.
    {
        let mut store = CredentialStore::load(&path)?;
        store.set(
            PROVIDER_ID,
            DEFAULT_ACCOUNT,
            Credential::from_oauth_token(OAuthToken {
                access_token: "not-a-jwt".into(),
                expires_at: 0,
                refresh_token: None,
            }),
        )?;
    }
    let applier = codex_applier(&path)?;
    let req = reqwest::Client::new()
        .post("https://chatgpt.com/backend-api/codex/responses")
        .build()?;
    let authed = applier.apply(req, &codex_target(None)).await?;
    assert!(authed.headers().get("chatgpt-account-id").is_none());
    assert!(
        authed
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn unauthorized_recovery_rereads_rotated_disk_token_before_refresh() -> TestResult<()> {
    let path = tmp_store_path()?;
    {
        let mut store = CredentialStore::load(&path)?;
        store.set(
            PROVIDER_ID,
            DEFAULT_ACCOUNT,
            Credential::from_oauth_token(OAuthToken {
                access_token: "old-access".into(),
                expires_at: 0,
                refresh_token: Some("RT-old".into()),
            }),
        )?;
    }
    let applier = codex_with_endpoint(
        &path,
        reqwest::Client::new(),
        "client-1",
        "http://insecure.example.com/oauth/token",
    )?;

    let first = applier
        .apply(
            reqwest::Client::new()
                .post("https://chatgpt.com/backend-api/codex/responses")
                .build()?,
            &codex_target(None),
        )
        .await?;
    assert_eq!(
        first
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer old-access")
    );

    {
        let mut store = CredentialStore::load(&path)?;
        store.set(
            PROVIDER_ID,
            DEFAULT_ACCOUNT,
            Credential::from_oauth_token(OAuthToken {
                access_token: "new-access".into(),
                expires_at: 0,
                refresh_token: Some("RT-new".into()),
            }),
        )?;
    }

    assert!(
        applier
            .refresh_after_unauthorized(
                &codex_target(None),
                Some(&HeaderValue::from_static("Bearer old-access")),
            )
            .await?
    );
    let second = applier
        .apply(
            reqwest::Client::new()
                .post("https://chatgpt.com/backend-api/codex/responses")
                .build()?,
            &codex_target(None),
        )
        .await?;
    assert_eq!(
        second
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        Some("Bearer new-access")
    );
    Ok(())
}

#[tokio::test]
async fn unauthorized_recovery_refreshes_when_disk_token_matches_rejected_token() -> TestResult<()>
{
    let path = tmp_store_path()?;
    {
        let mut store = CredentialStore::load(&path)?;
        store.set(
            PROVIDER_ID,
            DEFAULT_ACCOUNT,
            Credential::from_oauth_token(OAuthToken {
                access_token: "old-access".into(),
                expires_at: 0,
                refresh_token: Some("RT-old".into()),
            }),
        )?;
    }
    let applier = codex_with_endpoint(
        &path,
        reqwest::Client::new(),
        "client-1",
        "http://insecure.example.com/oauth/token",
    )?;
    let err = applier
        .refresh_after_unauthorized(
            &codex_target(None),
            Some(&HeaderValue::from_static("Bearer old-access")),
        )
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
async fn prepare_body_forces_store_false_and_reasoning_include()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = tmp_store_path()?;
    let applier = codex_applier(&path)?;
    // include absent → created with the reasoning item; store forced false.
    // `max_output_tokens` is stripped because ChatGPT/Codex rejects it.
    let mut body = serde_json::json!({
        "model": "gpt-5-codex",
        "input": [],
        "tools": [
            {"type": "function", "name": "shell", "parameters": {}},
            {"type": "namespace", "name": "workspace", "tools": []},
            {"type": "web_search"}
        ],
        "reasoning": {"effort": "high", "context": "legacy"},
        "max_output_tokens": 16,
        "stream_options": {"include_usage": true}
    });
    applier.prepare_body(&mut body, &codex_target(None)).await?;
    assert!(body.get("max_output_tokens").is_none());
    assert!(body.get("stream_options").is_none());
    assert_eq!(body["store"], serde_json::json!(false));
    assert_eq!(
        body["include"],
        serde_json::json!(["reasoning.encrypted_content"])
    );
    assert_eq!(body["reasoning"]["context"], serde_json::json!("all_turns"));
    assert_eq!(body["reasoning"]["effort"], serde_json::json!("high"));
    assert_eq!(body["parallel_tool_calls"], serde_json::json!(false));
    assert!(body.get("tools").is_none());
    assert_eq!(
        body["input"][0]["type"],
        serde_json::json!("additional_tools")
    );
    assert_eq!(body["input"][0]["role"], serde_json::json!("developer"));
    assert_eq!(
        body["input"][0]["tools"].as_array().map(|tools| tools
            .iter()
            .filter_map(|tool| tool.get("type").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>()),
        Some(vec!["function", "namespace", "web_search"])
    );
    // include already present → reasoning item appended without duplication.
    let mut body2 = serde_json::json!({ "include": ["foo"] });
    applier
        .prepare_body(&mut body2, &codex_target(None))
        .await?;
    assert_eq!(
        body2["include"],
        serde_json::json!(["foo", "reasoning.encrypted_content"])
    );
    // Idempotent — a second pass doesn't re-append.
    applier
        .prepare_body(&mut body2, &codex_target(None))
        .await?;
    assert_eq!(
        body2["include"],
        serde_json::json!(["foo", "reasoning.encrypted_content"])
    );
    // No system prompt → instructions defaulted to the Codex fallback.
    assert_eq!(
        body["instructions"],
        serde_json::json!("You are a helpful assistant.")
    );
    // A caller-supplied instructions is preserved untouched.
    let mut body3 = serde_json::json!({ "instructions": "be a pirate", "input": [] });
    applier
        .prepare_body(&mut body3, &codex_target(None))
        .await?;
    assert_eq!(body3["instructions"], serde_json::json!("be a pirate"));

    let mut body4 = serde_json::json!({
        "instructions": "base instructions",
        "metadata": {"session_id": "claude-session"},
        "client_metadata": {"thread_id": "codex-thread"},
        "thinking": {"type": "adaptive"},
        "context_management": {"edits": []},
        "output_config": {"effort": "high"},
        "input": [
            {
                "type": "message",
                "role": "developer",
                "content": [{"type": "input_text", "text": "developer guidance"}]
            },
            {
                "type": "message",
                "role": "system",
                "content": "system guidance"
            },
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            }
        ]
    });
    applier
        .prepare_body(&mut body4, &codex_target(None))
        .await?;
    assert!(body4.get("metadata").is_none());
    assert!(body4.get("client_metadata").is_none());
    assert!(body4.get("thinking").is_none());
    assert!(body4.get("context_management").is_none());
    assert!(body4.get("output_config").is_none());
    assert_eq!(
        body4["input"],
        serde_json::json!([
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            }
        ])
    );
    let instructions = body4["instructions"].as_str().ok_or("missing text")?;
    assert!(instructions.contains("base instructions"));
    assert!(instructions.contains("developer guidance"));
    assert!(instructions.contains("system guidance"));

    let mut body5 = serde_json::json!({
        "input": [],
        "tools": [
            { "type": "custom" },
            { "type": "tool_search" },
            { "type": "function", "name": "read_file", "parameters": {} }
        ]
    });
    applier
        .prepare_body(&mut body5, &codex_target(None))
        .await?;
    let moved_tools = &body5["input"][0]["tools"];
    assert_eq!(moved_tools[0]["name"], serde_json::json!("custom"));
    assert_eq!(
        moved_tools[1]["description"],
        serde_json::json!("Search for available tools.")
    );
    assert_eq!(moved_tools[1]["parameters"], serde_json::json!({}));
    assert!(moved_tools[1].get("name").is_none());
    assert_eq!(moved_tools[2]["name"], serde_json::json!("read_file"));
    Ok(())
}
type AuthTestResult = std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[tokio::test]
async fn explicit_credential_takes_priority_without_reading_or_refreshing_store() -> AuthTestResult
{
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("credentials.json");
    std::fs::write(&path, b"invalid credential JSON")?;
    let applier = codex_applier(&path)?;
    let mut target = codex_target(Some("selected"));
    target.api_key = make_jwt_with_account("explicit-account");
    let request = reqwest::Client::new()
        .post("https://example.invalid/responses")
        .build()?;
    let applied = applier.apply_with_authority(request, &target).await?;
    assert_eq!(
        applied
            .headers()
            .get("chatgpt-account-id")
            .and_then(|v| v.to_str().ok()),
        Some("explicit-account")
    );
    assert!(applied.into_parts().1.is_some());
    assert!(
        !applier
            .refresh_after_unauthorized(&target, Some(&HeaderValue::from_static("Bearer rejected")))
            .await?
    );
    Ok(())
}

#[tokio::test]
async fn fresh_calls_observe_login_replacement_and_logout_without_a_401() -> AuthTestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("credentials.json");
    let mut store = CredentialStore::load(&path)?;
    let credential = |account| {
        Credential::Oauth(OAuthToken {
            access_token: make_jwt_with_account(account),
            expires_at: 0,
            refresh_token: None,
        })
    };
    store.set(PROVIDER_ID, "selected", credential("first-account"))?;
    store.set(PROVIDER_ID, DEFAULT_ACCOUNT, credential("other-account"))?;
    let applier = codex_applier(&path)?;
    let target = codex_target(Some("selected"));
    let first = applier.continuation_authority(&target).await?;
    store.set(PROVIDER_ID, "selected", credential("new-login"))?;
    let second = applier.continuation_authority(&target).await?;
    assert_ne!(first, second);
    store.remove(PROVIDER_ID, "selected")?;
    assert!(matches!(
        applier.continuation_authority(&target).await,
        Err(ModelError::Provider { status: 401, .. })
    ));
    assert!(store.get_any(PROVIDER_ID, DEFAULT_ACCOUNT).is_some());
    Ok(())
}

struct HeldCodexRefresh {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl OAuthRefresher for HeldCodexRefresh {
    async fn refresh(&self, _current: &OAuthToken) -> bitrouter_ai::error::Result<OAuthToken> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
        Ok(OAuthToken {
            access_token: make_jwt_with_account("stable-account"),
            expires_at: 0,
            refresh_token: Some("rotated-refresh".into()),
        })
    }
}

#[tokio::test]
async fn dropped_applier_call_commits_rotation_for_another_instance() -> AuthTestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("credentials.json");
    let mut store = CredentialStore::load(&path)?;
    store.set(
        PROVIDER_ID,
        "selected",
        Credential::Oauth(OAuthToken {
            access_token: "expired-access".into(),
            expires_at: 1,
            refresh_token: Some("old-refresh".into()),
        }),
    )?;
    let refresh = Arc::new(HeldCodexRefresh {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let first = codex_with_refresher(Arc::new(FileCredentialStore::new(&path)?), refresh.clone());
    let second = codex_with_refresher(Arc::new(FileCredentialStore::new(&path)?), refresh.clone());
    let call = tokio::spawn(async move {
        first
            .continuation_authority(&codex_target(Some("selected")))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), refresh.entered.notified()).await?;
    call.abort();
    assert!(call.await.is_err_and(|error| error.is_cancelled()));
    refresh.release.notify_one();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(5),
            second.continuation_authority(&codex_target(Some("selected")))
        )
        .await??
        .is_some()
    );
    assert_eq!(refresh.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let stored = CredentialStore::load(&path)?;
    assert_eq!(
        stored
            .get_any(PROVIDER_ID, "selected")
            .and_then(Credential::as_oauth)
            .and_then(|t| t.refresh_token.as_deref()),
        Some("rotated-refresh")
    );
    Ok(())
}

#[tokio::test]
async fn ai_client_uses_injected_codex_account_and_shapes_stream_request() -> AuthTestResult {
    use bitrouter_ai::auth::AuthAppliers;
    use bitrouter_ai::auth::store::MemoryCredentialStore;
    use bitrouter_ai::client::{HttpTimeouts, ModelClient};
    use bitrouter_ai::protocol::{InboundAdapter, responses::ResponsesAdapter};
    use futures::StreamExt;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, ResponseTemplate};
    let server = MockServer::start().await;
    let token = make_jwt_with_account("injected-account");
    Mock::given(method("POST")).and(path("/responses"))
            .and(header("authorization", format!("Bearer {token}")))
            .and(header("chatgpt-account-id", "injected-account"))
            .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(concat!(
                "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_injected\"}}\n\n",
                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_injected\",\"status\":\"completed\",\"output\":[]}}\n\n"
            ))).expect(1).mount(&server).await;
    let mut target = codex_target(Some("selected"));
    target.api_base = server.uri();
    let store = MemoryCredentialStore::new([(
        bitrouter_ai::auth::store::CredentialKey {
            provider: PROVIDER_ID.into(),
            account: target
                .account_label
                .as_deref()
                .unwrap_or(DEFAULT_ACCOUNT)
                .into(),
        },
        Credential::Oauth(OAuthToken {
            access_token: token,
            expires_at: 0,
            refresh_token: None,
        }),
    )]);
    let applier = codex_with_refresher(
        Arc::new(store),
        Arc::new(RefreshGrant::new(
            reqwest::Client::new(),
            "https://auth.openai.com/oauth/token",
            "fixture-client",
        )),
    );
    let client = ModelClient::new(HttpTimeouts::default())?
        .with_auth_appliers(AuthAppliers::new().with(PROVIDER_ID, Arc::new(applier)));
    let prompt =
        ResponsesAdapter.parse_request(serde_json::json!({"model":"source","input":"hello"}))?;
    let stream = client
        .stream(
            &target,
            &prompt,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await?;
    let parts = stream.collect::<Vec<_>>().await;
    assert!(parts.iter().all(std::result::Result::is_ok));
    assert!(parts.iter().any(|part| {
        part.as_ref()
            .is_ok_and(bitrouter_ai::types::StreamPart::is_terminal)
    }));
    let requests = server
        .received_requests()
        .await
        .ok_or("missing model request")?;
    let request = requests.first().ok_or("missing captured model request")?;
    let body: serde_json::Value = serde_json::from_slice(&request.body)?;
    assert_eq!(body["model"], target.service_id);
    assert_eq!(body["store"], false);
    assert_eq!(body["include"][0], "reasoning.encrypted_content");
    Ok(())
}
