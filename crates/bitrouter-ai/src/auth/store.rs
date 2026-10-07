//! Injected account transactions and cancellation-safe OAuth rotation.
//!
//! A transaction serializes the selected slot across refresh and commit. Stores
//! retain staged replacements after a failed commit; the next operation retries
//! persistence before rotating again. Persistent backends define their own
//! process scope and durability. No default path or account discovery lives here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use super::credentials::{Credential, OAuthToken};
use super::{AuthOperation, normalize_auth_extension_error};
use crate::error::{ModelError, Result};

/// Selected slot used when a model target omits an explicit account label.
/// This names one slot; it never discovers or falls back to another account.
pub const DEFAULT_ACCOUNT: &str = "default";

/// One account slot selected by the caller.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CredentialKey {
    /// Provider whose authentication mechanism owns this credential.
    pub provider: String,
    /// Explicit account label, including a caller-chosen default label.
    pub account: String,
}

/// Store failure without credential or backend-controlled diagnostic text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// Storage could not be read or committed; a staged rotation stays retained.
    #[error("credential storage unavailable; any rotated replacement remains pending")]
    Unavailable,
    /// The selected credential changed externally; never overwrite its new owner.
    #[error("selected credential changed during refresh; replacement was not committed")]
    Conflict,
}

/// Storage acknowledgement, separate from successful token rotation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    /// State survives only for the lifetime of the in-memory store.
    Memory,
    /// Backend committed the replacement to its persistent storage.
    Persistent,
}

/// An exclusive selected-account lease spanning read, refresh and commit.
///
/// Implementations must retain `stage`d values beyond a dropped lease and a
/// failed commit. Commit must compare the original credential with current
/// storage, or otherwise exclude login/logout writers. A conflict must never
/// resurrect an account that was removed/replaced. Operations must be bounded.
#[async_trait]
pub trait CredentialTransaction: Send {
    /// Selected stored credential, including expired tokens.
    fn credential(&self) -> Option<&Credential>;
    /// A rotated replacement retained after a previous commit failure.
    fn pending(&self) -> Option<&Credential>;
    /// Retain a replacement before attempting persistence.
    fn stage(&mut self, credential: Credential);
    /// Commit the retained replacement; report the backend's actual guarantee.
    async fn commit(&mut self) -> std::result::Result<Durability, StoreError>;
}

/// Caller-injected storage; every transaction names exactly one selected slot.
#[async_trait]
pub trait CredentialStore: Send + Sync {
    /// Acquire the slot's exclusive lease. No network refresh may start before it.
    async fn begin(
        &self,
        key: &CredentialKey,
    ) -> std::result::Result<Box<dyn CredentialTransaction>, StoreError>;
}

#[derive(Default)]
struct MemoryAccount {
    credential: Option<Credential>,
    pending: Option<Credential>,
}

/// Explicit in-memory storage with no ambient credential loading.
#[derive(Default)]
pub struct MemoryCredentialStore {
    accounts: Mutex<HashMap<CredentialKey, Arc<AsyncMutex<MemoryAccount>>>>,
}

impl MemoryCredentialStore {
    /// Construct storage from caller-supplied account credentials.
    pub fn new(credentials: impl IntoIterator<Item = (CredentialKey, Credential)>) -> Self {
        Self {
            accounts: Mutex::new(
                credentials
                    .into_iter()
                    .map(|(key, credential)| {
                        (
                            key,
                            Arc::new(AsyncMutex::new(MemoryAccount {
                                credential: Some(credential),
                                pending: None,
                            })),
                        )
                    })
                    .collect(),
            ),
        }
    }
}

struct MemoryTransaction(OwnedMutexGuard<MemoryAccount>);

#[async_trait]
impl CredentialTransaction for MemoryTransaction {
    fn credential(&self) -> Option<&Credential> {
        self.0.credential.as_ref()
    }
    fn pending(&self) -> Option<&Credential> {
        self.0.pending.as_ref()
    }
    fn stage(&mut self, credential: Credential) {
        self.0.pending = Some(credential);
    }
    async fn commit(&mut self) -> std::result::Result<Durability, StoreError> {
        if let Some(credential) = self.0.pending.take() {
            self.0.credential = Some(credential);
        }
        Ok(Durability::Memory)
    }
}

#[async_trait]
impl CredentialStore for MemoryCredentialStore {
    async fn begin(
        &self,
        key: &CredentialKey,
    ) -> std::result::Result<Box<dyn CredentialTransaction>, StoreError> {
        let account = self
            .accounts
            .lock()
            .map_err(|_| StoreError::Unavailable)?
            .entry(key.clone())
            .or_default()
            .clone();
        Ok(Box::new(MemoryTransaction(account.lock_owned().await)))
    }
}

/// A provider's bounded token exchange for the already-selected account.
#[async_trait]
pub trait OAuthRefresher: Send + Sync {
    /// Exchange this credential only; no account/key fallback or login is allowed.
    /// The implementation must bound its I/O and return the full replacement.
    async fn refresh(&self, current: &OAuthToken) -> Result<OAuthToken>;
}

/// Account-scoped OAuth resolution using the injected store's transactions.
///
/// After acquiring a lease, an owned Tokio task completes token exchange and
/// persistence even if the caller drops its future. This guarantee requires a
/// running Tokio runtime; process/runtime shutdown can still interrupt I/O.
#[derive(Clone)]
pub struct OAuthSession {
    store: Arc<dyn CredentialStore>,
    refresher: Arc<dyn OAuthRefresher>,
    refresh_window: Duration,
}

impl OAuthSession {
    /// Bind one explicit store and provider refresh mechanism.
    pub fn new(
        store: Arc<dyn CredentialStore>,
        refresher: Arc<dyn OAuthRefresher>,
        refresh_window: Duration,
    ) -> Self {
        Self {
            store,
            refresher,
            refresh_window,
        }
    }

    /// Resolve a selected OAuth credential, refreshing only near expiry.
    pub async fn resolve(&self, key: &CredentialKey) -> Result<OAuthToken> {
        self.run(key, None, None).await
    }

    /// Resolve storage first, using the caller's permitted non-refreshable fallback
    /// only when the selected slot is absent. Wrong-kind/failed stored auth fails.
    pub async fn resolve_with_fallback(
        &self,
        key: &CredentialKey,
        fallback: Option<OAuthToken>,
    ) -> Result<OAuthToken> {
        self.run(key, None, fallback).await
    }

    /// Recover once from a rejected token. Reuse a committed fresh replacement
    /// if another request refreshed that same account while this one was in flight.
    pub async fn recover(
        &self,
        key: &CredentialKey,
        rejected_access: Option<&str>,
    ) -> Result<OAuthToken> {
        self.run(key, Some(rejected_access.map(str::to_owned)), None)
            .await
    }

    async fn run(
        &self,
        key: &CredentialKey,
        rejected_access: Option<Option<String>>,
        fallback: Option<OAuthToken>,
    ) -> Result<OAuthToken> {
        let mut transaction = self.store.begin(key).await.map_err(store_error)?;
        let session = self.clone();
        // Acquiring the lease is cancellable. Once admitted, the owned task is
        // independent of a dropped caller, including during commit.
        tokio::spawn(async move {
            if transaction.pending().is_some() {
                transaction.commit().await.map_err(store_error)?;
            }
            if transaction.credential().is_none()
                && let Some(fallback) = fallback.filter(|token| token.is_valid() && !token.access_token.is_empty()) { return Ok(fallback); }
            let token = transaction.credential().and_then(Credential::as_oauth).cloned()
                .ok_or_else(|| ModelError::Provider {
                    status: 401,
                    message: "selected account has no OAuth credential; supply or log in to that account explicitly".into(),
                })?;
            let fresh = !session.needs_refresh(&token);
            if fresh && token.access_token.is_empty() { return Err(ModelError::Provider { status: 401, message: "selected OAuth credential is empty".into() }); }
            let reuse = match rejected_access {
                None => fresh,
                Some(Some(rejected)) => fresh && token.access_token != rejected,
                Some(None) => false,
            };
            if reuse { return Ok(token); }
            let replacement = session.refresher.refresh(&token).await.map_err(|error| normalize_auth_extension_error(error, AuthOperation::Refresh))?;
            transaction.stage(Credential::Oauth(replacement.clone()));
            transaction.commit().await.map_err(store_error)?;
            if replacement.access_token.is_empty() { return Err(ModelError::invalid_credential("OAuth refresh returned an empty access token; replacement refresh state was retained")); }
            Ok(replacement)
        }).await.map_err(|_| ModelError::configuration("credential refresh task could not complete"))?
    }

    fn needs_refresh(&self, token: &OAuthToken) -> bool {
        if token.expires_at == 0 {
            return false;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now >= token
            .expires_at
            .saturating_sub(self.refresh_window.as_secs())
    }
}

fn store_error(error: StoreError) -> ModelError {
    ModelError::CredentialStorage { failure: error }
}
