//! Anthropic/Claude calls using only AI, injected memory stores and local HTTP.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bitrouter_ai::auth::credentials::{Credential, OAuthToken};
use bitrouter_ai::auth::oauth::{REFRESH_WINDOW, RefreshGrant};
use bitrouter_ai::auth::store::{
    CredentialKey, CredentialStore, MemoryCredentialStore, OAuthSession,
};
use bitrouter_ai::auth::{AuthApplier, AuthAppliers};
use bitrouter_ai::client::{HttpTimeouts, ModelClient};
use bitrouter_ai::error::ModelError;
use bitrouter_ai::protocol::{InboundAdapter, messages::MessagesAdapter};
use bitrouter_ai::providers::anthropic::{AnthropicApiKeyApplier, headers};
use bitrouter_ai::providers::claude_code::ClaudeCodeAuthApplier;
use bitrouter_ai::target::{CredentialPriority, ModelTarget};
use bitrouter_ai::types::{ApiProtocol, Content, Prompt, StreamPart};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn target(provider: &str, base: &str) -> ModelTarget {
    ModelTarget {
        provider_name: provider.into(),
        service_id: "fixture-model".into(),
        api_base: base.into(),
        api_key: String::new(),
        credential_priority: Default::default(),
        account_label: Some("selected".into()),
        api_protocol: ApiProtocol::Messages,
        auth_scheme: Default::default(),
        compatibility: Default::default(),
    }
}

fn prompt() -> bitrouter_ai::error::Result<Prompt> {
    MessagesAdapter.parse_request(json!({"model":"fixture-model","max_tokens":64,"system":"caller instruction","messages":[{"role":"user","content":"hello"}]}))
}

fn key(provider: &str, account: &str) -> CredentialKey {
    CredentialKey {
        provider: provider.into(),
        account: account.into(),
    }
}

fn token(access: &str, expiry: u64, refresh: Option<&str>) -> OAuthToken {
    OAuthToken {
        access_token: access.into(),
        expires_at: expiry,
        refresh_token: refresh.map(String::from),
    }
}

fn model_reply(request: &wiremock::Request) -> ResponseTemplate {
    let body = serde_json::from_slice::<Value>(&request.body).ok();
    if body.as_ref().is_some_and(|body| body["stream"] == true) {
        ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"native-message\",\"usage\":{\"input_tokens\":2,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ))
    } else {
        ResponseTemplate::new(200).set_body_json(json!({"id":"native-message","type":"message","role":"assistant","model":"fixture-model","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":1}}))
    }
}

#[tokio::test]
async fn platform_key_invokes_json_and_sse_without_subscription_shaping() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .and(header("x-api-key", "selected-key"))
        .and(header("anthropic-version", headers::ANTHROPIC_VERSION))
        .respond_with(model_reply)
        .expect(2)
        .mount(&server)
        .await;
    let store = Arc::new(MemoryCredentialStore::new([(
        key("anthropic", "selected"),
        Credential::api_key("selected-key"),
    )]));
    let client = ModelClient::new(HttpTimeouts::default())?.with_auth_appliers(
        AuthAppliers::new().with("anthropic", Arc::new(AnthropicApiKeyApplier::new(store))),
    );
    let selected = target("anthropic", &server.uri());
    let source = prompt()?;
    let original = source.clone();
    let result = client
        .generate(&selected, &source, &CancellationToken::new())
        .await?;
    assert!(matches!(result.content.as_slice(),[Content::Text {text,..}] if text == "done"));
    let parts = client
        .stream(&selected, &source, &CancellationToken::new())
        .await?
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    assert!(parts.iter().any(StreamPart::is_terminal));
    assert_eq!(source, original);
    for request in server
        .received_requests()
        .await
        .ok_or("missing native requests")?
    {
        assert!(!request.headers.contains_key("authorization"));
        assert!(!request.headers.contains_key("x-app"));
        let body: Value = serde_json::from_slice(&request.body)?;
        assert_eq!(body["system"], "caller instruction");
    }
    Ok(())
}

#[tokio::test]
async fn subscription_refreshes_selected_account_recovers_once_and_keeps_source() -> TestResult {
    let server = MockServer::start().await;
    let count = Arc::new(AtomicUsize::new(0));
    let refresh_count = count.clone();
    Mock::given(method("POST")).and(path("/token")).respond_with(move |_: &wiremock::Request| {
        let attempt = refresh_count.fetch_add(1,Ordering::SeqCst) + 1;
        ResponseTemplate::new(200).set_body_json(json!({"access_token":format!("access-{attempt}"),"refresh_token":format!("refresh-{attempt}"),"expires_in":3600}))
    }).expect(2).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .and(header("authorization", "Bearer access-1"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"error":"first token rejected"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .and(header("authorization", "Bearer access-2"))
        .and(header("user-agent", headers::CLAUDE_CODE_USER_AGENT))
        .and(header("x-app", headers::CLAUDE_CODE_X_APP))
        .respond_with(model_reply)
        .expect(2)
        .mount(&server)
        .await;
    let unrelated = Credential::api_key("other-account");
    let store = Arc::new(MemoryCredentialStore::new([
        (
            key("claude-code", "selected"),
            Credential::Oauth(token("expired", 1, Some("old-refresh"))),
        ),
        (key("claude-code", "other"), unrelated.clone()),
    ]));
    let session = OAuthSession::new(
        store.clone(),
        Arc::new(RefreshGrant::new(
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
            "explicit-client",
        )),
        REFRESH_WINDOW,
    );
    let client = ModelClient::new(HttpTimeouts::default())?.with_auth_appliers(
        AuthAppliers::new().with("claude-code", Arc::new(ClaudeCodeAuthApplier::new(session))),
    );
    let selected = target("claude-code", &server.uri());
    let source = prompt()?;
    let original = source.clone();
    let result = client
        .generate(&selected, &source, &CancellationToken::new())
        .await?;
    assert!(matches!(result.content.as_slice(),[Content::Text {text,..}] if text == "done"));
    let parts = client
        .stream(&selected, &source, &CancellationToken::new())
        .await?
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    assert!(parts.iter().any(StreamPart::is_terminal));
    assert_eq!(source, original);
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert_eq!(
        store
            .begin(&key("claude-code", "other"))
            .await?
            .credential(),
        Some(&unrelated)
    );
    let stored = store.begin(&key("claude-code", "selected")).await?;
    assert_eq!(
        stored
            .credential()
            .and_then(Credential::as_oauth)
            .and_then(|token| token.refresh_token.as_deref()),
        Some("refresh-2")
    );
    let requests = server
        .received_requests()
        .await
        .ok_or("missing subscription requests")?;
    let refreshes = requests
        .iter()
        .filter(|request| request.url.path() == "/token")
        .collect::<Vec<_>>();
    assert_eq!(refreshes.len(), 2);
    assert!(std::str::from_utf8(&refreshes[1].body)?.contains("refresh_token=refresh-1"));
    for request in requests
        .iter()
        .filter(|request| request.url.path() == "/messages")
    {
        assert!(!request.headers.contains_key("x-api-key"));
        let beta = request
            .headers
            .get("anthropic-beta")
            .ok_or("missing beta")?
            .to_str()?;
        assert!(beta.contains("oauth-2025-04-20"));
        let body: Value = serde_json::from_slice(&request.body)?;
        assert_eq!(
            body["system"][0]["text"],
            headers::CLAUDE_AGENT_SYSTEM_PROMPT
        );
        assert_eq!(body["system"][1]["text"], "caller instruction");
        assert_eq!(
            body["system"]
                .as_array()
                .ok_or("missing shaped system")?
                .len(),
            2
        );
    }
    Ok(())
}

#[tokio::test]
async fn invalid_selected_credentials_block_fallback_but_explicit_keys_win() -> TestResult {
    for provider in ["anthropic", "claude-code"] {
        let wrong = if provider == "anthropic" {
            Credential::Oauth(token("wrong", 0, None))
        } else {
            Credential::api_key("wrong")
        };
        let store = Arc::new(MemoryCredentialStore::new([(
            key(provider, "selected"),
            wrong,
        )]));
        let applier: Arc<dyn AuthApplier> = if provider == "anthropic" {
            Arc::new(AnthropicApiKeyApplier::new(store))
        } else {
            Arc::new(
                ClaudeCodeAuthApplier::new(OAuthSession::new(
                    store,
                    Arc::new(RefreshGrant::new(
                        reqwest::Client::new(),
                        "http://127.0.0.1:9/token",
                        "explicit-client",
                    )),
                    REFRESH_WINDOW,
                ))
                .with_fallback_token(Some(token("ambient", 0, None))),
            )
        };
        let mut selected = target(provider, "https://fixture.invalid");
        selected.api_key = "fallback".into();
        selected.credential_priority = CredentialPriority::Fallback;
        let request = || {
            reqwest::Client::new()
                .post("https://fixture.invalid/messages")
                .build()
        };
        assert!(matches!(
            applier.apply(request()?, &selected).await,
            Err(ModelError::Provider { status: 401, .. })
        ));
        selected.api_key = "explicit".into();
        selected.credential_priority = CredentialPriority::Explicit;
        let applied = applier.apply(request()?, &selected).await?;
        let (name, expected) = if provider == "anthropic" {
            ("x-api-key", "explicit")
        } else {
            ("authorization", "Bearer explicit")
        };
        assert_eq!(
            applied
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok()),
            Some(expected)
        );
    }
    Ok(())
}
