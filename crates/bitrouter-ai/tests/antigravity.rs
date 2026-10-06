//! AI-only Google calls with explicit client metadata/secrets and local HTTP.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bitrouter_ai::auth::credentials::{Credential, OAuthToken};
use bitrouter_ai::auth::oauth::REFRESH_WINDOW;
use bitrouter_ai::auth::store::{
    CredentialKey, CredentialStore, MemoryCredentialStore, OAuthRefresher, OAuthSession,
};
use bitrouter_ai::auth::{AuthApplier, AuthAppliers};
use bitrouter_ai::client::{HttpTimeouts, ModelClient};
use bitrouter_ai::error::ModelError;
use bitrouter_ai::protocol::{
    InboundAdapter, OutboundDispatch, chat_completions::ChatCompletionsAdapter,
};
use bitrouter_ai::providers::antigravity::{
    AntigravityAuthApplier, PROVIDER_ID, protocol, refresh::AntigravityRefresher,
};
use bitrouter_ai::target::{CredentialPriority, ModelTarget};
use bitrouter_ai::types::{ApiProtocol, Content, Prompt, StreamPart};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_string_contains, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const DEADLINE: Duration = Duration::from_secs(5);

fn key(account: &str) -> CredentialKey {
    CredentialKey {
        provider: PROVIDER_ID.into(),
        account: account.into(),
    }
}
fn target(base: &str) -> ModelTarget {
    ModelTarget {
        provider_name: PROVIDER_ID.into(),
        service_id: "selected-service".into(),
        api_base: base.into(),
        api_key: String::new(),
        credential_priority: Default::default(),
        account_label: Some("selected".into()),
        api_protocol: ApiProtocol::Custom(protocol::PROTOCOL.into()),
        auth_scheme: Default::default(),
        compatibility: Default::default(),
    }
}
fn prompt() -> bitrouter_ai::error::Result<Prompt> {
    ChatCompletionsAdapter.parse_request(
        json!({"model":"source-model","messages":[{"role":"user","content":"hello"}]}),
    )
}
fn expired() -> OAuthToken {
    OAuthToken {
        access_token: "expired".into(),
        expires_at: 1,
        refresh_token: Some("old-refresh".into()),
    }
}
fn reply() -> Value {
    json!({"response":{"responseId":"google-result","candidates":[{"content":{"role":"model","parts":[{"text":"done"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":1}}})
}
fn client(applier: AntigravityAuthApplier) -> bitrouter_ai::error::Result<ModelClient> {
    let mut dispatch = OutboundDispatch::builtin();
    protocol::register(&mut dispatch);
    Ok(
        ModelClient::with_dispatch(HttpTimeouts::default(), Arc::new(dispatch))?
            .with_auth_appliers(AuthAppliers::new().with(PROVIDER_ID, Arc::new(applier))),
    )
}
fn applier(
    store: Arc<dyn CredentialStore>,
    refresher: Arc<dyn OAuthRefresher>,
) -> AntigravityAuthApplier {
    AntigravityAuthApplier::new(
        OAuthSession::new(store, refresher, REFRESH_WINDOW),
        reqwest::Client::new(),
    )
}

#[tokio::test]
async fn google_json_and_sse_probe_secrets_recover_once_and_bind_each_origin() -> TestResult {
    let server = MockServer::start().await;
    let other = MockServer::start().await;
    let sources = Arc::new(AtomicUsize::new(0));
    let source_count = sources.clone();
    let rotations = Arc::new(AtomicUsize::new(0));
    let rotate = rotations.clone();
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("client_secret=bad-secret"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error":"invalid_client"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST")).and(path("/token")).and(body_string_contains("client_secret=working-secret"))
        .and(body_string_contains("client_id=explicit-client"))
        .respond_with(move |_: &wiremock::Request| {
            let count = rotate.fetch_add(1,Ordering::SeqCst)+1;
            ResponseTemplate::new(200).set_body_json(json!({"access_token":format!("access-{count}"),"refresh_token":format!("refresh-{count}"),"expires_in":3600}))
        }).expect(2).mount(&server).await;
    for access in ["access-1", "access-2"] {
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .and(header("authorization", format!("Bearer {access}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"cloudaicompanionProject":format!("project-{access}")})),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/v1internal:generateContent"))
        .and(header("authorization", "Bearer access-1"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1internal:generateContent"))
        .and(header("authorization", "Bearer access-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1internal:streamGenerateContent"))
        .and(query_param("alt", "sse"))
        .and(header("authorization", "Bearer access-2"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(format!("data: {}\n\n", reply())),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1internal:loadCodeAssist"))
        .and(header("authorization", "Bearer access-2"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"cloudaicompanionProject":"other-project"})),
        )
        .expect(1)
        .mount(&other)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1internal:generateContent"))
        .and(header("authorization", "Bearer access-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply()))
        .expect(1)
        .mount(&other)
        .await;
    let unrelated = Credential::api_key("other-account");
    let store = Arc::new(MemoryCredentialStore::new([
        (key("selected"), Credential::Oauth(expired())),
        (key("other"), unrelated.clone()),
    ]));
    let refresher = Arc::new(AntigravityRefresher::new(
        reqwest::Client::new(),
        format!("{}/token", server.uri()),
        "explicit-client",
        Arc::new(move || {
            source_count.fetch_add(1, Ordering::SeqCst);
            Ok(vec!["bad-secret".into(), "working-secret".into()])
        }),
    ));
    let client = client(applier(store.clone(), refresher))?;
    let selected = target(&server.uri());
    let source = prompt()?;
    let original = source.clone();
    let result = client
        .generate(&selected, &source, &CancellationToken::new())
        .await?;
    assert!(matches!(result.content.as_slice(),[Content::Text { text,.. }] if text == "done"));
    assert_eq!(result.usage.ok_or("missing usage")?.prompt_tokens, 2);
    let parts = client
        .stream(&selected, &source, &CancellationToken::new())
        .await?
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    assert!(parts.iter().any(StreamPart::is_terminal));
    client
        .generate(&target(&other.uri()), &source, &CancellationToken::new())
        .await?;
    assert_eq!(source, original);
    assert_eq!(sources.load(Ordering::SeqCst), 1);
    assert_eq!(rotations.load(Ordering::SeqCst), 2);
    assert_eq!(
        store.begin(&key("other")).await?.credential(),
        Some(&unrelated)
    );
    assert_eq!(
        store
            .begin(&key("selected"))
            .await?
            .credential()
            .and_then(Credential::as_oauth)
            .and_then(|token| token.refresh_token.as_deref()),
        Some("refresh-2")
    );
    for (origin, foreign) in [(&server, false), (&other, true)] {
        let requests = origin
            .received_requests()
            .await
            .ok_or("missing Google requests")?;
        for request in requests.iter().filter(|request| {
            request.url.path().contains("GenerateContent")
                || request.url.path().ends_with("generateContent")
        }) {
            let bearer = request
                .headers
                .get("authorization")
                .ok_or("missing bearer")?
                .to_str()?;
            let body: Value = serde_json::from_slice(&request.body)?;
            let access = bearer.strip_prefix("Bearer ").ok_or("not a bearer")?;
            assert_eq!(
                body["project"],
                if foreign {
                    "other-project".into()
                } else {
                    format!("project-{access}")
                }
            );
            assert_eq!(body["model"], "selected-service");
            assert_eq!(body["request"]["contents"][0]["parts"][0]["text"], "hello");
            assert!(!request.headers.contains_key("x-goog-api-key"));
            assert!(
                request
                    .headers
                    .get("user-agent")
                    .ok_or("missing UA")?
                    .to_str()?
                    .starts_with("antigravity/")
            );
            assert!(request.headers.contains_key("client-metadata"));
        }
    }
    let requests = server
        .received_requests()
        .await
        .ok_or("missing token requests")?;
    let grants = requests
        .iter()
        .filter(|request| request.url.path() == "/token")
        .collect::<Vec<_>>();
    assert_eq!(grants.len(), 3);
    assert!(std::str::from_utf8(&grants[2].body)?.contains("refresh_token=refresh-1"));
    Ok(())
}

#[tokio::test]
async fn refresh_aborts_other_failures_and_diagnostics_omit_secret_payloads() -> TestResult {
    for (template, status) in [
        (
            ResponseTemplate::new(400).set_body_json(
                json!({"error":"invalid_grant","error_description":"opaque-response-secret"}),
            ),
            401,
        ),
        (
            ResponseTemplate::new(400).set_body_json(json!({"error":"opaque-response-secret"})),
            401,
        ),
        (
            ResponseTemplate::new(200).set_body_string("opaque-response-secret"),
            502,
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("client_secret=first-secret"))
            .respond_with(template)
            .expect(1)
            .mount(&server)
            .await;
        let refresh = AntigravityRefresher::new(
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
            "explicit-client",
            Arc::new(|| Ok(vec!["first-secret".into(), "second-secret".into()])),
        );
        let error = refresh
            .refresh(&expired())
            .await
            .err()
            .ok_or("refresh unexpectedly succeeded")?;
        assert!(matches!(&error,ModelError::Provider {status:actual,..} if *actual == status));
        let diagnostic = format!("{error} {error:?}");
        for secret in [
            "opaque-response-secret",
            "first-secret",
            "second-secret",
            "old-refresh",
        ] {
            assert!(!diagnostic.contains(secret));
        }
    }
    let server = MockServer::start().await;
    for candidates in [Vec::new(), vec!["first-secret".into()]] {
        let refresher = AntigravityRefresher::new(
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
            "explicit-client",
            Arc::new(move || Ok(candidates.clone())),
        );
        let mut token = expired();
        token.refresh_token = None;
        assert!(matches!(
            refresher.refresh(&token).await,
            Err(ModelError::Provider { status: 401, .. })
        ));
    }
    assert!(
        server
            .received_requests()
            .await
            .ok_or("missing request inventory")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn bootstrap_errors_keep_http_status_without_reflecting_the_response() -> TestResult {
    for (status, expected) in [(429, 429), (200, 502)] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(ResponseTemplate::new(status).set_body_string("opaque-bootstrap-secret"))
            .expect(1)
            .mount(&server)
            .await;
        let refresh = Arc::new(AntigravityRefresher::new(
            reqwest::Client::new(),
            "https://fixture.invalid/token",
            "explicit-client",
            Arc::new(|| Err(ModelError::configuration("secret source must not run"))),
        ));
        let client = client(applier(Arc::new(MemoryCredentialStore::default()), refresh))?;
        let mut selected = target(&server.uri());
        selected.api_key = "explicit-bearer".into();
        let error = client
            .generate(&selected, &prompt()?, &CancellationToken::new())
            .await
            .err()
            .ok_or("bootstrap unexpectedly succeeded")?;
        assert!(matches!(&error,ModelError::Provider {status,..} if *status == expected));
        assert!(!format!("{error} {error:?}").contains("opaque-bootstrap-secret"));
        assert_eq!(
            server
                .received_requests()
                .await
                .ok_or("missing request inventory")?
                .len(),
            1
        );
    }
    Ok(())
}

#[tokio::test]
async fn invalid_slot_blocks_fallback_and_explicit_credentials_skip_secret_discovery() -> TestResult
{
    let sources = Arc::new(AtomicUsize::new(0));
    let source = sources.clone();
    let refresher = Arc::new(AntigravityRefresher::new(
        reqwest::Client::new(),
        "https://fixture.invalid/token",
        "explicit-client",
        Arc::new(move || {
            source.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }),
    ));
    let store = Arc::new(MemoryCredentialStore::new([(
        key("selected"),
        Credential::api_key("wrong-kind"),
    )]));
    let applier = applier(store, refresher);
    let mut selected = target("https://fixture.invalid");
    selected.api_key = "permitted-fallback".into();
    selected.credential_priority = CredentialPriority::Fallback;
    let request = || {
        reqwest::Client::new()
            .post("https://fixture.invalid/model")
            .build()
    };
    assert!(matches!(
        applier.apply(request()?, &selected).await,
        Err(ModelError::Provider { status: 401, .. })
    ));
    selected.credential_priority = CredentialPriority::Explicit;
    let applied = applier.apply(request()?, &selected).await?;
    assert_eq!(
        applied
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer permitted-fallback")
    );
    assert_eq!(sources.load(Ordering::SeqCst), 0);
    Ok(())
}

struct HeldRefresh {
    inner: AntigravityRefresher,
    entered: Notify,
    release: Notify,
}

#[async_trait::async_trait]
impl OAuthRefresher for HeldRefresh {
    async fn refresh(&self, token: &OAuthToken) -> bitrouter_ai::error::Result<OAuthToken> {
        let rotated = self.inner.refresh(token).await?;
        self.entered.notify_one();
        self.release.notified().await;
        Ok(rotated)
    }
}

#[tokio::test]
async fn dropping_generate_during_refresh_still_commits_for_the_next_call() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600})))
        .expect(1).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/v1internal:loadCodeAssist"))
        .and(header("authorization", "Bearer rotated-access"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"cloudaicompanionProject":"rotated-project"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1internal:generateContent"))
        .and(header("authorization", "Bearer rotated-access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply()))
        .expect(1)
        .mount(&server)
        .await;
    let store = Arc::new(MemoryCredentialStore::new([(
        key("selected"),
        Credential::Oauth(expired()),
    )]));
    let refresher = Arc::new(HeldRefresh {
        inner: AntigravityRefresher::new(
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
            "explicit-client",
            Arc::new(|| Ok(vec!["fixture-secret".into()])),
        ),
        entered: Notify::new(),
        release: Notify::new(),
    });
    let client = client(applier(store.clone(), refresher.clone()))?;
    let selected = target(&server.uri());
    let source = prompt()?;
    let cancellation = CancellationToken::new();
    let mut call = Box::pin(client.generate(&selected, &source, &cancellation));
    tokio::select! {
        result=&mut call=>return Err(format!("call completed before held refresh: {result:?}").into()),
        started=tokio::time::timeout(DEADLINE,refresher.entered.notified())=>{started?;}
    }
    drop(call);
    refresher.release.notify_one();
    tokio::time::timeout(
        DEADLINE,
        client.generate(&selected, &source, &CancellationToken::new()),
    )
    .await??;
    let saved = store.begin(&key("selected")).await?;
    assert_eq!(
        saved
            .credential()
            .and_then(Credential::as_oauth)
            .and_then(|token| token.refresh_token.as_deref()),
        Some("rotated-refresh")
    );
    Ok(())
}
