//! `BitrouterCloudAuthApplier` — credential resolver + header injector for
//! outbound requests against the BitRouter Cloud LLM endpoints.
//!
//! Two credential sources are tried in order:
//!
//! 1. **OAuth access token** persisted by `bitrouter auth login`
//!    ([`crate::auth::credentials::CredentialsStore`]). When the stored
//!    access token is within
//!    [`crate::auth::credentials::REFRESH_WINDOW`] of expiry the store's
//!    [`CredentialsStore::current_token`](crate::auth::credentials::CredentialsStore::current_token)
//!    method runs the RFC 6749 §6 refresh exchange against the AS token
//!    endpoint and writes the rotated tokens back to disk before the
//!    bearer is returned. Concurrent inbound requests are serialised
//!    through a single-flight [`tokio::sync::Mutex`] so only one refresh
//!    POST happens at a time — RFC 6749 §6 lets the AS invalidate the
//!    older refresh token once a new one is minted, so two concurrent
//!    refreshes can silently log the user out.
//! 2. **Inline `brk_…` API key** carried on the [`RoutingTarget`]. The
//!    `bitrouter` entry in `bitrouter-providers` advertises
//!    `auth.env = "BITROUTER_API_KEY"`, so
//!    `bitrouter_providers::apply_builtin_defaults` populates
//!    `RoutingTarget::api_key` from the process environment at config
//!    assembly time. Applied as `Authorization: Bearer <api_key>`.
//!
//! When neither source resolves the applier returns
//! [`BitrouterError::Upstream`] with status `401` and an onboarding hint
//! ([`onboarding_hint`]). The CLI surfaces this back through the standard
//! error-reporting path.
//!
//! ## Cached AS metadata
//!
//! The RFC 8414 authorization-server metadata document is fetched on the
//! first call that exercises the OAuth branch and cached for the process
//! lifetime. The cache key is the AS URL recorded in the credentials
//! file, so a `bitrouter auth login` against a fresh AS will trigger a
//! re-fetch on the first inference call.
//!
//! References:
//! - RFC 6749 §6 (refresh): <https://www.rfc-editor.org/rfc/rfc6749#section-6>
//! - RFC 6750 §2.1 (Bearer): <https://www.rfc-editor.org/rfc/rfc6750#section-2.1>
//! - RFC 8414 (AS metadata): <https://www.rfc-editor.org/rfc/rfc8414>
//! - RFC 9700 §4.14 (rotation guidance): <https://www.rfc-editor.org/rfc/rfc9700#section-4.14>

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::HeaderValue;
use tokio::sync::{Mutex, RwLock};

use bitrouter_sdk::language_model::AuthApplier;
use bitrouter_sdk::language_model::types::RoutingTarget;
use bitrouter_sdk::{BitrouterError, Result};

use crate::auth::credentials::CredentialsStore;
use crate::auth::metadata::{self, AsMetadata};

/// Provider id this applier is registered under. Short and brand-aligned
/// so model addressing reads naturally: `bitrouter:gpt-5.5`,
/// `bitrouter:claude-sonnet-4.6`, etc. The crate name keeps the longer
/// `bitrouter-cloud-sdk` form to disambiguate from the local bitrouter
/// binary.
pub const PROVIDER_ID: &str = "bitrouter";

/// Onboarding text emitted when neither an OAuth credential nor a
/// `BITROUTER_API_KEY` is available. Kept short for the 401 response body
/// the CLI prints back to the user; the longer multi-step prompt at zero
/// config time is rendered separately by `apps/bitrouter`.
pub fn onboarding_hint() -> &'static str {
    "no BitRouter Cloud credential — run `bitrouter auth login` or set BITROUTER_API_KEY=brk_…"
}

/// `AuthApplier` for `provider_name == "bitrouter"`.
///
/// The applier is constructed once during [`apps/bitrouter`'s
/// `build_auth_appliers`](../../../bitrouter/index.html) call and
/// registered against the SDK executor; one instance serves the lifetime
/// of the daemon. Internal mutable state (the AS-metadata cache and the
/// refresh single-flight gate) is held behind tokio locks so the applier
/// is `Send + Sync`.
pub struct BitrouterCloudAuthApplier {
    credentials_path: PathBuf,
    refresh_client: reqwest::Client,
    /// AS metadata cache — keyed by AS base URL so a re-login against a
    /// different AS invalidates the entry naturally on the next request.
    metadata_cache: Arc<RwLock<Option<CachedMetadata>>>,
    /// Single-flight gate around the disk-read → refresh → persist
    /// sequence. RFC 6749 §6 permits the AS to invalidate the prior
    /// refresh token once a new one is minted, so two concurrent refreshes
    /// can lose tokens; the mutex serialises them.
    refresh_gate: Arc<Mutex<()>>,
}

#[derive(Clone)]
struct CachedMetadata {
    as_url: String,
    metadata: AsMetadata,
}

impl BitrouterCloudAuthApplier {
    /// Build an applier reading the user credentials from `credentials_path`.
    /// Most call sites pass
    /// [`crate::auth::credentials::default_credentials_path`].
    pub fn new(credentials_path: impl Into<PathBuf>) -> Result<Self> {
        let refresh_client = reqwest::Client::builder()
            .user_agent(concat!("bitrouter-cloud-sdk/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| {
                BitrouterError::internal(format!(
                    "building BitRouter Cloud refresh HTTP client: {e}"
                ))
            })?;
        Ok(Self {
            credentials_path: credentials_path.into(),
            refresh_client,
            metadata_cache: Arc::new(RwLock::new(None)),
            refresh_gate: Arc::new(Mutex::new(())),
        })
    }

    /// Construct an applier with an explicit HTTP client. Tests use this
    /// to point the refresh client at a `wiremock` server.
    pub fn with_client(
        credentials_path: impl Into<PathBuf>,
        refresh_client: reqwest::Client,
    ) -> Self {
        Self {
            credentials_path: credentials_path.into(),
            refresh_client,
            metadata_cache: Arc::new(RwLock::new(None)),
            refresh_gate: Arc::new(Mutex::new(())),
        }
    }

    /// Fetch + cache the AS metadata for `as_url`. Subsequent calls with
    /// the same URL return the cached value without a network round-trip.
    async fn resolve_metadata(&self, as_url: &str) -> Result<AsMetadata> {
        if let Some(cached) = self.metadata_cache.read().await.as_ref()
            && cached.as_url == as_url
        {
            return Ok(cached.metadata.clone());
        }
        let metadata = metadata::fetch(&self.refresh_client, as_url)
            .await
            .map_err(|e| BitrouterError::Upstream {
                status: 502,
                message: format!("fetching BitRouter Cloud AS metadata at {as_url}: {e:#}"),
            })?;
        *self.metadata_cache.write().await = Some(CachedMetadata {
            as_url: as_url.to_string(),
            metadata: metadata.clone(),
        });
        Ok(metadata)
    }

    /// Resolve a current OAuth bearer if a credentials file is present,
    /// refreshing transparently if it's within the refresh window. Returns
    /// `Ok(None)` when no credentials file exists — callers fall through
    /// to the inline API-key path.
    async fn resolve_oauth_bearer(&self) -> Result<Option<String>> {
        // Cheap pre-check: no file means we can skip the lock + AS fetch.
        if !self.credentials_path.exists() {
            return Ok(None);
        }
        // Serialise the disk-read → refresh → persist sequence per RFC
        // 6749 §6 rotation safety; see the field-level comment on
        // `refresh_gate`.
        let _guard = self.refresh_gate.lock().await;
        // Double-check after acquiring the gate — a parallel task may
        // have just deleted the file (logout).
        let mut store = CredentialsStore::load(&self.credentials_path).map_err(|e| {
            BitrouterError::internal(format!(
                "reading credentials at {}: {e:#}",
                self.credentials_path.display()
            ))
        })?;
        let Some(creds) = store.current().cloned() else {
            return Ok(None);
        };
        let metadata = self.resolve_metadata(&creds.authorization_server).await?;
        let token = store
            .current_token(&self.refresh_client, &metadata)
            .await
            .map_err(|e| BitrouterError::Upstream {
                status: 401,
                message: format!("BitRouter Cloud token refresh failed: {e:#}"),
            })?;
        Ok(Some(token))
    }
}

#[async_trait]
impl AuthApplier for BitrouterCloudAuthApplier {
    async fn apply(
        &self,
        mut request: reqwest::Request,
        target: &RoutingTarget,
    ) -> Result<reqwest::Request> {
        let bearer = match self.resolve_oauth_bearer().await? {
            Some(token) => token,
            None => {
                // Fall back to the inline api_key (filled from
                // BITROUTER_API_KEY by apply_builtin_defaults, or set
                // explicitly in bitrouter.yaml). `effective_api_key`
                // honours a per-request `api_key_override` for BYOK
                // hooks that substitute the caller's own key.
                let key = target.effective_api_key();
                if key.is_empty() {
                    return Err(BitrouterError::Upstream {
                        status: 401,
                        message: onboarding_hint().to_string(),
                    });
                }
                key.to_string()
            }
        };
        let value = HeaderValue::from_str(&format!("Bearer {bearer}")).map_err(|e| {
            BitrouterError::internal(format!(
                "invalid BitRouter Cloud bearer for Authorization: {e}"
            ))
        })?;
        request
            .headers_mut()
            .insert(reqwest::header::AUTHORIZATION, value);
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use bitrouter_sdk::language_model::types::ApiProtocol;
    use chrono::{Duration as ChronoDuration, Utc};
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::credentials::{Credentials, CredentialsStore};

    fn tmp_creds_path(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-cloud-applier-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("account-credentials.json")
    }

    fn target_with_api_key(key: &str) -> RoutingTarget {
        RoutingTarget {
            provider_name: PROVIDER_ID.to_string(),
            service_id: "gpt-4o".to_string(),
            api_base: "https://api.bitrouter.ai/v1".to_string(),
            api_key: key.to_string(),
            api_protocol: ApiProtocol::ChatCompletions,
            account_label: None,
            api_key_override: None,
            api_base_override: None,
        }
    }

    fn empty_request() -> reqwest::Request {
        // A throwaway request — the applier only mutates headers.
        let client = reqwest::Client::new();
        client
            .post("https://api.bitrouter.ai/v1/chat/completions")
            .build()
            .unwrap()
    }

    fn bearer(request: &reqwest::Request) -> Option<String> {
        request
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    }

    #[tokio::test]
    async fn applies_inline_api_key_when_no_credentials_file() {
        let path = tmp_creds_path("apikey-only");
        let applier = BitrouterCloudAuthApplier::new(&path).unwrap();
        let target = target_with_api_key("brk_test.abc");
        let request = empty_request();
        let out = applier.apply(request, &target).await.unwrap();
        assert_eq!(bearer(&out).as_deref(), Some("Bearer brk_test.abc"));
    }

    #[tokio::test]
    async fn honours_per_request_api_key_override() {
        let path = tmp_creds_path("override");
        let applier = BitrouterCloudAuthApplier::new(&path).unwrap();
        let mut target = target_with_api_key("brk_base.value");
        target.api_key_override = Some("brk_override.value".into());
        let out = applier.apply(empty_request(), &target).await.unwrap();
        // `effective_api_key` returns the override.
        assert_eq!(bearer(&out).as_deref(), Some("Bearer brk_override.value"));
    }

    #[tokio::test]
    async fn returns_401_with_onboarding_when_no_credential_at_all() {
        let path = tmp_creds_path("nothing");
        let applier = BitrouterCloudAuthApplier::new(&path).unwrap();
        let target = target_with_api_key("");
        let err = applier.apply(empty_request(), &target).await.unwrap_err();
        match err {
            BitrouterError::Upstream { status, message } => {
                assert_eq!(status, 401);
                assert!(message.contains("bitrouter auth login"), "msg={message}");
                assert!(message.contains("BITROUTER_API_KEY"), "msg={message}");
            }
            other => panic!("expected Upstream 401, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn applies_oauth_bearer_when_credentials_present_and_fresh() {
        let path = tmp_creds_path("oauth-fresh");
        let mut store = CredentialsStore::load(&path).unwrap();
        store
            .save(Credentials {
                access_token: "fresh-access".into(),
                refresh_token: Some("rt".into()),
                expires_at: Utc::now() + ChronoDuration::seconds(3600),
                refresh_token_expires_at: None,
                token_type: "Bearer".into(),
                scope: "inference:invoke".into(),
                client_id: "bitrouter-cli".into(),
                authorization_server: "https://as.invalid".into(),
                subject: Some("u-1".into()),
            })
            .unwrap();
        // The AS URL is unreachable, but since the access token is fresh
        // we still hit the metadata endpoint via `current_token`.
        // Stand up a wiremock for the metadata + token endpoints.
        let server = MockServer::start().await;
        let uri = server.uri();
        Mock::given(method("GET"))
            .and(wm_path("/.well-known/oauth-authorization-server"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issuer": uri,
                "device_authorization_endpoint": format!("{uri}/oauth/device_authorization"),
                "token_endpoint": format!("{uri}/oauth/token"),
            })))
            .mount(&server)
            .await;
        // Re-save with the real AS URL.
        store
            .save(Credentials {
                access_token: "fresh-access".into(),
                refresh_token: Some("rt".into()),
                expires_at: Utc::now() + ChronoDuration::seconds(3600),
                refresh_token_expires_at: None,
                token_type: "Bearer".into(),
                scope: "inference:invoke".into(),
                client_id: "bitrouter-cli".into(),
                authorization_server: uri.clone(),
                subject: Some("u-1".into()),
            })
            .unwrap();
        let applier = BitrouterCloudAuthApplier::new(&path).unwrap();
        let target = target_with_api_key("");
        let out = applier.apply(empty_request(), &target).await.unwrap();
        assert_eq!(bearer(&out).as_deref(), Some("Bearer fresh-access"));
    }

    #[tokio::test]
    async fn refreshes_oauth_bearer_within_refresh_window() {
        let server = MockServer::start().await;
        let uri = server.uri();
        Mock::given(method("GET"))
            .and(wm_path("/.well-known/oauth-authorization-server"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issuer": uri,
                "device_authorization_endpoint": format!("{uri}/oauth/device_authorization"),
                "token_endpoint": format!("{uri}/oauth/token"),
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(wm_path("/oauth/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "refreshed-access",
                "token_type": "Bearer",
                "expires_in": 3600,
                "refresh_token": "rotated-rt",
                "scope": "inference:invoke",
            })))
            .mount(&server)
            .await;
        let path = tmp_creds_path("oauth-refresh");
        let mut store = CredentialsStore::load(&path).unwrap();
        // Within the refresh window — `current_token` will exchange.
        store
            .save(Credentials {
                access_token: "stale-access".into(),
                refresh_token: Some("rt-original".into()),
                expires_at: Utc::now() + ChronoDuration::seconds(10),
                refresh_token_expires_at: None,
                token_type: "Bearer".into(),
                scope: "inference:invoke".into(),
                client_id: "bitrouter-cli".into(),
                authorization_server: uri,
                subject: Some("u-1".into()),
            })
            .unwrap();
        let applier = BitrouterCloudAuthApplier::new(&path).unwrap();
        let target = target_with_api_key("");
        let out = applier.apply(empty_request(), &target).await.unwrap();
        assert_eq!(bearer(&out).as_deref(), Some("Bearer refreshed-access"));
        // The rotated refresh token was persisted.
        let reloaded = CredentialsStore::load(&path).unwrap();
        assert_eq!(
            reloaded.current().unwrap().refresh_token.as_deref(),
            Some("rotated-rt")
        );
    }

    #[tokio::test]
    async fn oauth_credential_takes_precedence_over_inline_api_key() {
        let server = MockServer::start().await;
        let uri = server.uri();
        Mock::given(method("GET"))
            .and(wm_path("/.well-known/oauth-authorization-server"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issuer": uri,
                "device_authorization_endpoint": format!("{uri}/oauth/device_authorization"),
                "token_endpoint": format!("{uri}/oauth/token"),
            })))
            .mount(&server)
            .await;
        let path = tmp_creds_path("precedence");
        let mut store = CredentialsStore::load(&path).unwrap();
        store
            .save(Credentials {
                access_token: "oauth-wins".into(),
                refresh_token: Some("rt".into()),
                expires_at: Utc::now() + ChronoDuration::seconds(3600),
                refresh_token_expires_at: None,
                token_type: "Bearer".into(),
                scope: "inference:invoke".into(),
                client_id: "bitrouter-cli".into(),
                authorization_server: uri,
                subject: None,
            })
            .unwrap();
        let applier = BitrouterCloudAuthApplier::new(&path).unwrap();
        // Inline key set but OAuth wins.
        let target = target_with_api_key("brk_should_be_ignored");
        let out = applier.apply(empty_request(), &target).await.unwrap();
        assert_eq!(bearer(&out).as_deref(), Some("Bearer oauth-wins"));
    }

    #[tokio::test]
    async fn metadata_is_cached_across_requests() {
        // The mock counts how many times the well-known metadata endpoint
        // is hit; multiple `apply` calls should produce exactly one fetch.
        let server = MockServer::start().await;
        let uri = server.uri();
        Mock::given(method("GET"))
            .and(wm_path("/.well-known/oauth-authorization-server"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issuer": uri,
                "device_authorization_endpoint": format!("{uri}/oauth/device_authorization"),
                "token_endpoint": format!("{uri}/oauth/token"),
            })))
            .expect(1)
            .mount(&server)
            .await;
        let path = tmp_creds_path("metadata-cache");
        let mut store = CredentialsStore::load(&path).unwrap();
        store
            .save(Credentials {
                access_token: "cached".into(),
                refresh_token: Some("rt".into()),
                expires_at: Utc::now() + ChronoDuration::seconds(3600),
                refresh_token_expires_at: None,
                token_type: "Bearer".into(),
                scope: "inference:invoke".into(),
                client_id: "bitrouter-cli".into(),
                authorization_server: uri,
                subject: None,
            })
            .unwrap();
        let applier = BitrouterCloudAuthApplier::new(&path).unwrap();
        let target = target_with_api_key("");
        for _ in 0..3 {
            let _ = applier.apply(empty_request(), &target).await.unwrap();
        }
        // `expect(1)` on the mock asserts on drop that exactly one
        // metadata fetch happened across the three requests.
        drop(server);
    }
}
