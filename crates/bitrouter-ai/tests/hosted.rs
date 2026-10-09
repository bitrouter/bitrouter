//! Hosted JSON/SSE invocation and full-envelope transactions using only AI.
#![cfg(feature = "hosted")]

use async_trait::async_trait;
use bitrouter_ai::auth::store::{CredentialKey, Durability, StoreError};
use bitrouter_ai::auth::{AuthApplier, AuthAppliers};
use bitrouter_ai::client::{HttpTimeouts, ModelClient};
use bitrouter_ai::error::ModelError;
use bitrouter_ai::protocol::{InboundAdapter, chat_completions::ChatCompletionsAdapter};
use bitrouter_ai::providers::hosted::PROVIDER_ID;
use bitrouter_ai::providers::hosted::applier::BitrouterAuthApplier;
use bitrouter_ai::providers::hosted::credentials::{Credentials, StoredCredential};
use bitrouter_ai::providers::hosted::session::{
    HostedCredentialStore, HostedCredentialTransaction, HostedSession,
};
use bitrouter_ai::target::{CredentialPriority, ModelTarget};
use bitrouter_ai::types::{ApiProtocol, Content, Prompt, StreamPart};
use chrono::{Duration, Utc};
use futures::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct State {
    current: Option<StoredCredential>,
    pending: Option<StoredCredential>,
    rejection: Option<&'static str>,
    fail_commit: bool,
    barrier: Option<Arc<CommitBarrier>>,
}

#[derive(Default)]
struct CommitBarrier {
    entered: Notify,
    release: Notify,
}

struct MemoryStore(Arc<Mutex<State>>);

impl MemoryStore {
    fn new(current: Option<StoredCredential>) -> Self {
        Self(Arc::new(Mutex::new(State {
            current,
            pending: None,
            rejection: None,
            fail_commit: false,
            barrier: None,
        })))
    }
}

struct Transaction(OwnedMutexGuard<State>);

#[async_trait]
impl HostedCredentialStore for MemoryStore {
    async fn begin(
        &self,
        selected: &CredentialKey,
    ) -> Result<Box<dyn HostedCredentialTransaction>, StoreError> {
        if *selected != key() {
            return Err(StoreError::Unavailable);
        }
        Ok(Box::new(Transaction(self.0.clone().lock_owned().await)))
    }
}

#[async_trait]
impl HostedCredentialTransaction for Transaction {
    fn credential(&self) -> Option<&StoredCredential> {
        self.0.pending.as_ref().or(self.0.current.as_ref())
    }
    fn rejection(&self) -> Option<&'static str> {
        self.0.rejection
    }
    fn stage(&mut self, replacement: StoredCredential, rejection: Option<&'static str>) {
        self.0.pending = Some(replacement);
        self.0.rejection = rejection;
    }
    async fn commit(&mut self) -> Result<Durability, StoreError> {
        if self.0.pending.is_some() {
            if let Some(barrier) = self.0.barrier.take() {
                barrier.entered.notify_one();
                barrier.release.notified().await;
            }
            if self.0.fail_commit {
                self.0.fail_commit = false;
                return Err(StoreError::Unavailable);
            }
            self.0.current = self.0.pending.take();
        }
        Ok(Durability::Memory)
    }
}

fn key() -> CredentialKey {
    CredentialKey {
        provider: PROVIDER_ID.into(),
        account: "selected".into(),
    }
}

fn credential(issuer: &str) -> Credentials {
    Credentials {
        access_token: "expired-access".into(),
        refresh_token: Some("old-refresh".into()),
        expires_at: Utc::now() - Duration::seconds(1),
        refresh_token_expires_at: Some(Utc::now() + Duration::hours(1)),
        token_type: "Bearer".into(),
        scope: "inference:invoke account:read".into(),
        client_id: "persisted-client".into(),
        authorization_server: issuer.into(),
        namespace_id: Some("namespace-one".into()),
        subject: Some("subject-one".into()),
    }
}

fn target(origin: &str) -> ModelTarget {
    ModelTarget {
        provider_name: PROVIDER_ID.into(),
        service_id: "fixture-model".into(),
        api_base: origin.into(),
        api_key: String::new(),
        credential_priority: Default::default(),
        account_label: Some("selected".into()),
        api_protocol: ApiProtocol::ChatCompletions,
        auth_scheme: Default::default(),
        compatibility: Default::default(),
    }
}

fn prompt() -> bitrouter_ai::error::Result<Prompt> {
    ChatCompletionsAdapter.parse_request(
        json!({"model":"fixture-model","messages":[{"role":"user","content":"hello"}]}),
    )
}

fn session(store: Arc<MemoryStore>) -> HostedSession {
    HostedSession::new(key(), store, reqwest::Client::new())
}
fn applier(session: HostedSession) -> BitrouterAuthApplier {
    BitrouterAuthApplier::new(session, "caller-owned onboarding".into())
}
fn client(session: HostedSession) -> bitrouter_ai::error::Result<ModelClient> {
    Ok(ModelClient::new(HttpTimeouts::default())?
        .with_auth_appliers(AuthAppliers::new().with(PROVIDER_ID, Arc::new(applier(session)))))
}

async fn metadata(server: &MockServer, issuer_path: &str) {
    Mock::given(method("GET"))
        .and(path(format!(
            "/.well-known/oauth-authorization-server{issuer_path}"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_authorization_endpoint":format!("{}/device",server.uri()),
            "token_endpoint":format!("{}/token",server.uri())
        })))
        .expect(1)
        .mount(server)
        .await;
}

async fn rotation(server: &MockServer, reply: Value) {
    metadata(server, "/issuer").await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("refresh_token=old-refresh"))
        .and(body_string_contains("client_id=persisted-client"))
        .and(body_string_contains(
            "scope=inference%3Ainvoke+account%3Aread",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply))
        .expect(1)
        .mount(server)
        .await;
}

fn rotated() -> Value {
    json!({"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600,"refresh_token_expires_in":7200,"scope":"inference:invoke"})
}

fn model_reply(request: &wiremock::Request) -> ResponseTemplate {
    let body = serde_json::from_slice::<Value>(&request.body).ok();
    if body.as_ref().is_some_and(|body| body["stream"] == true) {
        ResponseTemplate::new(200).insert_header("content-type","text/event-stream").set_body_string(concat!(
            "data: {\"id\":\"hosted-response\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"hosted-response\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"))
    } else {
        ResponseTemplate::new(200).set_body_json(json!({"id":"hosted-response","model":"fixture-model","choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}))
    }
}

async fn model(server: &MockServer, count: u64) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer new-access"))
        .respond_with(model_reply)
        .expect(count)
        .mount(server)
        .await;
}

#[tokio::test]
async fn json_and_sse_share_committed_rotation_identity_and_preserve_source() -> TestResult {
    let server = MockServer::start().await;
    rotation(&server, rotated()).await;
    model(&server, 2).await;
    let old = credential(&format!("{}/issuer", server.uri()));
    let store = Arc::new(MemoryStore::new(Some(old.clone().into())));
    let session = session(store.clone());
    let applier = applier(session.clone());
    let client = client(session)?;
    let target = target(&server.uri());
    let source = prompt()?;
    let original = source.clone();
    let authority = applier.continuation_authority(&target).await?;
    let result = client
        .generate(&target, &source, &CancellationToken::new())
        .await?;
    assert!(matches!(result.content.as_slice(),[Content::Text {text,..}] if text == "done"));
    let parts = client
        .stream(&target, &source, &CancellationToken::new())
        .await?
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    assert!(parts.iter().any(StreamPart::is_terminal));
    assert_eq!(source, original);
    assert_eq!(authority, applier.continuation_authority(&target).await?);
    let state = store.0.lock().await;
    let new = state
        .current
        .as_ref()
        .and_then(StoredCredential::oauth)
        .ok_or("missing committed hosted envelope")?;
    assert_eq!(new.refresh_token.as_deref(), Some("new-refresh"));
    assert_eq!(new.scope, "inference:invoke");
    assert_eq!(new.authorization_server, old.authorization_server);
    assert_eq!(new.client_id, old.client_id);
    assert_eq!(new.namespace_id, old.namespace_id);
    assert_eq!(new.subject, old.subject);
    assert!(new.refresh_token_expires_at > old.refresh_token_expires_at);
    Ok(())
}

#[tokio::test]
async fn failed_commit_retains_full_rotation_and_retries_write_without_another_grant() -> TestResult
{
    let server = MockServer::start().await;
    rotation(&server, rotated()).await;
    model(&server, 1).await;
    let old = credential(&format!("{}/issuer", server.uri()));
    let store = Arc::new(MemoryStore::new(Some(old.clone().into())));
    store.0.lock().await.fail_commit = true;
    let client = client(session(store.clone()))?;
    let target = target(&server.uri());
    let source = prompt()?;
    assert!(matches!(
        client
            .generate(&target, &source, &CancellationToken::new())
            .await,
        Err(ModelError::CredentialStorage {
            failure: StoreError::Unavailable
        })
    ));
    {
        let state = store.0.lock().await;
        assert_eq!(state.current, Some(old.into()));
        assert_eq!(
            state
                .pending
                .as_ref()
                .and_then(StoredCredential::oauth)
                .and_then(|c| c.refresh_token.as_deref()),
            Some("new-refresh")
        );
    }
    client
        .generate(&target, &source, &CancellationToken::new())
        .await?;
    assert!(store.0.lock().await.pending.is_none());
    Ok(())
}

#[tokio::test]
async fn dropping_model_caller_after_rotation_staging_still_finishes_commit() -> TestResult {
    let server = MockServer::start().await;
    rotation(&server, rotated()).await;
    model(&server, 1).await;
    let store = Arc::new(MemoryStore::new(Some(
        credential(&format!("{}/issuer", server.uri())).into(),
    )));
    let barrier = Arc::new(CommitBarrier::default());
    store.0.lock().await.barrier = Some(barrier.clone());
    let client = Arc::new(client(session(store.clone()))?);
    let target = target(&server.uri());
    let source = prompt()?;
    let caller = {
        let client = client.clone();
        let target = target.clone();
        let source = source.clone();
        tokio::spawn(async move {
            client
                .generate(&target, &source, &CancellationToken::new())
                .await
        })
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        barrier.entered.notified(),
    )
    .await?;
    caller.abort();
    assert!(caller.await.is_err());
    barrier.release.notify_one();
    // The next lease waits for the owned task to acknowledge the staged write.
    client
        .generate(&target, &source, &CancellationToken::new())
        .await?;
    assert_eq!(
        store
            .0
            .lock()
            .await
            .current
            .as_ref()
            .and_then(StoredCredential::oauth)
            .map(|c| c.access_token.as_str()),
        Some("new-access")
    );
    Ok(())
}

#[tokio::test]
async fn contradictory_identity_retains_rotation_and_never_dispatches_or_reexchanges() -> TestResult
{
    let server = MockServer::start().await;
    let mut reply = rotated();
    reply["namespace_id"] = json!("different-namespace");
    rotation(&server, reply).await;
    let old = credential(&format!("{}/issuer", server.uri()));
    let store = Arc::new(MemoryStore::new(Some(old.clone().into())));
    let client = client(session(store.clone()))?;
    for _ in 0..2 {
        assert!(matches!(
            client
                .generate(
                    &target(&server.uri()),
                    &prompt()?,
                    &CancellationToken::new()
                )
                .await,
            Err(ModelError::Provider { status: 401, .. })
        ));
    }
    let state = store.0.lock().await;
    assert_eq!(state.current, Some(old.into()));
    assert!(state.rejection.is_some());
    assert_eq!(
        state
            .pending
            .as_ref()
            .and_then(StoredCredential::oauth)
            .and_then(|c| c.refresh_token.as_deref()),
        Some("new-refresh")
    );
    Ok(())
}

#[tokio::test]
async fn bad_native_http_replies_fail_closed_without_echoing_or_committing_material() -> TestResult
{
    for (status, body) in [
        (
            401,
            r#"{"access_token":"forbidden-secret","refresh_token":"rotated-secret","error_description":"opaque-secret"}"#,
        ),
        (200, r#"{"access_token":"malformed-secret""#),
    ] {
        let server = MockServer::start().await;
        metadata(&server, "/issuer").await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .expect(1)
            .mount(&server)
            .await;
        let old = credential(&format!("{}/issuer", server.uri()));
        let store = Arc::new(MemoryStore::new(Some(old.clone().into())));
        let client = client(session(store.clone()))?;
        let error = client
            .generate(
                &target(&server.uri()),
                &prompt()?,
                &CancellationToken::new(),
            )
            .await
            .err()
            .ok_or("unexpected native authentication success")?;
        assert!(matches!(error, ModelError::Provider { status: 401, .. }));
        let text = format!("{error:?}");
        assert!(!text.contains("secret"));
        let state = store.0.lock().await;
        assert_eq!(state.current, Some(old.into()));
        assert!(state.pending.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn selected_slot_and_origin_failures_never_enable_fallback_but_explicit_key_bypasses()
-> TestResult {
    let store = Arc::new(MemoryStore::new(Some(StoredCredential::api_key(
        "stored-key".into(),
        "https://bound.example".into(),
    ))));
    let auth = applier(session(store));
    let mut selected = target("https://other.example");
    selected.api_key = "fallback-key".into();
    selected.credential_priority = CredentialPriority::Fallback;
    assert!(matches!(
        auth.continuation_authority(&selected).await,
        Err(ModelError::Provider { status: 401, .. })
    ));
    selected.account_label = Some("other".into());
    assert!(matches!(
        auth.continuation_authority(&selected).await,
        Err(ModelError::Configuration { .. })
    ));
    selected.credential_priority = CredentialPriority::Explicit;
    assert!(auth.continuation_authority(&selected).await?.is_some());
    let absent = applier(session(Arc::new(MemoryStore::new(None))));
    selected.account_label = Some("selected".into());
    selected.credential_priority = CredentialPriority::Fallback;
    let request = reqwest::Client::new()
        .post("https://other.example")
        .build()?;
    assert_eq!(
        absent.apply(request, &selected).await?.headers()["authorization"],
        "Bearer fallback-key"
    );
    Ok(())
}
