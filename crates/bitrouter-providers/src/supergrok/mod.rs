//! SuperGrok — the xAI **subscription** `AuthApplier` (grok.com / X Premium+).
//!
//! Registered under the provider id `"supergrok"`. This applier resolves an
//! OAuth credential minted by the official Grok CLI's SuperGrok login and
//! shapes the request as a subscription call: `Authorization: Bearer <jwt>`
//! against `https://api.x.ai/v1`. Distinct from the `xai` provider, which is
//! the metered API-key path (`XAI_API_KEY`) — the subscription credential is a
//! different grant that xAI host-locks to `*.x.ai`.
//!
//! | Stored credential | Outbound headers |
//! |---|---|
//! | `Credential::Oauth` (SuperGrok) | `Authorization: Bearer <access_token>`. |
//! | _no credential in store_        | `401` — there is no API-key fallback here (use the `xai` provider for that). |
//!
//! The credential is obtained by importing the Grok CLI's own session
//! (`~/.grok/auth.json`, the OIDC entry) via
//! [`crate::import::grok`] — mirroring the Codex import. The access token is a
//! JWT the xAI API accepts as a Bearer; the applier refreshes it via the public
//! OIDC client against `https://auth.x.ai/oauth2/token` when it is within
//! [`crate::oauth::refresh::REFRESH_WINDOW`] of expiry, writes the new token
//! back to the store, and caches it in memory.
//!
//! ## Auth client
//!
//! SuperGrok's OIDC client is public (PKCE, no client secret), so refresh needs
//! only the client id — the standard [`crate::oauth::refresh::refresh`] path.
//! The client id + token endpoint are the ones the official Grok CLI ships
//! with (confirmed in `~/.grok/auth.json`, keyed `<issuer>::<client_id>`). A
//! native browser login could be added later via [`crate::oauth::registry`];
//! today the only path is importing the Grok CLI session.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::HeaderValue;

use bitrouter_sdk::language_model::AuthApplier;
use bitrouter_sdk::language_model::types::RoutingTarget;
use bitrouter_sdk::{BitrouterError, Result};

use crate::oauth::auth_code::AuthCodeError;
use crate::oauth::credential_store::{Credential, CredentialStore, DEFAULT_LABEL, OAuthToken};
use crate::oauth::refresh::{needs_refresh, refresh};

/// Provider id this applier is registered under.
pub const PROVIDER_ID: &str = "supergrok";

/// SuperGrok's public OIDC client id — the one the official Grok CLI ships with
/// (confirmed as the `<issuer>::<client_id>` key in `~/.grok/auth.json`).
const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";

/// OIDC token endpoint for the refresh-token grant (from `auth.x.ai`'s
/// `/.well-known/openid-configuration`).
const TOKEN_ENDPOINT: &str = "https://auth.x.ai/oauth2/token";

/// `AuthApplier` for `provider_name == "supergrok"`.
pub struct SuperGrokAuthApplier {
    store_path: std::path::PathBuf,
    refresh_client: reqwest::Client,
    client_id: String,
    token_endpoint: String,
    cache: Arc<Mutex<std::collections::HashMap<String, OAuthToken>>>,
    /// Per-label single-flight gate around disk-read → refresh → persist. See
    /// [`crate::claude_code::ClaudeCodeAuthApplier`] for the rationale
    /// (concurrent refreshes can have the older refresh_token invalidated per
    /// RFC 6749 §6).
    refresh_gates: Arc<Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl SuperGrokAuthApplier {
    /// Build an applier reading the credential store at `store_path` and using
    /// SuperGrok's public OIDC client + token endpoint.
    pub fn new(store_path: impl Into<std::path::PathBuf>) -> Result<Self> {
        let refresh_client = reqwest::Client::builder()
            .user_agent(concat!("bitrouter-providers/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| {
                BitrouterError::internal(format!(
                    "building SuperGrok OAuth refresh HTTP client: {e}"
                ))
            })?;
        Ok(Self {
            store_path: store_path.into(),
            refresh_client,
            client_id: CLIENT_ID.to_string(),
            token_endpoint: TOKEN_ENDPOINT.to_string(),
            cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
            refresh_gates: Arc::new(Mutex::new(std::collections::HashMap::new())),
        })
    }

    /// Tests override the refresh client + endpoint.
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

    /// Per-label single-flight gate — same shape as the Codex applier.
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

    async fn resolve_token(&self, label: &str) -> Result<OAuthToken> {
        // 1. Lock-free cache hit.
        if let Some(cached) = self.cached_fresh(label) {
            return Ok(cached);
        }
        // 2. Acquire the per-label gate before any disk or network work.
        let gate = self.refresh_gate(label);
        let _guard = gate.lock().await;
        // 3. Double-checked locking — another task may have refreshed while we
        //    were waiting on the gate.
        if let Some(cached) = self.cached_fresh(label) {
            return Ok(cached);
        }
        let store = CredentialStore::load(&self.store_path).map_err(|e| {
            BitrouterError::internal(format!(
                "reading credential store at {}: {e}",
                self.store_path.display()
            ))
        })?;
        let stored = store
            .get_any(PROVIDER_ID, label)
            .ok_or_else(|| BitrouterError::Upstream {
                status: 401,
                message: format!(
                    "no supergrok credential for label '{label}' — \
                     run `bitrouter providers login supergrok` (imports your Grok CLI session)"
                ),
            })?;
        let token = stored
            .as_oauth()
            .cloned()
            .ok_or_else(|| BitrouterError::Upstream {
                status: 401,
                message: format!(
                    "supergrok credential for '{label}' is an API key — this provider only \
                     accepts subscription OAuth tokens (use the `xai` provider for an API key)"
                ),
            })?;
        if needs_refresh(&token) {
            let refreshed = refresh(
                &self.refresh_client,
                &self.token_endpoint,
                &self.client_id,
                &token,
            )
            .await
            .map_err(refresh_to_bitrouter_error)?;
            self.persist_refreshed(label, refreshed.clone())?;
            self.store_in_cache(label, &refreshed);
            return Ok(refreshed);
        }
        self.store_in_cache(label, &token);
        Ok(token)
    }

    fn persist_refreshed(&self, label: &str, token: OAuthToken) -> Result<()> {
        let mut store = CredentialStore::load(&self.store_path).map_err(|e| {
            BitrouterError::internal(format!(
                "reloading credential store before refresh write-back: {e}"
            ))
        })?;
        store
            .set(PROVIDER_ID, label, Credential::from_oauth_token(token))
            .map_err(|e| {
                BitrouterError::internal(format!("persisting refreshed supergrok OAuth token: {e}"))
            })?;
        Ok(())
    }

    fn label_for<'a>(&self, target: &'a RoutingTarget) -> &'a str {
        target.account_label.as_deref().unwrap_or(DEFAULT_LABEL)
    }
}

fn refresh_to_bitrouter_error(e: AuthCodeError) -> BitrouterError {
    match e {
        AuthCodeError::OAuthError { error, description } => BitrouterError::Upstream {
            status: 401,
            message: format!(
                "supergrok OAuth refresh failed ({error}{}). Re-run `bitrouter providers login supergrok`.",
                description.map(|d| format!(": {d}")).unwrap_or_default()
            ),
        },
        other => BitrouterError::Upstream {
            status: 502,
            message: format!("supergrok OAuth refresh transport error: {other}"),
        },
    }
}

#[async_trait]
impl AuthApplier for SuperGrokAuthApplier {
    async fn apply(
        &self,
        mut request: reqwest::Request,
        target: &RoutingTarget,
    ) -> Result<reqwest::Request> {
        let label = self.label_for(target);
        let token = self.resolve_token(label).await?;
        let bearer = format!("Bearer {}", token.access_token);
        let auth = HeaderValue::from_str(&bearer).map_err(|e| {
            BitrouterError::internal(format!("invalid SuperGrok bearer for Authorization: {e}"))
        })?;
        request
            .headers_mut()
            .insert(reqwest::header::AUTHORIZATION, auth);
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use bitrouter_sdk::language_model::types::ApiProtocol;
    use wiremock::MockServer;

    use super::*;

    fn tmp_store_path() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-supergrok-test-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("creds.json")
    }

    fn supergrok_target(label: Option<&str>) -> RoutingTarget {
        RoutingTarget {
            provider_name: PROVIDER_ID.to_string(),
            service_id: "grok-build-0.1".to_string(),
            api_base: "https://api.x.ai/v1".to_string(),
            api_key: String::new(),
            api_protocol: ApiProtocol::Responses,
            chat_token_limit_field: None,
            account_label: label.map(String::from),
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
        }
    }

    #[tokio::test]
    async fn applies_bearer_from_stored_oauth() {
        let path = tmp_store_path();
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set(
                    PROVIDER_ID,
                    DEFAULT_LABEL,
                    Credential::from_oauth_token(OAuthToken {
                        access_token: "grok-jwt-fresh".into(),
                        expires_at: 0, // non-expiring → no refresh attempt
                        refresh_token: Some("r".into()),
                    }),
                )
                .unwrap();
        }
        let applier = SuperGrokAuthApplier::new(&path).unwrap();
        let req = reqwest::Client::new()
            .post("https://api.x.ai/v1/responses")
            .build()
            .unwrap();
        let authed = applier.apply(req, &supergrok_target(None)).await.unwrap();
        assert_eq!(
            authed
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer grok-jwt-fresh")
        );
    }

    #[tokio::test]
    async fn fails_when_no_credential_stored() {
        let path = tmp_store_path();
        let applier = SuperGrokAuthApplier::new(&path).unwrap();
        let req = reqwest::Client::new()
            .post("https://api.x.ai/v1/responses")
            .build()
            .unwrap();
        let err = applier
            .apply(req, &supergrok_target(None))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("bitrouter providers login supergrok"),
            "expected helpful hint, got: {err}"
        );
    }

    #[tokio::test]
    async fn rejects_api_key_credential() {
        let path = tmp_store_path();
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set(PROVIDER_ID, DEFAULT_LABEL, Credential::api_key("xai-..."))
                .unwrap();
        }
        let applier = SuperGrokAuthApplier::new(&path).unwrap();
        let req = reqwest::Client::new()
            .post("https://api.x.ai/v1/responses")
            .build()
            .unwrap();
        let err = applier
            .apply(req, &supergrok_target(None))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("subscription OAuth"),
            "expected API-key rejection, got: {err}"
        );
    }

    #[tokio::test]
    async fn fresh_token_skips_refresh() {
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
        // Point refresh at a wiremock with no mounts → any hit 404s and fails.
        let server = MockServer::start().await;
        let applier = SuperGrokAuthApplier::with_client_and_endpoint(
            &path,
            reqwest::Client::new(),
            "client-1",
            format!("{}/oauth/token", server.uri()),
        );
        let req = reqwest::Client::new()
            .post("https://api.x.ai/v1/responses")
            .build()
            .unwrap();
        let authed = applier.apply(req, &supergrok_target(None)).await.unwrap();
        assert_eq!(
            authed
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer still-fresh")
        );
    }
}
