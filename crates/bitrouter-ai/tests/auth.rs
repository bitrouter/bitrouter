//! Explicit selected-account auth, isolated from SDK and ambient file paths.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bitrouter_ai::auth::credentials::{Credential, OAuthToken};
use bitrouter_ai::auth::store::{
    CredentialKey, CredentialStore, CredentialTransaction, Durability, MemoryCredentialStore,
    OAuthRefresher, OAuthSession, StoreError,
};
use bitrouter_ai::auth::{
    AppliedAuth, AuthOperation, CredentialAuthority, normalize_auth_extension_error,
};
use bitrouter_ai::error::{ModelError, Result as ModelResult};
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
