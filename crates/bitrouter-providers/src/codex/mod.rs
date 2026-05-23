//! OpenAI Codex — `AuthApplier` for the ChatGPT-subscription Codex route.
//!
//! Distinct from the `openai` provider: this targets
//! `chatgpt.com/backend-api/codex` (Responses-only) using an OAuth access
//! token minted by the `bitrouter login openai-codex` flow against
//! `auth.openai.com`. The ChatGPT subscription credential does **not**
//! authenticate to `api.openai.com`, so a separate provider id is the
//! cleanest model.
//!
//! Per-request:
//! 1. Read `(openai-codex, target.account_label)` from the credential
//!    store. Must be a `Credential::Oauth` — no API-key path here.
//! 2. Refresh if the access token is within
//!    [`crate::oauth::refresh::REFRESH_WINDOW`] of expiry.
//! 3. Decode the access token JWT to extract `chatgpt_account_id` and
//!    forward it on the `chatgpt-account-id` header alongside the Bearer.
//! 4. Set `OpenAI-Beta: responses=experimental` and `originator: bitrouter`
//!    so the upstream admits the request through the Codex pipeline.
//!
//! ## Body shape — known gap
//!
//! Same caveat as the Anthropic OAuth applier: subscription endpoints
//! expect first-party-CLI-shaped bodies (specific `instructions` /
//! `store` fields). This module only places credentials + integration
//! headers; body shaping is a follow-up. Until then, requests will
//! authenticate but the upstream may reject them on body grounds.

pub mod headers;
pub mod jwt;

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};

use bitrouter_sdk::language_model::AuthApplier;
use bitrouter_sdk::language_model::types::RoutingTarget;
use bitrouter_sdk::{BitrouterError, Result};

use crate::oauth::auth_code::AuthCodeError;
use crate::oauth::credential_store::{
    Credential, CredentialStore, DEFAULT_LABEL, OAuthToken,
};
use crate::oauth::refresh::{needs_refresh, refresh};

/// Provider id this applier is registered under.
pub const PROVIDER_ID: &str = "openai-codex";

/// `AuthApplier` for `provider_name == "openai-codex"`.
pub struct OpenAiCodexAuthApplier {
    store_path: std::path::PathBuf,
    refresh_client: reqwest::Client,
    client_id: String,
    token_endpoint: String,
    cache: Arc<Mutex<std::collections::HashMap<String, OAuthToken>>>,
}

impl OpenAiCodexAuthApplier {
    /// Build an applier reading the credential store at `store_path` and
    /// using the registry's default Codex OAuth client + token endpoint.
    pub fn new(store_path: impl Into<std::path::PathBuf>) -> Result<Self> {
        let registry = crate::oauth::registry::find(PROVIDER_ID).ok_or_else(|| {
            BitrouterError::internal(
                "openai-codex PKCE registry entry is missing — build-time bug".to_string(),
            )
        })?;
        let refresh_client = reqwest::Client::builder()
            .user_agent(concat!("bitrouter-providers/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| {
                BitrouterError::internal(format!(
                    "building Codex OAuth refresh HTTP client: {e}"
                ))
            })?;
        Ok(Self {
            store_path: store_path.into(),
            refresh_client,
            client_id: registry.auth.client_id,
            token_endpoint: registry.auth.token_endpoint,
            cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
        })
    }

    /// Tests override the refresh client + endpoint.
    #[cfg(test)]
    pub fn with_client_and_endpoint(
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
        }
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
                    "no openai-codex credential for label '{label}' — \
                     run `bitrouter login openai-codex`"
                ),
            })?;
        let token = stored.as_oauth().cloned().ok_or_else(|| {
            BitrouterError::Upstream {
                status: 401,
                message: format!(
                    "openai-codex credential for '{label}' is an API key — \
                     this provider only accepts subscription OAuth tokens"
                ),
            }
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
                BitrouterError::internal(format!(
                    "persisting refreshed openai-codex OAuth token: {e}"
                ))
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
                "openai-codex OAuth refresh failed ({error}{}). Re-run `bitrouter login openai-codex`.",
                description.map(|d| format!(": {d}")).unwrap_or_default()
            ),
        },
        other => BitrouterError::Upstream {
            status: 502,
            message: format!("openai-codex OAuth refresh transport error: {other}"),
        },
    }
}

#[async_trait]
impl AuthApplier for OpenAiCodexAuthApplier {
    async fn apply(
        &self,
        mut request: reqwest::Request,
        target: &RoutingTarget,
    ) -> Result<reqwest::Request> {
        let label = self.label_for(target);
        let token = self.resolve_token(label).await?;
        // The ChatGPT-account-id is namespaced inside the JWT; if the JWT
        // doesn't carry it (test fixtures, an unrelated token) we still
        // attach the Bearer — the upstream will reject and we'll see why.
        // Logging the decode error rather than failing the request keeps
        // a known-incomplete claim from breaking unrelated requests.
        let account_id = jwt::decode_codex_claims(&token.access_token)
            .ok()
            .and_then(|c| c.chatgpt_account_id);
        let bearer = format!("Bearer {}", token.access_token);
        let auth = HeaderValue::from_str(&bearer).map_err(|e| {
            BitrouterError::internal(format!("invalid Codex bearer for Authorization: {e}"))
        })?;
        let headers_mut = request.headers_mut();
        headers_mut.insert(reqwest::header::AUTHORIZATION, auth);
        if let Some(account_id) = account_id {
            let value = HeaderValue::from_str(&account_id).map_err(|e| {
                BitrouterError::internal(format!("invalid chatgpt-account-id header: {e}"))
            })?;
            headers_mut.insert(HeaderName::from_static("chatgpt-account-id"), value);
        }
        headers_mut.insert(
            HeaderName::from_static("openai-beta"),
            HeaderValue::from_static(headers::OPENAI_BETA),
        );
        headers_mut.insert(
            HeaderName::from_static("originator"),
            HeaderValue::from_static(headers::ORIGINATOR),
        );
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use bitrouter_sdk::language_model::types::ApiProtocol;
    use wiremock::MockServer;

    use super::*;

    fn tmp_store_path() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-codex-test-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("creds.json")
    }

    fn codex_target(label: Option<&str>) -> RoutingTarget {
        RoutingTarget {
            provider_name: PROVIDER_ID.to_string(),
            service_id: "gpt-5-codex".to_string(),
            api_base: "https://chatgpt.com/backend-api/codex".to_string(),
            api_key: String::new(),
            api_protocol: ApiProtocol::Responses,
            account_label: label.map(String::from),
            api_key_override: None,
            api_base_override: None,
        }
    }

    fn make_jwt_with_account(account_id: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode("{}");
        let payload = URL_SAFE_NO_PAD.encode(format!(
            r#"{{"exp":1700000000,"https://api.openai.com/auth":{{"chatgpt_account_id":"{account_id}"}}}}"#
        ));
        let sig = URL_SAFE_NO_PAD.encode("sig");
        format!("{header}.{payload}.{sig}")
    }

    #[tokio::test]
    async fn applies_bearer_account_id_and_integration_headers() {
        let path = tmp_store_path();
        let jwt = make_jwt_with_account("acct-bitrouter");
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set(
                    PROVIDER_ID,
                    DEFAULT_LABEL,
                    Credential::from_oauth_token(OAuthToken {
                        access_token: jwt.clone(),
                        expires_at: 0, // non-expiring → no refresh attempt
                        refresh_token: Some("r".into()),
                    }),
                )
                .unwrap();
        }
        let server = MockServer::start().await;
        let applier = OpenAiCodexAuthApplier::with_client_and_endpoint(
            &path,
            reqwest::Client::new(),
            "client-1",
            format!("{}/oauth/token", server.uri()),
        );
        let req = reqwest::Client::new()
            .post("https://chatgpt.com/backend-api/codex/responses")
            .build()
            .unwrap();
        let authed = applier.apply(req, &codex_target(None)).await.unwrap();
        let h = authed.headers();
        assert_eq!(
            h.get(reqwest::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some(format!("Bearer {jwt}").as_str())
        );
        assert_eq!(
            h.get("chatgpt-account-id").and_then(|v| v.to_str().ok()),
            Some("acct-bitrouter")
        );
        assert_eq!(
            h.get("openai-beta").and_then(|v| v.to_str().ok()),
            Some(headers::OPENAI_BETA)
        );
        assert_eq!(
            h.get("originator").and_then(|v| v.to_str().ok()),
            Some(headers::ORIGINATOR)
        );
    }

    #[tokio::test]
    async fn fails_when_no_credential_stored() {
        let path = tmp_store_path();
        let applier = OpenAiCodexAuthApplier::new(&path).unwrap();
        let req = reqwest::Client::new()
            .post("https://chatgpt.com/backend-api/codex/responses")
            .build()
            .unwrap();
        let err = applier
            .apply(req, &codex_target(None))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bitrouter login openai-codex"),
            "expected helpful hint, got: {msg}"
        );
    }

    #[tokio::test]
    async fn rejects_api_key_credential() {
        let path = tmp_store_path();
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set(PROVIDER_ID, DEFAULT_LABEL, Credential::api_key("sk-..."))
                .unwrap();
        }
        let applier = OpenAiCodexAuthApplier::new(&path).unwrap();
        let req = reqwest::Client::new()
            .post("https://chatgpt.com/backend-api/codex/responses")
            .build()
            .unwrap();
        let err = applier
            .apply(req, &codex_target(None))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("subscription OAuth"),
            "expected API-key rejection, got: {msg}"
        );
    }

    #[tokio::test]
    async fn omits_account_id_header_when_jwt_lacks_claim() {
        let path = tmp_store_path();
        // Plain non-JWT string — claim decode fails gracefully and the
        // applier still sets the Bearer.
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set(
                    PROVIDER_ID,
                    DEFAULT_LABEL,
                    Credential::from_oauth_token(OAuthToken {
                        access_token: "not-a-jwt".into(),
                        expires_at: 0,
                        refresh_token: None,
                    }),
                )
                .unwrap();
        }
        let applier = OpenAiCodexAuthApplier::new(&path).unwrap();
        let req = reqwest::Client::new()
            .post("https://chatgpt.com/backend-api/codex/responses")
            .build()
            .unwrap();
        let authed = applier.apply(req, &codex_target(None)).await.unwrap();
        assert!(authed.headers().get("chatgpt-account-id").is_none());
        assert!(authed.headers().get(reqwest::header::AUTHORIZATION).is_some());
    }
}
