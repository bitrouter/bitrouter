//! GitHub Copilot — `AuthApplier` plus the GitHub→Copilot token exchange.
//!
//! Two-step authentication:
//!
//! 1. The user runs `bitrouter login github-copilot` once, which drives the
//!    OAuth Device Authorization Grant against `github.com` and stores a
//!    long-lived GitHub user-to-server access token (e.g. `ghu_…`) in the
//!    [`crate::oauth::credential_store::CredentialStore`].
//! 2. At request time, [`CopilotAuthApplier`] reads that GitHub token and
//!    exchanges it for a short-lived Copilot "internal" token via
//!    `GET https://api.github.com/copilot_internal/v2/token`. The Copilot
//!    token (cached until `expires_at - 60s`) is what
//!    `api.githubcopilot.com` actually accepts as a Bearer.
//!
//! Authoritative references:
//! - Copilot REST endpoints landscape:
//!   <https://docs.github.com/en/copilot/reference/api-reference/copilot-api-endpoints>
//! - VS Code Copilot Chat (MIT) reads the same `copilot_internal/v2/token`
//!   endpoint to obtain the Bearer used against `api.githubcopilot.com`:
//!   <https://github.com/microsoft/vscode-copilot-chat>
//! - opencode's reference implementation of the same exchange (TypeScript):
//!   <https://github.com/sst/opencode/blob/dev/packages/opencode/src/auth/copilot.ts>
//!
//! Integration headers that the Copilot API requires alongside the Bearer
//! are produced by [`headers::copilot_request_headers`].

pub mod exchange;
pub mod headers;

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};

use bitrouter_sdk::language_model::AuthApplier;
use bitrouter_sdk::language_model::types::RoutingTarget;
use bitrouter_sdk::{BitrouterError, Result};

use crate::oauth::credential_store::{CredentialStore, DEFAULT_LABEL, OAuthToken};
use exchange::{CopilotToken, TOKEN_EXCHANGE_URL, exchange_for_copilot_token_at};

/// Provider id used throughout the codebase. Matches the TOML filename stem.
pub const PROVIDER_ID: &str = "github-copilot";

/// `AuthApplier` for `provider_name == "github-copilot"`.
///
/// On every request:
/// 1. Read the cached `CopilotToken` (in-memory).
/// 2. If absent or expired (within 60s of `expires_at`), look up the stored
///    `OAuthToken` for `github-copilot` in the on-disk token store and POST
///    the exchange against `api.github.com`.
/// 3. Apply `Authorization: Bearer <copilot_token>` + the Copilot integration
///    headers to the outbound request.
pub struct CopilotAuthApplier {
    /// HTTP client used for the token exchange. Separate from the upstream
    /// HTTP client because the exchange targets `api.github.com`, not
    /// `api.githubcopilot.com`.
    exchange_client: reqwest::Client,
    /// Override of the exchange URL (tests). `None` → use the production
    /// constant `TOKEN_EXCHANGE_URL`.
    exchange_url: Option<String>,
    /// Source of the GitHub OAuth access token (`ghu_…`).
    token_store_path: std::path::PathBuf,
    /// In-memory cache of the last-issued Copilot token.
    cache: Arc<Mutex<Option<CopilotToken>>>,
}

impl CopilotAuthApplier {
    /// Build an applier reading the GitHub OAuth token from `token_store_path`.
    pub fn new(token_store_path: impl Into<std::path::PathBuf>) -> Result<Self> {
        let exchange_client = reqwest::Client::builder()
            .user_agent(concat!("bitrouter-providers/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| {
                BitrouterError::internal(format!("building Copilot exchange HTTP client: {e}"))
            })?;
        Ok(Self {
            exchange_client,
            exchange_url: None,
            token_store_path: token_store_path.into(),
            cache: Arc::new(Mutex::new(None)),
        })
    }

    /// Build an applier with an explicit reqwest client and exchange URL
    /// (tests use this against `wiremock`).
    pub fn with_client_and_url(
        exchange_client: reqwest::Client,
        exchange_url: impl Into<String>,
        token_store_path: impl Into<std::path::PathBuf>,
    ) -> Self {
        Self {
            exchange_client,
            exchange_url: Some(exchange_url.into()),
            token_store_path: token_store_path.into(),
            cache: Arc::new(Mutex::new(None)),
        }
    }

    fn cached_if_fresh(&self) -> Option<CopilotToken> {
        let guard = self.cache.lock().ok()?;
        let token = guard.as_ref()?;
        token.is_fresh().then(|| token.clone())
    }

    fn store_in_cache(&self, token: &CopilotToken) {
        if let Ok(mut guard) = self.cache.lock() {
            *guard = Some(token.clone());
        }
    }

    fn read_github_token(&self) -> Result<OAuthToken> {
        let store = CredentialStore::load(&self.token_store_path).map_err(|e| {
            BitrouterError::internal(format!(
                "reading credential store at {}: {e}",
                self.token_store_path.display()
            ))
        })?;
        // The github-copilot login stores a single OAuth credential under
        // the default label — no multi-account support, because the
        // upstream user-to-server token doesn't carry an account identifier
        // the downstream pipeline could fan out on.
        store
            .get_any(PROVIDER_ID, DEFAULT_LABEL)
            .and_then(|c| c.as_oauth().cloned())
            .ok_or_else(|| BitrouterError::Upstream {
                status: 401,
                message: "no GitHub Copilot OAuth token — run `bitrouter login github-copilot`"
                    .to_string(),
            })
    }

    /// Resolve a current Copilot Bearer token, doing the exchange + caching
    /// it on demand. Exposed for tests; the production path is [`Self::apply`].
    pub async fn obtain_copilot_token(&self) -> Result<CopilotToken> {
        if let Some(cached) = self.cached_if_fresh() {
            return Ok(cached);
        }
        let github = self.read_github_token()?;
        let url = self.exchange_url.as_deref().unwrap_or(TOKEN_EXCHANGE_URL);
        let token = exchange_for_copilot_token_at(&self.exchange_client, url, &github.access_token)
            .await
            .map_err(|e| BitrouterError::Upstream {
                status: 502,
                message: format!("GitHub→Copilot token exchange failed: {e}"),
            })?;
        self.store_in_cache(&token);
        Ok(token)
    }

    /// Replace the in-memory cached Copilot token. Tests use this to seed a
    /// known-fresh token without touching the network.
    #[cfg(test)]
    pub fn cache_for_test(&self, token: CopilotToken) {
        self.store_in_cache(&token);
    }
}

#[async_trait]
impl AuthApplier for CopilotAuthApplier {
    async fn apply(
        &self,
        mut request: reqwest::Request,
        _target: &RoutingTarget,
    ) -> Result<reqwest::Request> {
        let token = self.obtain_copilot_token().await?;
        let bearer = format!("Bearer {}", token.token);
        let value = HeaderValue::from_str(&bearer).map_err(|e| {
            BitrouterError::internal(format!("invalid Copilot bearer for Authorization: {e}"))
        })?;
        request
            .headers_mut()
            .insert(reqwest::header::AUTHORIZATION, value);
        for (name, value) in headers::copilot_request_headers() {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                BitrouterError::internal(format!("invalid Copilot header name '{name}': {e}"))
            })?;
            let value = HeaderValue::from_str(&value).map_err(|e| {
                BitrouterError::internal(format!("invalid Copilot header value: {e}"))
            })?;
            request.headers_mut().insert(name, value);
        }
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use bitrouter_sdk::language_model::types::ApiProtocol;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::oauth::credential_store::{Credential, CredentialStore};

    fn tmp_token_store_path() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-copilot-test-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("tokens.json")
    }

    fn copilot_target() -> RoutingTarget {
        RoutingTarget {
            provider_name: PROVIDER_ID.to_string(),
            service_id: "claude-sonnet-4.6".to_string(),
            api_base: "https://api.githubcopilot.com".to_string(),
            api_key: String::new(),
            api_protocol: ApiProtocol::Messages,
            account_label: None,
            api_key_override: None,
            api_base_override: None,
        }
    }

    #[tokio::test]
    async fn fails_when_no_oauth_token_stored() {
        let store_path = tmp_token_store_path();
        let applier = CopilotAuthApplier::new(&store_path).unwrap();
        let req = reqwest::Client::new()
            .post("https://api.githubcopilot.com/v1/messages")
            .build()
            .unwrap();
        let err = applier.apply(req, &copilot_target()).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bitrouter login github-copilot"),
            "expected helpful hint, got: {msg}"
        );
    }

    #[tokio::test]
    async fn exchanges_github_token_and_injects_headers() {
        // Seed a stored GitHub OAuth token.
        let store_path = tmp_token_store_path();
        let mut store = CredentialStore::load(&store_path).unwrap();
        store
            .set(
                PROVIDER_ID,
                DEFAULT_LABEL,
                Credential::from_oauth_token(OAuthToken {
                    access_token: "ghu_test_github_oauth".into(),
                    expires_at: 0,
                    refresh_token: None,
                }),
            )
            .unwrap();

        // Mock the GitHub → Copilot token exchange endpoint.
        let server = MockServer::start().await;
        let future_expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        Mock::given(method("GET"))
            .and(path("/copilot_internal/v2/token"))
            .and(header("authorization", "token ghu_test_github_oauth"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "tid=copilot-bearer;exp=zzz",
                "expires_at": future_expiry,
                "refresh_in": 1500,
                "chat_enabled": true
            })))
            .expect(1)
            .mount(&server)
            .await;

        let applier = CopilotAuthApplier::with_client_and_url(
            reqwest::Client::new(),
            format!("{}/copilot_internal/v2/token", server.uri()),
            &store_path,
        );
        let req = reqwest::Client::new()
            .post("https://api.githubcopilot.com/v1/messages")
            .build()
            .unwrap();
        let authed = applier.apply(req, &copilot_target()).await.unwrap();
        let headers = authed.headers();
        assert_eq!(
            headers
                .get(reqwest::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer tid=copilot-bearer;exp=zzz")
        );
        assert!(headers.get("editor-version").is_some());
        assert!(headers.get("copilot-integration-id").is_some());
    }

    #[tokio::test]
    async fn cached_fresh_token_skips_exchange() {
        let store_path = tmp_token_store_path();
        let server = MockServer::start().await;
        // No mounts → if the applier hit the exchange endpoint the request
        // would 404 and the test would fail.

        let applier = CopilotAuthApplier::with_client_and_url(
            reqwest::Client::new(),
            format!("{}/copilot_internal/v2/token", server.uri()),
            &store_path,
        );

        let future_expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        applier.cache_for_test(CopilotToken {
            token: "tid=cached".into(),
            expires_at: future_expiry,
        });

        let req = reqwest::Client::new()
            .post("https://api.githubcopilot.com/v1/messages")
            .build()
            .unwrap();
        let authed = applier.apply(req, &copilot_target()).await.unwrap();
        assert_eq!(
            authed
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer tid=cached")
        );
    }
}
