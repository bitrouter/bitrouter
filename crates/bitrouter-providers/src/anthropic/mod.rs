//! Anthropic — dual-mode `AuthApplier` covering both API-key auth and the
//! Claude Pro/Max OAuth subscription path.
//!
//! Same provider id (`"anthropic"`) for both modes: at request time the
//! applier looks up `(anthropic, target.account_label)` in the
//! [`crate::oauth::credential_store::CredentialStore`] and branches on the
//! credential type.
//!
//! | Stored credential | Outbound headers |
//! |---|---|
//! | `Credential::Oauth` (Claude Pro/Max)  | `Authorization: Bearer sk-ant-oat…`, `anthropic-beta: oauth-2025-04-20,claude-code-20250219`, `anthropic-version: 2023-06-01`. **No `x-api-key`** — the upstream rejects OAuth requests that also carry `x-api-key`. |
//! | `Credential::ApiKey`                  | `x-api-key: <value>`, `anthropic-version: 2023-06-01`. |
//! | _no credential in store_              | Fall back to the routing target's `api_key` field (the env-var path). Existing `${ANTHROPIC_API_KEY}` setups behave exactly as before. |
//!
//! The OAuth branch refreshes the access token via
//! [`crate::oauth::refresh::refresh`] if it's within
//! [`crate::oauth::refresh::REFRESH_WINDOW`] of expiry, writes the new token
//! back to the store, and caches it in memory to avoid hammering the
//! refresh endpoint under load.
//!
//! ## Body shape — known gap
//!
//! The Anthropic OAuth endpoint expects requests bodies shaped like
//! Claude Code's: the first system block has to be Claude Code's identity
//! string. This module does NOT enforce that — `AuthApplier::apply`
//! receives an already-serialised body and rewriting JSON there is the
//! wrong layer. A follow-up will land a body shim in the protocol-adapter
//! path. Until then, OAuth credentials will authenticate successfully but
//! the upstream may still reject requests on body grounds.
//!
//! Authoritative reference for the OAuth client + headers: Claude Code's
//! published OAuth client (client_id `9d1c250a-…`), as reused by
//! [`@earendil-works/pi-ai`](https://github.com/openclaw/openclaw)'s
//! Anthropic login and OpenCode's auth registry.

pub mod headers;

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};

use bitrouter_sdk::language_model::AuthApplier;
use bitrouter_sdk::language_model::types::RoutingTarget;
use bitrouter_sdk::{BitrouterError, Result};

use crate::oauth::auth_code::AuthCodeError;
use crate::oauth::credential_store::{Credential, CredentialStore, DEFAULT_LABEL, OAuthToken};
use crate::oauth::refresh::{needs_refresh, refresh};

/// Provider id this applier is registered under.
pub const PROVIDER_ID: &str = "anthropic";

/// `AuthApplier` for `provider_name == "anthropic"`.
///
/// The applier owns:
/// - the credential-store path so it can read + refresh-write back; and
/// - the OAuth client id + token endpoint pulled from
///   [`crate::oauth::registry::find`] at construction.
///
/// In-memory the applier caches the freshly-refreshed `OAuthToken` keyed
/// by `account_label` so concurrent requests don't all hit the refresh
/// endpoint.
pub struct AnthropicOAuthApplier {
    store_path: std::path::PathBuf,
    refresh_client: reqwest::Client,
    /// OAuth client id used for the refresh grant. Mirrors what the
    /// `bitrouter login anthropic` flow wrote.
    client_id: String,
    /// Token endpoint used for the refresh grant.
    token_endpoint: String,
    /// `account_label -> freshest OAuthToken` cache. Populated on first
    /// refresh; subsequent requests within the validity window reuse it
    /// without touching disk or the refresh endpoint.
    cache: Arc<Mutex<std::collections::HashMap<String, OAuthToken>>>,
    /// Per-label single-flight gate around the disk-read → refresh →
    /// persist sequence. RFC 6749 §6 lets the server invalidate older
    /// refresh tokens once a new one is minted, so two concurrent refreshes
    /// of the same label can silently log the user out. Holding this
    /// `tokio::sync::Mutex` across the refresh `await` serialises them;
    /// the second waiter re-checks the cache after acquiring the lock
    /// (double-checked locking) and skips the refresh if the first one
    /// already populated it.
    refresh_gates: Arc<Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl AnthropicOAuthApplier {
    /// Build an applier that reads + writes the credential store at
    /// `store_path` and refreshes tokens using the registry's default
    /// Anthropic OAuth client + token endpoint.
    pub fn new(store_path: impl Into<std::path::PathBuf>) -> Result<Self> {
        let registry = crate::oauth::registry::find(PROVIDER_ID).ok_or_else(|| {
            BitrouterError::internal(
                "anthropic PKCE registry entry is missing — this is a build-time bug".to_string(),
            )
        })?;
        let refresh_client = reqwest::Client::builder()
            .user_agent(concat!("bitrouter-providers/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| {
                BitrouterError::internal(format!(
                    "building Anthropic OAuth refresh HTTP client: {e}"
                ))
            })?;
        Ok(Self {
            store_path: store_path.into(),
            refresh_client,
            client_id: registry.auth.client_id,
            token_endpoint: registry.auth.token_endpoint,
            cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
            refresh_gates: Arc::new(Mutex::new(std::collections::HashMap::new())),
        })
    }

    /// Override the refresh client + token endpoint (tests use this against
    /// `wiremock`).
    #[cfg(test)]
    pub(crate) fn with_client_and_endpoint(
        store_path: impl Into<std::path::PathBuf>,
        refresh_client: reqwest::Client,
        client_id: impl Into<String>,
        token_endpoint: impl Into<String>,
    ) -> Self {
        Self {
            store_path: store_path.into(),
            refresh_client,
            client_id: client_id.into(),
            token_endpoint: token_endpoint.into(),
            cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
            refresh_gates: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Per-label single-flight gate. Cloned out under the std mutex (no
    /// awaits held); the returned `tokio::sync::Mutex` serialises the
    /// disk-read → refresh → persist sequence for that label.
    fn refresh_gate(&self, label: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut guard = self
            .refresh_gates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard
            .entry(label.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn cached_fresh(&self, label: &str) -> Option<OAuthToken> {
        let guard = self.cache.lock().ok()?;
        let token = guard.get(label)?;
        (!needs_refresh(token)).then(|| token.clone())
    }

    fn store_in_cache(&self, label: &str, token: &OAuthToken) {
        if let Ok(mut guard) = self.cache.lock() {
            guard.insert(label.to_string(), token.clone());
        }
    }

    /// Load the stored credential for the given account label, refreshing
    /// the inner OAuth access token if it's within the refresh window.
    /// Returns `None` when no credential is stored under that label —
    /// callers fall through to the routing-target's inline `api_key`.
    ///
    /// Concurrency: cache hits are lock-free. The disk-read → refresh →
    /// persist sequence is single-flighted per label via
    /// [`Self::refresh_gate`] so concurrent requests don't both POST to
    /// the token endpoint (and risk having the server invalidate the
    /// older refresh token, per RFC 6749 §6).
    async fn resolve_credential(&self, label: &str) -> Result<Option<ResolvedCredential>> {
        // 1. Cheap in-memory cache check — no locks held across awaits.
        if let Some(cached) = self.cached_fresh(label) {
            return Ok(Some(ResolvedCredential::Oauth(cached)));
        }
        // 2. Acquire the per-label single-flight gate before any disk
        //    read or refresh POST. If another task is mid-refresh for
        //    this label, we wait here and then short-circuit on the
        //    cache.
        let gate = self.refresh_gate(label);
        let _guard = gate.lock().await;
        // 3. Double-checked locking — another task may have just
        //    refreshed and populated the cache while we were waiting on
        //    the gate.
        if let Some(cached) = self.cached_fresh(label) {
            return Ok(Some(ResolvedCredential::Oauth(cached)));
        }
        // 4. Disk read.
        let store = CredentialStore::load(&self.store_path).map_err(|e| {
            BitrouterError::internal(format!(
                "reading credential store at {}: {e}",
                self.store_path.display()
            ))
        })?;
        let cred = match store.get_any(PROVIDER_ID, label) {
            Some(c) => c.clone(),
            None => return Ok(None),
        };
        // 5. ApiKey: no refresh logic, return as-is.
        let token = match cred {
            Credential::ApiKey { value } => {
                return Ok(Some(ResolvedCredential::ApiKey(value)));
            }
            Credential::Oauth(t) => t,
        };
        // 6. OAuth: refresh if expiring.
        if needs_refresh(&token) {
            let refreshed = refresh(
                &self.refresh_client,
                &self.token_endpoint,
                &self.client_id,
                &token,
            )
            .await
            .map_err(refresh_to_bitrouter_error)?;
            // Persist back to disk so other processes / reloads see it.
            self.persist_refreshed(label, refreshed.clone())?;
            self.store_in_cache(label, &refreshed);
            return Ok(Some(ResolvedCredential::Oauth(refreshed)));
        }
        self.store_in_cache(label, &token);
        Ok(Some(ResolvedCredential::Oauth(token)))
    }

    fn persist_refreshed(&self, label: &str, token: OAuthToken) -> Result<()> {
        let mut store = CredentialStore::load(&self.store_path).map_err(|e| {
            BitrouterError::internal(format!(
                "reloading credential store before refresh write-back at {}: {e}",
                self.store_path.display()
            ))
        })?;
        store
            .set(PROVIDER_ID, label, Credential::from_oauth_token(token))
            .map_err(|e| {
                BitrouterError::internal(format!("persisting refreshed anthropic OAuth token: {e}"))
            })?;
        Ok(())
    }

    fn label_for<'a>(&self, target: &'a RoutingTarget) -> &'a str {
        target.account_label.as_deref().unwrap_or(DEFAULT_LABEL)
    }
}

enum ResolvedCredential {
    Oauth(OAuthToken),
    ApiKey(String),
}

fn refresh_to_bitrouter_error(e: AuthCodeError) -> BitrouterError {
    match e {
        AuthCodeError::OAuthError { error, description } => BitrouterError::Upstream {
            status: 401,
            message: format!(
                "anthropic OAuth refresh failed ({error}{}). Re-run `bitrouter login anthropic`.",
                description.map(|d| format!(": {d}")).unwrap_or_default()
            ),
        },
        other => BitrouterError::Upstream {
            status: 502,
            message: format!("anthropic OAuth refresh transport error: {other}"),
        },
    }
}

#[async_trait]
impl AuthApplier for AnthropicOAuthApplier {
    async fn apply(
        &self,
        mut request: reqwest::Request,
        target: &RoutingTarget,
    ) -> Result<reqwest::Request> {
        let label = self.label_for(target);
        let resolved = self.resolve_credential(label).await?;
        // `anthropic-version` is mandatory regardless of credential type.
        request.headers_mut().insert(
            "anthropic-version",
            HeaderValue::from_static(headers::ANTHROPIC_VERSION),
        );
        match resolved {
            Some(ResolvedCredential::Oauth(token)) => {
                let bearer = format!("Bearer {}", token.access_token);
                let auth = HeaderValue::from_str(&bearer).map_err(|e| {
                    BitrouterError::internal(format!(
                        "invalid anthropic OAuth bearer for Authorization: {e}"
                    ))
                })?;
                let headers_mut = request.headers_mut();
                headers_mut.insert(reqwest::header::AUTHORIZATION, auth);
                // OAuth requests must NOT carry x-api-key — the upstream
                // returns 401 when both auth schemes are present.
                headers_mut.remove("x-api-key");
                let beta_value = headers::OAUTH_BETA_VALUES.join(",");
                let beta_header = HeaderValue::from_str(&beta_value).map_err(|e| {
                    BitrouterError::internal(format!("invalid anthropic-beta header: {e}"))
                })?;
                let beta_name = HeaderName::from_static("anthropic-beta");
                headers_mut.insert(beta_name, beta_header);
                Ok(request)
            }
            Some(ResolvedCredential::ApiKey(value)) => {
                apply_api_key_header(&mut request, &value)?;
                Ok(request)
            }
            None => {
                // No store entry — fall through to the routing-target's
                // configured key (env-var path). Mirrors what
                // `MessagesTransport::authorise` would have done.
                let key = target.effective_api_key();
                if key.is_empty() {
                    return Err(BitrouterError::Upstream {
                        status: 401,
                        message: "no anthropic credential — set ANTHROPIC_API_KEY or run \
                             `bitrouter login anthropic`"
                            .into(),
                    });
                }
                apply_api_key_header(&mut request, key)?;
                Ok(request)
            }
        }
    }
}

fn apply_api_key_header(request: &mut reqwest::Request, key: &str) -> Result<()> {
    let value = HeaderValue::from_str(key).map_err(|e| {
        BitrouterError::internal(format!("invalid api key for x-api-key header: {e}"))
    })?;
    request.headers_mut().insert("x-api-key", value);
    // OAuth and API-key paths must not both be set; clear any stale
    // Bearer the protocol layer might have added.
    request.headers_mut().remove(reqwest::header::AUTHORIZATION);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use bitrouter_sdk::language_model::types::ApiProtocol;
    use wiremock::MockServer;

    use super::*;
    use crate::oauth::credential_store::OAuthToken;

    fn tmp_store_path() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-anthropic-test-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("creds.json")
    }

    fn anthropic_target(label: Option<&str>) -> RoutingTarget {
        RoutingTarget {
            provider_name: PROVIDER_ID.to_string(),
            service_id: "claude-opus-4-7".to_string(),
            api_base: "https://api.anthropic.com/v1".to_string(),
            api_key: String::new(),
            api_protocol: ApiProtocol::Messages,
            account_label: label.map(String::from),
            api_key_override: None,
            api_base_override: None,
        }
    }

    fn anthropic_target_with_env_key(key: &str) -> RoutingTarget {
        let mut t = anthropic_target(None);
        t.api_key = key.to_string();
        t
    }

    #[tokio::test]
    async fn fallthrough_uses_target_api_key_when_store_is_empty() {
        let path = tmp_store_path();
        let applier = AnthropicOAuthApplier::new(&path).unwrap();
        let req = reqwest::Client::new()
            .post("https://api.anthropic.com/v1/messages")
            .build()
            .unwrap();
        let target = anthropic_target_with_env_key("sk-ant-api03-env");
        let authed = applier.apply(req, &target).await.unwrap();
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
    }

    #[tokio::test]
    async fn errors_when_no_credential_anywhere() {
        let path = tmp_store_path();
        let applier = AnthropicOAuthApplier::new(&path).unwrap();
        let req = reqwest::Client::new()
            .post("https://api.anthropic.com/v1/messages")
            .build()
            .unwrap();
        let err = applier
            .apply(req, &anthropic_target(None))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bitrouter login anthropic"),
            "expected helpful hint, got: {msg}"
        );
    }

    #[tokio::test]
    async fn stored_api_key_overrides_target_fallthrough() {
        let path = tmp_store_path();
        // Seed an API key in the store.
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set(
                    PROVIDER_ID,
                    DEFAULT_LABEL,
                    Credential::api_key("sk-ant-api03-from-store"),
                )
                .unwrap();
        }
        let applier = AnthropicOAuthApplier::new(&path).unwrap();
        let req = reqwest::Client::new()
            .post("https://api.anthropic.com/v1/messages")
            .build()
            .unwrap();
        let authed = applier
            .apply(req, &anthropic_target_with_env_key("env-key-shadowed"))
            .await
            .unwrap();
        assert_eq!(
            authed
                .headers()
                .get("x-api-key")
                .and_then(|v| v.to_str().ok()),
            Some("sk-ant-api03-from-store")
        );
    }

    #[tokio::test]
    async fn stored_oauth_token_applies_bearer_and_strips_x_api_key() {
        let path = tmp_store_path();
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set(
                    PROVIDER_ID,
                    DEFAULT_LABEL,
                    Credential::from_oauth_token(OAuthToken {
                        access_token: "sk-ant-oat-fresh".into(),
                        expires_at: 0, // non-expiring → no refresh attempt
                        refresh_token: Some("r".into()),
                    }),
                )
                .unwrap();
        }
        let applier = AnthropicOAuthApplier::new(&path).unwrap();
        let mut req = reqwest::Client::new()
            .post("https://api.anthropic.com/v1/messages")
            .build()
            .unwrap();
        // Pretend the protocol adapter already set x-api-key; the OAuth
        // path must strip it.
        req.headers_mut()
            .insert("x-api-key", HeaderValue::from_static("stale-key"));
        let authed = applier.apply(req, &anthropic_target(None)).await.unwrap();
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
            .unwrap();
        assert!(beta.contains("oauth-2025-04-20"));
        assert!(beta.contains("claude-code-20250219"));
    }

    #[tokio::test]
    async fn fresh_oauth_token_skips_refresh() {
        // Sanity check on the cache-hit path: a long-lived token (1h
        // ahead of the 60s refresh window) is reused directly. The end-
        // to-end refresh round-trip is covered in
        // `oauth::refresh::tests::refresh_returns_new_access_token`.
        let path = tmp_store_path();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set(
                    PROVIDER_ID,
                    DEFAULT_LABEL,
                    Credential::from_oauth_token(OAuthToken {
                        access_token: "still-fresh".into(),
                        expires_at: now + 3600,
                        refresh_token: Some("ignored".into()),
                    }),
                )
                .unwrap();
        }
        // Point the refresh endpoint at a wiremock that fails the test
        // if it's hit at all (no mounted responder → wiremock 404s).
        let server = MockServer::start().await;
        let applier = AnthropicOAuthApplier::with_client_and_endpoint(
            &path,
            reqwest::Client::new(),
            "client-1",
            format!("{}/oauth/token", server.uri()),
        );
        let req = reqwest::Client::new()
            .post("https://api.anthropic.com/v1/messages")
            .build()
            .unwrap();
        let authed = applier.apply(req, &anthropic_target(None)).await.unwrap();
        assert_eq!(
            authed
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer still-fresh")
        );
    }

    #[tokio::test]
    async fn multi_account_lookup_uses_target_label() {
        let path = tmp_store_path();
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set(PROVIDER_ID, "pro-max", Credential::api_key("for-pro-max"))
                .unwrap();
            store
                .set(PROVIDER_ID, "work-key", Credential::api_key("for-work"))
                .unwrap();
        }
        let applier = AnthropicOAuthApplier::new(&path).unwrap();
        let req = reqwest::Client::new()
            .post("https://api.anthropic.com/v1/messages")
            .build()
            .unwrap();
        let authed = applier
            .apply(req, &anthropic_target(Some("pro-max")))
            .await
            .unwrap();
        assert_eq!(
            authed
                .headers()
                .get("x-api-key")
                .and_then(|v| v.to_str().ok()),
            Some("for-pro-max")
        );
        let req2 = reqwest::Client::new()
            .post("https://api.anthropic.com/v1/messages")
            .build()
            .unwrap();
        let authed2 = applier
            .apply(req2, &anthropic_target(Some("work-key")))
            .await
            .unwrap();
        assert_eq!(
            authed2
                .headers()
                .get("x-api-key")
                .and_then(|v| v.to_str().ok()),
            Some("for-work")
        );
    }
}
