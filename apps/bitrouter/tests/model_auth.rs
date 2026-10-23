//! Explicit selected-account auth, isolated from SDK and ambient file paths.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bitrouter::gateway::model::error::{ModelError, Result as ModelResult};
use bitrouter::gateway::providers::auth::credentials::{Credential, OAuthToken};
use bitrouter::gateway::providers::auth::store::{
    CredentialKey, CredentialStore, CredentialTransaction, Durability, MemoryCredentialStore,
    OAuthRefresher, OAuthSession, StoreError,
};
use bitrouter::gateway::providers::auth::{
    AppliedAuth, AuthOperation, CredentialAuthority, normalize_auth_extension_error,
};
use tokio::sync::Notify;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const DEADLINE: Duration = Duration::from_secs(5);

fn key(account: &str) -> CredentialKey {
    CredentialKey {
        provider: "selected".into(),
        account: account.into(),
    }
}
fn old_token() -> OAuthToken {
    OAuthToken {
        access_token: "old-private-access".into(),
        expires_at: 1,
        refresh_token: Some("old-private-refresh".into()),
    }
}
fn replacement() -> OAuthToken {
    OAuthToken {
        access_token: "new-private-access".into(),
        expires_at: 0,
        refresh_token: Some("new-private-refresh".into()),
    }
}

#[derive(Default)]
struct Refresh {
    calls: AtomicUsize,
    entered: Notify,
    release: Notify,
    block: bool,
}

#[async_trait]
impl OAuthRefresher for Refresh {
    async fn refresh(&self, current: &OAuthToken) -> ModelResult<OAuthToken> {
        assert_eq!(current, &old_token());
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        if self.block {
            self.release.notified().await;
        }
        Ok(replacement())
    }
}

fn session(store: Arc<dyn CredentialStore>, refresh: Arc<Refresh>) -> OAuthSession {
    OAuthSession::new(store, refresh, Duration::from_secs(60))
}

#[tokio::test]
async fn concurrent_calls_commit_one_rotation_and_share_its_replacement() -> TestResult {
    let store = Arc::new(MemoryCredentialStore::new([(
        key("one"),
        Credential::Oauth(old_token()),
    )]));
    let refresh = Arc::new(Refresh {
        block: true,
        ..Default::default()
    });
    let mut calls = Vec::new();
    for _ in 0..8 {
        // Separate session instances must still share the store's account lease.
        let session = session(store.clone(), refresh.clone());
        calls.push(tokio::spawn(
            async move { session.resolve(&key("one")).await },
        ));
    }
    tokio::time::timeout(DEADLINE, refresh.entered.notified()).await?;
    refresh.release.notify_one();
    for call in calls {
        assert_eq!(tokio::time::timeout(DEADLINE, call).await???, replacement());
    }
    assert_eq!(refresh.calls.load(Ordering::SeqCst), 1);
    let transaction = store.begin(&key("one")).await?;
    assert_eq!(
        transaction.credential(),
        Some(&Credential::Oauth(replacement()))
    );
    assert!(transaction.pending().is_none());
    Ok(())
}

#[tokio::test]
async fn dropping_caller_during_rotation_still_commits_returned_refresh_token() -> TestResult {
    let store = Arc::new(MemoryCredentialStore::new([(
        key("one"),
        Credential::Oauth(old_token()),
    )]));
    let refresh = Arc::new(Refresh {
        block: true,
        ..Default::default()
    });
    let session = session(store.clone(), refresh.clone());
    let call = tokio::spawn(async move { session.resolve(&key("one")).await });
    tokio::time::timeout(DEADLINE, refresh.entered.notified()).await?;
    call.abort();
    assert!(call.await.is_err_and(|error| error.is_cancelled()));
    refresh.release.notify_one();
    // The owned operation holds the lease until after commit; this read is a
    // completion signal, rather than a timing guess about a detached task.
    let transaction = tokio::time::timeout(DEADLINE, store.begin(&key("one"))).await??;
    assert_eq!(
        transaction.credential(),
        Some(&Credential::Oauth(replacement()))
    );
    assert!(transaction.pending().is_none());
    assert_eq!(refresh.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

struct ControlledStore {
    inner: MemoryCredentialStore,
    fail_once: Arc<AtomicBool>,
    block_commit: bool,
    commit_entered: Arc<Notify>,
    release_commit: Arc<Notify>,
}

struct ControlledTransaction {
    inner: Box<dyn CredentialTransaction>,
    fail_once: Arc<AtomicBool>,
    block_commit: bool,
    commit_entered: Arc<Notify>,
    release_commit: Arc<Notify>,
}

#[async_trait]
impl CredentialStore for ControlledStore {
    async fn begin(
        &self,
        key: &CredentialKey,
    ) -> Result<Box<dyn CredentialTransaction>, StoreError> {
        Ok(Box::new(ControlledTransaction {
            inner: self.inner.begin(key).await?,
            fail_once: self.fail_once.clone(),
            block_commit: self.block_commit,
            commit_entered: self.commit_entered.clone(),
            release_commit: self.release_commit.clone(),
        }))
    }
}

#[async_trait]
impl CredentialTransaction for ControlledTransaction {
    fn credential(&self) -> Option<&Credential> {
        self.inner.credential()
    }
    fn pending(&self) -> Option<&Credential> {
        self.inner.pending()
    }
    fn stage(&mut self, credential: Credential) {
        self.inner.stage(credential);
    }
    async fn commit(&mut self) -> Result<Durability, StoreError> {
        self.commit_entered.notify_one();
        if self.fail_once.swap(false, Ordering::SeqCst) {
            return Err(StoreError::Unavailable);
        }
        if self.block_commit {
            self.release_commit.notified().await;
        }
        self.inner.commit().await
    }
}

fn controlled_store(fail: bool, block: bool) -> Arc<ControlledStore> {
    Arc::new(ControlledStore {
        inner: MemoryCredentialStore::new([(key("one"), Credential::Oauth(old_token()))]),
        fail_once: Arc::new(AtomicBool::new(fail)),
        block_commit: block,
        commit_entered: Arc::new(Notify::new()),
        release_commit: Arc::new(Notify::new()),
    })
}

#[tokio::test]
async fn commit_failure_is_honest_and_next_call_persists_without_rotating_again() -> TestResult {
    let store = controlled_store(true, false);
    let refresh = Arc::new(Refresh::default());
    let session = session(store.clone(), refresh.clone());
    let error = session
        .resolve(&key("one"))
        .await
        .err()
        .ok_or("failed commit was reported as success")?;
    assert!(error.to_string().contains("remains pending"));
    for secret in [
        "old-private-access",
        "old-private-refresh",
        "new-private-access",
        "new-private-refresh",
    ] {
        assert!(!format!("{error:?}\n{error}").contains(secret));
    }
    {
        let transaction = store.begin(&key("one")).await?;
        assert_eq!(
            transaction.credential(),
            Some(&Credential::Oauth(old_token()))
        );
        assert_eq!(
            transaction.pending(),
            Some(&Credential::Oauth(replacement()))
        );
    }
    assert_eq!(session.resolve(&key("one")).await?, replacement());
    assert_eq!(refresh.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn dropping_caller_during_commit_does_not_discard_rotation() -> TestResult {
    let store = controlled_store(false, true);
    let refresh = Arc::new(Refresh::default());
    let session = session(store.clone(), refresh);
    let call = tokio::spawn(async move { session.resolve(&key("one")).await });
    tokio::time::timeout(DEADLINE, store.commit_entered.notified()).await?;
    call.abort();
    assert!(call.await.is_err_and(|error| error.is_cancelled()));
    store.release_commit.notify_one();
    let transaction = tokio::time::timeout(DEADLINE, store.begin(&key("one"))).await??;
    assert_eq!(
        transaction.credential(),
        Some(&Credential::Oauth(replacement()))
    );
    assert!(transaction.pending().is_none());
    Ok(())
}

#[tokio::test]
async fn missing_wrong_kind_and_other_account_never_switch_to_another_credential() -> TestResult {
    let store = Arc::new(MemoryCredentialStore::new([
        (key("other"), Credential::Oauth(replacement())),
        (key("wrong"), Credential::api_key("private-key")),
    ]));
    let refresh = Arc::new(Refresh::default());
    let session = session(store, refresh.clone());
    for account in ["missing", "wrong"] {
        assert!(matches!(
            session.resolve(&key(account)).await,
            Err(ModelError::Provider { status: 401, .. })
        ));
    }
    assert_eq!(refresh.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[test]
fn continuation_proofs_reject_ambiguous_wire_auth_and_redact_request_debug() -> TestResult {
    let mut request = reqwest::Client::new()
        .post("https://user:private-password@example.invalid/path?api_key=private-key")
        .build()?;
    request.headers_mut().insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_static("Bearer private-access"),
    );
    let proof = CredentialAuthority::derive("provider/account", "private-principal");
    let (mut request, authority) = AppliedAuth::proven(request, proof.clone()).into_parts();
    let authority = authority.ok_or("valid Bearer did not prove authority")?;
    assert!(authority.validates_final_request(&request));
    request.headers_mut().insert(
        "x-api-key",
        reqwest::header::HeaderValue::from_static("other-secret"),
    );
    assert!(!authority.validates_final_request(&request));
    let applied = AppliedAuth::proven(request, proof);
    let debug = format!("{applied:?}");
    assert!(!debug.contains("private"));
    assert!(applied.into_parts().1.is_none());
    Ok(())
}

#[test]
fn opaque_auth_diagnostics_keep_status_and_retry_hints_without_secret_text() {
    let error = normalize_auth_extension_error(
        ModelError::HttpResponse {
            status: 429,
            body: "private-refresh".into(),
            retry_after: Some(9),
        },
        AuthOperation::Refresh,
    );
    assert!(matches!(
        error,
        ModelError::HttpResponse {
            status: 429,
            retry_after: Some(9),
            ..
        }
    ));
    assert!(!format!("{error:?}\n{error}").contains("private-refresh"));
    assert!(format!("{:?}", Credential::Oauth(replacement())).contains("<redacted>"));
}

struct RejectedRefresh;
#[async_trait]
impl OAuthRefresher for RejectedRefresh {
    async fn refresh(&self, _current: &OAuthToken) -> ModelResult<OAuthToken> {
        Err(ModelError::Provider {
            status: 401,
            message: "private-refresh-rejected".into(),
        })
    }
}

#[tokio::test]
async fn fallback_is_used_only_for_absent_slot_and_never_hides_stored_auth_failure() -> TestResult {
    let store = Arc::new(MemoryCredentialStore::new([
        (key("wrong"), Credential::api_key("other-kind")),
        (key("expired"), Credential::Oauth(old_token())),
        (key("valid"), Credential::Oauth(replacement())),
    ]));
    let session = OAuthSession::new(store, Arc::new(RejectedRefresh), Duration::from_secs(60));
    let fallback = OAuthToken {
        access_token: "permitted-fallback".into(),
        expires_at: 0,
        refresh_token: None,
    };
    assert_eq!(
        session
            .resolve_with_fallback(&key("missing"), Some(fallback.clone()))
            .await?,
        fallback
    );
    assert_eq!(
        session
            .resolve_with_fallback(&key("valid"), Some(fallback.clone()))
            .await?,
        replacement()
    );
    for account in ["wrong", "expired"] {
        let error = session
            .resolve_with_fallback(&key(account), Some(fallback.clone()))
            .await
            .err()
            .ok_or("stored auth failure used fallback")?;
        assert!(matches!(error, ModelError::Provider { status: 401, .. }));
        assert!(!format!("{error:?}").contains("private-refresh-rejected"));
    }
    Ok(())
}

// Models a durable lease: dropping a transaction does not release shared state.
struct ReadLeaseStore {
    credential: Option<Credential>,
    leased: Arc<AtomicBool>,
    releases: Arc<AtomicUsize>,
    fail_release: bool,
}

impl ReadLeaseStore {
    fn new(credential: Option<Credential>, fail_release: bool) -> Arc<Self> {
        Arc::new(Self {
            credential,
            leased: Arc::new(AtomicBool::new(false)),
            releases: Arc::new(AtomicUsize::new(0)),
            fail_release,
        })
    }
}

struct ReadLease {
    credential: Option<Credential>,
    pending: Option<Credential>,
    leased: Arc<AtomicBool>,
    releases: Arc<AtomicUsize>,
    fail_release: bool,
}

#[async_trait]
impl CredentialStore for ReadLeaseStore {
    async fn begin(
        &self,
        _key: &CredentialKey,
    ) -> Result<Box<dyn CredentialTransaction>, StoreError> {
        self.leased
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| StoreError::Conflict)?;
        Ok(Box::new(ReadLease {
            credential: self.credential.clone(),
            pending: None,
            leased: self.leased.clone(),
            releases: self.releases.clone(),
            fail_release: self.fail_release,
        }))
    }
}

#[async_trait]
impl CredentialTransaction for ReadLease {
    fn credential(&self) -> Option<&Credential> {
        self.credential.as_ref()
    }
    fn pending(&self) -> Option<&Credential> {
        self.pending.as_ref()
    }
    fn stage(&mut self, credential: Credential) {
        self.pending = Some(credential);
    }
    async fn commit(&mut self) -> Result<Durability, StoreError> {
        Err(StoreError::Conflict)
    }
    async fn release(&mut self) -> Result<(), StoreError> {
        if self.fail_release {
            return Err(StoreError::Unavailable);
        }
        self.leased.store(false, Ordering::SeqCst);
        self.releases.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn read_lease_target(provider: &str) -> bitrouter::gateway::model::target::ModelTarget {
    bitrouter::gateway::model::target::ModelTarget {
        provider_name: provider.into(),
        service_id: "fixture-model".into(),
        api_base: "https://example.invalid".into(),
        api_key: String::new(),
        credential_priority: Default::default(),
        account_label: Some("selected".into()),
        api_protocol: bitrouter_ai::types::ApiProtocol::ChatCompletions,
        auth_scheme: Default::default(),
        compatibility: Default::default(),
    }
}

#[tokio::test]
async fn anthropic_releases_read_leases_on_success_and_invalid_credentials() -> TestResult {
    use bitrouter::gateway::providers::anthropic::AnthropicApiKeyApplier;
    use bitrouter::gateway::providers::auth::AuthApplier;
    for credential in [
        Some(Credential::ApiKey {
            value: "selected-api-key".into(),
        }),
        Some(Credential::Oauth(replacement())),
        None,
    ] {
        let valid = matches!(credential, Some(Credential::ApiKey { .. }));
        let store = ReadLeaseStore::new(credential, false);
        let applier = AnthropicApiKeyApplier::new(store.clone());
        for _ in 0..2 {
            let request = reqwest::Request::new(
                reqwest::Method::POST,
                "https://example.invalid/messages".parse()?,
            );
            let applied = applier
                .apply(request, &read_lease_target("anthropic"))
                .await;
            if valid {
                assert_eq!(applied?.headers()["x-api-key"], "selected-api-key");
            } else {
                assert!(matches!(
                    applied,
                    Err(ModelError::Provider { status: 401, .. })
                ));
            }
        }
        assert_eq!(store.releases.load(Ordering::SeqCst), 2);
        assert!(!store.leased.load(Ordering::SeqCst));
    }
    Ok(())
}

#[tokio::test]
async fn anthropic_does_not_expose_auth_when_read_lease_release_fails() -> TestResult {
    use bitrouter::gateway::providers::anthropic::AnthropicApiKeyApplier;
    use bitrouter::gateway::providers::auth::AuthApplier;
    let store = ReadLeaseStore::new(
        Some(Credential::ApiKey {
            value: "selected-api-key".into(),
        }),
        true,
    );
    let applier = AnthropicApiKeyApplier::new(store.clone());
    let request = reqwest::Request::new(
        reqwest::Method::POST,
        "https://example.invalid/messages".parse()?,
    );
    assert!(matches!(
        applier
            .apply(request, &read_lease_target("anthropic"))
            .await,
        Err(ModelError::CredentialStorage {
            failure: StoreError::Unavailable
        })
    ));
    assert_eq!(store.releases.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn copilot_releases_read_leases_after_exchange_cache_hit_and_failure() -> TestResult {
    use bitrouter::gateway::providers::copilot::CopilotAuthApplier;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    for status in [200, 500] {
        let server = MockServer::start().await;
        let expires = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
            + 3600;
        Mock::given(method("GET"))
            .and(path("/exchange"))
            .respond_with(ResponseTemplate::new(status).set_body_json(
                serde_json::json!({"token":"ephemeral-token", "expires_at":expires}),
            ))
            .expect(if status == 200 { 1 } else { 2 })
            .mount(&server)
            .await;
        let store = ReadLeaseStore::new(Some(Credential::Oauth(replacement())), false);
        let applier = CopilotAuthApplier::new(
            reqwest::Client::builder().timeout(DEADLINE).build()?,
            format!("{}/exchange", server.uri()),
            store.clone(),
        );
        for _ in 0..2 {
            let auth = applier
                .obtain_copilot_token(&read_lease_target("github-copilot"))
                .await;
            if status == 200 {
                assert_eq!(auth?.token, "ephemeral-token");
            } else {
                assert!(matches!(
                    auth,
                    Err(ModelError::Provider { status: 502, .. })
                ));
            }
        }
        assert_eq!(store.releases.load(Ordering::SeqCst), 2);
        assert!(!store.leased.load(Ordering::SeqCst));
    }
    Ok(())
}

#[tokio::test]
async fn copilot_releases_invalid_credentials_without_attempting_exchange() -> TestResult {
    use bitrouter::gateway::providers::copilot::CopilotAuthApplier;
    let store = ReadLeaseStore::new(
        Some(Credential::ApiKey {
            value: "wrong-kind".into(),
        }),
        false,
    );
    let applier = CopilotAuthApplier::new(
        reqwest::Client::builder().timeout(DEADLINE).build()?,
        "http://127.0.0.1:1/exchange",
        store.clone(),
    );
    for _ in 0..2 {
        assert!(matches!(
            applier
                .obtain_copilot_token(&read_lease_target("github-copilot"))
                .await,
            Err(ModelError::Provider { status: 401, .. })
        ));
    }
    assert_eq!(store.releases.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn copilot_does_not_expose_cached_auth_when_read_lease_release_fails() -> TestResult {
    use bitrouter::gateway::providers::copilot::CopilotAuthApplier;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let expires = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs()
        + 3600;
    Mock::given(method("GET"))
        .and(path("/exchange"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"token":"ephemeral-token", "expires_at":expires}),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    let store = ReadLeaseStore::new(Some(Credential::Oauth(replacement())), true);
    let applier = CopilotAuthApplier::new(
        reqwest::Client::builder().timeout(DEADLINE).build()?,
        format!("{}/exchange", server.uri()),
        store.clone(),
    );
    assert!(matches!(
        applier
            .obtain_copilot_token(&read_lease_target("github-copilot"))
            .await,
        Err(ModelError::CredentialStorage {
            failure: StoreError::Unavailable
        })
    ));
    assert_eq!(store.releases.load(Ordering::SeqCst), 0);
    assert!(store.leased.load(Ordering::SeqCst));
    Ok(())
}
