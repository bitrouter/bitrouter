//! Antigravity (Google) — the `agy` **subscription** integration that powers the
//! `google-ai` provider ([`PROVIDER_ID`]; there is no separate `antigravity`
//! provider — see `registry/providers/google-ai.yaml`).
//!
//! Routes the user's Google Antigravity session (imported from the `agy` CLI,
//! see [`crate::import::antigravity`]) to Google's Code Assist backend
//! (`cloudcode-pa.googleapis.com/v1internal:*`). Three parts:
//!
//! - [`agy_client`] — the OAuth client: a pinned public id plus the confidential
//!   `GOCSPX-…` secret read from the local `agy` binary at refresh time (never
//!   vendored here).
//! - [`protocol`] — an [`AuthApplier`]-adjacent
//!   `Adapter`/`Transport` pair registered under the `Custom("antigravity")`
//!   protocol. It reuses the Gemini `generateContent` translation but targets
//!   the `v1internal:{verb}` method endpoint and unwraps the `{"response": …}`
//!   envelope cloudcode-pa wraps replies in.
//! - the [`AntigravityAuthApplier`] — sets the Bearer + first-party spoof
//!   headers, resolves the project id via a cached `loadCodeAssist` bootstrap,
//!   and wraps the request body in the `{model, project, request}` envelope.

pub mod agy_client;
pub mod protocol;

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
use crate::oauth::refresh::{needs_refresh, refresh_with_client_secret};

/// Provider id this applier is registered under. The `google-ai` subscription
/// provider is powered by this Antigravity (`agy` / cloudcode-pa) integration —
/// there is no separate `antigravity` provider.
pub const PROVIDER_ID: &str = "google-ai";

/// `User-Agent` version we present as. cloudcode-pa is lenient about the exact
/// version, but the `antigravity/*` shape is what admits the request to the
/// Antigravity model set. Matches the current `agy` release line.
const AGY_VERSION: &str = "1.1.0";

/// `loadCodeAssist` request body — the minimal shape that returns the project.
const LOAD_CODE_ASSIST_BODY: &str = r#"{"metadata":{"pluginType":"GEMINI"}}"#;

/// `AuthApplier` for `provider_name == "antigravity"`.
///
/// Per request: resolve the Google OAuth Bearer (refreshing via the confidential
/// `agy` client when stale), resolve the Code Assist project id (cached
/// `loadCodeAssist` bootstrap), wrap the body in the `{model, project, request}`
/// envelope, and set the first-party spoof headers. The `v1internal:{verb}` URL
/// and the `{"response": …}` unwrap are handled by [`protocol`].
pub struct AntigravityAuthApplier {
    store_path: std::path::PathBuf,
    http: reqwest::Client,
    /// `label -> freshest OAuthToken`.
    token_cache: Arc<Mutex<std::collections::HashMap<String, OAuthToken>>>,
    /// `label -> Code Assist project id` (from `loadCodeAssist`).
    project_cache: Arc<Mutex<std::collections::HashMap<String, String>>>,
    /// The `GOCSPX-…` secret that last refreshed successfully, cached to avoid
    /// re-scanning the `agy` binary and re-trying every candidate.
    working_secret: Arc<Mutex<Option<String>>>,
    /// Per-label single-flight gate around the disk-read → refresh → persist
    /// sequence (RFC 6749 §6 refresh-token rotation).
    refresh_gates: Arc<Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl AntigravityAuthApplier {
    /// Build an applier reading + writing the credential store at `store_path`.
    pub fn new(store_path: impl Into<std::path::PathBuf>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("bitrouter-providers/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| {
                BitrouterError::internal(format!("building Antigravity HTTP client: {e}"))
            })?;
        Ok(Self {
            store_path: store_path.into(),
            http,
            token_cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
            project_cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
            working_secret: Arc::new(Mutex::new(None)),
            refresh_gates: Arc::new(Mutex::new(std::collections::HashMap::new())),
        })
    }

    fn label_for<'a>(&self, target: &'a RoutingTarget) -> &'a str {
        target.account_label.as_deref().unwrap_or(DEFAULT_LABEL)
    }

    fn refresh_gate(&self, label: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut guard = self.refresh_gates.lock().unwrap_or_else(|p| p.into_inner());
        guard
            .entry(label.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn cached_fresh(&self, label: &str) -> Option<OAuthToken> {
        let guard = self.token_cache.lock().ok()?;
        let token = guard.get(label)?;
        (!needs_refresh(token)).then(|| token.clone())
    }

    fn store_in_cache(&self, label: &str, token: &OAuthToken) {
        if let Ok(mut guard) = self.token_cache.lock() {
            guard.insert(label.to_string(), token.clone());
        }
    }

    /// Resolve a fresh Bearer for `label`, refreshing through the confidential
    /// `agy` client when the stored token is near expiry.
    async fn resolve_token(&self, label: &str) -> Result<OAuthToken> {
        if let Some(cached) = self.cached_fresh(label) {
            return Ok(cached);
        }
        let gate = self.refresh_gate(label);
        let _guard = gate.lock().await;
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
            .and_then(|c| c.as_oauth().cloned())
            .ok_or_else(|| BitrouterError::Upstream {
                status: 401,
                message: format!(
                    "no google-ai credential for label '{label}' — \
                     run `bitrouter providers login google-ai` (imports your `agy` session)"
                ),
            })?;
        if needs_refresh(&stored) {
            let refreshed = self.refresh_via_agy(&stored).await?;
            self.persist_refreshed(label, refreshed.clone())?;
            self.store_in_cache(label, &refreshed);
            return Ok(refreshed);
        }
        self.store_in_cache(label, &stored);
        Ok(stored)
    }

    /// Refresh the Google OAuth token using the confidential `agy` client. The
    /// client secret is read from the local `agy` binary; `agy` embeds more than
    /// one, so each candidate is tried until the grant succeeds (then cached).
    async fn refresh_via_agy(&self, token: &OAuthToken) -> Result<OAuthToken> {
        let secrets = self.secret_candidates()?;
        let mut last: Option<AuthCodeError> = None;
        for secret in secrets {
            match refresh_with_client_secret(
                &self.http,
                agy_client::TOKEN_ENDPOINT,
                agy_client::CLIENT_ID,
                &secret,
                token,
            )
            .await
            {
                Ok(refreshed) => {
                    if let Ok(mut guard) = self.working_secret.lock() {
                        *guard = Some(secret);
                    }
                    return Ok(refreshed);
                }
                Err(e) => last = Some(e),
            }
        }
        Err(match last {
            Some(e) => refresh_to_bitrouter_error(e),
            None => BitrouterError::Upstream {
                status: 401,
                message: "no `agy` OAuth client secret available to refresh the Antigravity token"
                    .into(),
            },
        })
    }

    /// The secret candidates to try: the last-working one first (cheap), else
    /// every `GOCSPX-…` extracted from the `agy` binary.
    fn secret_candidates(&self) -> Result<Vec<String>> {
        if let Ok(guard) = self.working_secret.lock()
            && let Some(s) = guard.as_ref()
        {
            return Ok(vec![s.clone()]);
        }
        agy_client::extract_secrets().map_err(|e| BitrouterError::Upstream {
            status: 401,
            message: format!("cannot refresh the Antigravity token: {e}"),
        })
    }

    fn persist_refreshed(&self, label: &str, token: OAuthToken) -> Result<()> {
        let mut store = CredentialStore::load(&self.store_path).map_err(|e| {
            BitrouterError::internal(format!("reloading credential store before write-back: {e}"))
        })?;
        store
            .set(PROVIDER_ID, label, Credential::from_oauth_token(token))
            .map_err(|e| {
                BitrouterError::internal(format!("persisting refreshed antigravity token: {e}"))
            })?;
        Ok(())
    }

    /// Resolve the Code Assist project id for `label`, calling `loadCodeAssist`
    /// once and caching the result. `api_base` is the cloudcode-pa base from the
    /// routing target.
    async fn resolve_project(&self, label: &str, api_base: &str, bearer: &str) -> Result<String> {
        if let Ok(guard) = self.project_cache.lock()
            && let Some(p) = guard.get(label)
        {
            return Ok(p.clone());
        }
        let url = format!(
            "{}/v1internal:loadCodeAssist",
            api_base.trim_end_matches('/')
        );
        let resp = self
            .http
            .post(&url)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {bearer}"))
            .header(reqwest::header::USER_AGENT, user_agent())
            .header("Client-Metadata", client_metadata())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(LOAD_CODE_ASSIST_BODY)
            .send()
            .await
            .map_err(|e| BitrouterError::Upstream {
                status: 502,
                message: format!("Antigravity loadCodeAssist request failed: {e}"),
            })?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.map_err(|e| BitrouterError::Upstream {
            status: 502,
            message: format!("Antigravity loadCodeAssist returned non-JSON: {e}"),
        })?;
        if !status.is_success() {
            return Err(BitrouterError::Upstream {
                status: status.as_u16(),
                message: format!("Antigravity loadCodeAssist failed: {body}"),
            });
        }
        let project = body
            .get("cloudaicompanionProject")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| BitrouterError::Upstream {
                status: 502,
                message: "Antigravity loadCodeAssist returned no cloudaicompanionProject — \
                          the account may need onboarding in the `agy` CLI first"
                    .into(),
            })?
            .to_string();
        if let Ok(mut guard) = self.project_cache.lock() {
            guard.insert(label.to_string(), project.clone());
        }
        Ok(project)
    }
}

fn refresh_to_bitrouter_error(e: AuthCodeError) -> BitrouterError {
    match e {
        AuthCodeError::OAuthError { error, description } => BitrouterError::Upstream {
            status: 401,
            message: format!(
                "google-ai OAuth refresh failed ({error}{}). Re-run `agy` to re-authenticate, \
                 then `bitrouter providers login google-ai`.",
                description.map(|d| format!(": {d}")).unwrap_or_default()
            ),
        },
        other => BitrouterError::Upstream {
            status: 502,
            message: format!("antigravity OAuth refresh transport error: {other}"),
        },
    }
}

/// The `User-Agent` presented to cloudcode-pa: `antigravity/<ver> <goos>/<goarch>`.
fn user_agent() -> String {
    format!("antigravity/{AGY_VERSION} {}/{}", go_os(), go_arch())
}

/// The `Client-Metadata` header identifying the (spoofed) first-party client.
fn client_metadata() -> String {
    format!(
        r#"{{"ideType":"IDE_UNSPECIFIED","platform":"{}","pluginType":"GEMINI"}}"#,
        go_platform()
    )
}

/// Go's `runtime.GOOS` for this target (the `agy` UA uses Go naming).
fn go_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other, // "linux", "windows"
    }
}

/// Go's `runtime.GOARCH` for this target.
fn go_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

/// cloudcode-pa `Client-Metadata.platform` value.
fn go_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "DARWIN",
        "windows" => "WINDOWS",
        _ => "LINUX",
    }
}

#[async_trait]
impl AuthApplier for AntigravityAuthApplier {
    async fn apply(
        &self,
        mut request: reqwest::Request,
        target: &RoutingTarget,
    ) -> Result<reqwest::Request> {
        let label = self.label_for(target);
        let token = self.resolve_token(label).await?;
        let headers = request.headers_mut();
        let bearer = format!("Bearer {}", token.access_token);
        headers.insert(
            reqwest::header::AUTHORIZATION,
            HeaderValue::from_str(&bearer).map_err(|e| {
                BitrouterError::internal(format!("invalid antigravity bearer: {e}"))
            })?,
        );
        // The Gemini transport default would set `x-goog-api-key`; cloudcode-pa
        // authenticates by Bearer, so drop it.
        headers.remove("x-goog-api-key");
        headers.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_str(&user_agent())
                .map_err(|e| BitrouterError::internal(format!("invalid user-agent: {e}")))?,
        );
        headers.insert(
            HeaderName::from_static("client-metadata"),
            HeaderValue::from_str(&client_metadata())
                .map_err(|e| BitrouterError::internal(format!("invalid client-metadata: {e}")))?,
        );
        Ok(request)
    }

    async fn prepare_body(
        &self,
        body: &mut serde_json::Value,
        target: &RoutingTarget,
    ) -> Result<()> {
        // Wrap the rendered Gemini body in cloudcode-pa's envelope:
        // `{ model, project, request: <gemini body> }`. Needs the Bearer (to
        // call loadCodeAssist for the project) — resolved + cached here.
        let label = self.label_for(target);
        let token = self.resolve_token(label).await?;
        let project = self
            .resolve_project(label, target.effective_api_base(), &token.access_token)
            .await?;
        let inner = std::mem::replace(body, serde_json::Value::Null);
        *body = serde_json::json!({
            "model": target.service_id,
            "project": project,
            "request": inner,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use bitrouter_sdk::language_model::types::ApiProtocol;

    use super::*;

    fn tmp_store_path() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-antigravity-test-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("creds.json")
    }

    fn target() -> RoutingTarget {
        RoutingTarget {
            provider_name: PROVIDER_ID.into(),
            service_id: "gemini-2.5-flash".into(),
            api_base: "https://cloudcode-pa.googleapis.com".into(),
            api_key: String::new(),
            api_protocol: ApiProtocol::Custom(protocol::PROTOCOL.into()),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
        }
    }

    #[tokio::test]
    async fn apply_fails_without_credential() {
        let applier = AntigravityAuthApplier::new(tmp_store_path()).unwrap();
        let req = reqwest::Client::new()
            .post("https://cloudcode-pa.googleapis.com/v1internal:generateContent")
            .build()
            .unwrap();
        let err = applier.apply(req, &target()).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("bitrouter providers login google-ai"),
            "expected login hint, got: {err}"
        );
    }

    #[tokio::test]
    async fn apply_sets_bearer_and_spoof_headers() {
        let path = tmp_store_path();
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set(
                    PROVIDER_ID,
                    DEFAULT_LABEL,
                    Credential::from_oauth_token(OAuthToken {
                        access_token: "ya29.test".into(),
                        expires_at: 0, // non-expiring → no refresh
                        refresh_token: Some("1//r".into()),
                    }),
                )
                .unwrap();
        }
        let applier = AntigravityAuthApplier::new(&path).unwrap();
        let req = reqwest::Client::new()
            .post("https://cloudcode-pa.googleapis.com/v1internal:generateContent")
            .build()
            .unwrap();
        let authed = applier.apply(req, &target()).await.unwrap();
        let h = authed.headers();
        assert_eq!(
            h.get(reqwest::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer ya29.test")
        );
        assert!(
            h.get(reqwest::header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|ua| ua.starts_with("antigravity/"))
        );
        assert!(h.get("client-metadata").is_some());
        assert!(h.get("x-goog-api-key").is_none());
    }

    #[test]
    fn user_agent_and_platform_are_well_formed() {
        let ua = user_agent();
        assert!(ua.starts_with("antigravity/"));
        assert!(ua.contains('/'));
        let cm = client_metadata();
        assert!(cm.contains("\"pluginType\":\"GEMINI\""));
    }
}
