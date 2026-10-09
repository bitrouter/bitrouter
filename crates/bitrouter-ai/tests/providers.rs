//! Native provider calls with only AI, injected memory storage and local HTTP.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use bitrouter_ai::auth::oauth::{REFRESH_WINDOW, RefreshGrant, parse_token_reply};
use bitrouter_ai::auth::store::{
    CredentialKey, CredentialStore, MemoryCredentialStore, OAuthSession,
};
use bitrouter_ai::auth::{
    AuthAppliers,
    credentials::{Credential, OAuthToken},
};
use bitrouter_ai::client::{HttpTimeouts, ModelClient};
use bitrouter_ai::protocol::{InboundAdapter, chat_completions::ChatCompletionsAdapter};
use bitrouter_ai::providers::{
    codex::OpenAiCodexAuthApplier, copilot::CopilotAuthApplier, supergrok::SuperGrokAuthApplier,
};
use bitrouter_ai::target::ModelTarget;
use bitrouter_ai::types::ApiProtocol;
use futures::StreamExt;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn target(provider: &str, base: String, protocol: ApiProtocol) -> ModelTarget {
    ModelTarget {
        provider_name: provider.into(),
        service_id: "fixture-model".into(),
        api_base: base,
        api_key: String::new(),
        credential_priority: Default::default(),
        account_label: Some("selected".into()),
        api_protocol: protocol,
        auth_scheme: Default::default(),
        compatibility: Default::default(),
    }
}

#[tokio::test]
async fn native_subscription_streams_refresh_and_commit_using_only_ai() -> TestResult {
    for provider in ["openai-codex", "supergrok"] {
        let server = MockServer::start().await;
        let token = format!("header.{}.signature",URL_SAFE_NO_PAD.encode(r#"{"sub":"subject","https://api.openai.com/auth":{"chatgpt_account_id":"account"}}"#));
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("client_id=explicit-client"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"access_token":token,"expires_in":3600,"refresh_token":"rotated-refresh"}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST")).and(path("/responses"))
            .and(header("authorization",format!("Bearer {token}")))
            .respond_with(ResponseTemplate::new(200).insert_header("content-type","text/event-stream")
                .set_body_string(concat!(
                    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"native-response\",\"status\":\"in_progress\",\"output\":[]}}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"native-response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":2,\"output_tokens\":0,\"total_tokens\":2}}}\n\n")))
            .expect(if provider == "openai-codex" { 2 } else { 1 }).mount(&server).await;
        let key = CredentialKey {
            provider: provider.into(),
            account: "selected".into(),
        };
        let store = Arc::new(MemoryCredentialStore::new([(
            key.clone(),
            Credential::Oauth(OAuthToken {
                access_token: "expired".into(),
                expires_at: 1,
                refresh_token: Some("old-refresh".into()),
            }),
        )]));
        let session = OAuthSession::new(
            store.clone(),
            Arc::new(RefreshGrant::new(
                reqwest::Client::new(),
                format!("{}/token", server.uri()),
                "explicit-client",
            )),
            REFRESH_WINDOW,
        );
        let mut auth = AuthAppliers::new();
        if provider == "openai-codex" {
            auth.register(provider, Arc::new(OpenAiCodexAuthApplier::new(session)));
        } else {
            auth.register(provider, Arc::new(SuperGrokAuthApplier::new(session)));
        }
        let client = ModelClient::new(HttpTimeouts::default())?.with_auth_appliers(auth);
        let prompt = ChatCompletionsAdapter.parse_request(
            json!({"model":"fixture-model","messages":[{"role":"user","content":"hello"}]}),
        )?;
        let result = client
            .stream(
                &target(provider, server.uri(), ApiProtocol::Responses),
                &prompt,
                &CancellationToken::new(),
            )
            .await?;
        let parts = result.collect::<Vec<_>>().await;
        assert!(parts.iter().all(Result::is_ok));
        assert!(parts.iter().any(|part| {
            part.as_ref()
                .is_ok_and(bitrouter_ai::types::StreamPart::is_terminal)
        }));
        if provider == "openai-codex" {
            let result = client
                .generate(
                    &target(provider, server.uri(), ApiProtocol::Responses),
                    &prompt,
                    &CancellationToken::new(),
                )
                .await?;
            assert_eq!(result.response_id.as_deref(), Some("native-response"));
            assert_eq!(result.usage.ok_or("missing native usage")?.prompt_tokens, 2);
        }
        let stored = store.begin(&key).await?;
        assert_eq!(
            stored
                .credential()
                .and_then(Credential::as_oauth)
                .and_then(|token| token.refresh_token.as_deref()),
            Some("rotated-refresh")
        );
        if provider == "openai-codex" {
            let requests = server.received_requests().await.ok_or("missing requests")?;
            let model = requests
                .iter()
                .find(|request| request.url.path() == "/responses")
                .ok_or("missing model request")?;
            let body: serde_json::Value = serde_json::from_slice(&model.body)?;
            assert_eq!(body["store"], false);
            assert_eq!(body["include"][0], "reasoning.encrypted_content");
        }
    }
    Ok(())
}

#[tokio::test]
async fn copilot_recovers_once_without_overwriting_the_selected_github_credential() -> TestResult {
    let server = MockServer::start().await;
    let expires_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + 3600;
    let exchanges = Arc::new(AtomicUsize::new(0));
    let counter = exchanges.clone();
    Mock::given(method("GET"))
        .and(path("/exchange"))
        .and(header("authorization", "token selected-github"))
        .respond_with(move |_: &wiremock::Request| {
            let token = if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                "first"
            } else {
                "second"
            };
            ResponseTemplate::new(200).set_body_json(json!({"token":token,"expires_at":expires_at}))
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer first"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error":"expired"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST")).and(path("/chat/completions")).and(header("authorization","Bearer second"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"copilot-result","object":"chat.completion","model":"fixture-model","choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}})))
        .expect(1).mount(&server).await;
    let key = CredentialKey {
        provider: "github-copilot".into(),
        account: "selected".into(),
    };
    let github = Credential::Oauth(OAuthToken {
        access_token: "selected-github".into(),
        expires_at: 0,
        refresh_token: None,
    });
    let store = Arc::new(MemoryCredentialStore::new([(key.clone(), github.clone())]));
    let applier = CopilotAuthApplier::new(
        reqwest::Client::new(),
        format!("{}/exchange", server.uri()),
        store.clone(),
    );
    let client = ModelClient::new(HttpTimeouts::default())?
        .with_auth_appliers(AuthAppliers::new().with("github-copilot", Arc::new(applier)));
    let prompt = ChatCompletionsAdapter.parse_request(
        json!({"model":"fixture-model","messages":[{"role":"user","content":"hello"}]}),
    )?;
    client
        .generate(
            &target("github-copilot", server.uri(), ApiProtocol::ChatCompletions),
            &prompt,
            &CancellationToken::new(),
        )
        .await?;
    assert_eq!(exchanges.load(Ordering::SeqCst), 2);
    assert_eq!(store.begin(&key).await?.credential(), Some(&github));
    Ok(())
}

#[tokio::test]
async fn token_endpoint_diagnostics_never_include_payload_or_opaque_error_text() -> TestResult {
    let server = MockServer::start().await;
    for body in [
        json!({"error":"opaque-secret-error","error_description":"secret-refresh-token"}),
        json!({"error":"invalid_client","error_description":"secret-access-token"}),
    ] {
        server.reset().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(body))
            .mount(&server)
            .await;
        let response = reqwest::Client::new()
            .post(format!("{}/token", server.uri()))
            .send()
            .await?;
        let error = parse_token_reply(response, &format!("{}/token", server.uri()))
            .await
            .err()
            .ok_or("failed exchange succeeded")?;
        assert!(!format!("{error:?} {error}").contains("secret"));
    }
    let token = bitrouter_ai::providers::copilot::exchange::CopilotToken {
        token: "copilot-secret-bearer".into(),
        expires_at: 1,
    };
    assert!(!format!("{token:?}").contains("secret"));
    Ok(())
}
