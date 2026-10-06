//! Antigravity (Google) — the `agy` **subscription** integration that powers the
//! `google-ai` provider ([`PROVIDER_ID`]; there is no separate `antigravity`
//! provider — see `registry/providers/google-ai.yaml`).
//!
//! Uses a caller-injected OAuth session and HTTP client for Google's Code Assist
//! backend (`cloudcode-pa.googleapis.com/v1internal:*`). The caller supplies
//! account/storage policy and any confidential OAuth client secret source.
//! [`protocol`] delegates model semantics to Gemini and owns its custom endpoint
//! and response envelope. [`refresh`] supplies the explicit confidential grant.

pub mod protocol;
pub mod refresh;

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};

use crate::auth::AuthApplier;
use crate::error::{ModelError, Result};
use crate::target::ModelTarget;

use crate::auth::CredentialAuthority;
use crate::auth::credentials::OAuthToken;
use crate::auth::store::DEFAULT_ACCOUNT;
use crate::auth::store::{CredentialKey, OAuthSession};

/// Provider id this applier is registered under. The `google-ai` subscription
/// provider is powered by this Antigravity (`agy` / cloudcode-pa) integration —
/// there is no separate `antigravity` provider.
pub const PROVIDER_ID: &str = "google-ai";

/// `User-Agent` version we present as. cloudcode-pa is lenient about the exact
/// version, but the `antigravity/*` shape is what admits the request to the
/// Antigravity model set. Retains the existing client profile version.
const AGY_VERSION: &str = "1.1.0";

/// `loadCodeAssist` request body — the minimal shape that returns the project.
const LOAD_CODE_ASSIST_BODY: &str = r#"{"metadata":{"pluginType":"GEMINI"}}"#;

/// `AuthApplier` for the `google-ai` provider.
///
/// Per request: resolve the Google OAuth Bearer (refreshing via the confidential
/// `agy` client when stale), resolve the Code Assist project id (cached
/// `loadCodeAssist` bootstrap), wrap the body in the `{model, project, request}`
/// envelope, and set the first-party spoof headers. The `v1internal:{verb}` URL
/// and the `{"response": …}` unwrap are handled by [`protocol`].
pub struct AntigravityAuthApplier {
    session: OAuthSession,
    http: reqwest::Client,
    project_cache: Arc<Mutex<std::collections::HashMap<(CredentialAuthority, String), String>>>,
}

impl AntigravityAuthApplier {
    /// Bind an explicit selected-account session and bootstrap client.
    /// This constructor discovers no credentials, client secrets or storage paths.
    pub fn new(session: OAuthSession, http: reqwest::Client) -> Self {
        Self {
            session,
            http,
            project_cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }
    fn key_for(target: &ModelTarget) -> CredentialKey {
        CredentialKey {
            provider: PROVIDER_ID.into(),
            account: target
                .account_label
                .as_deref()
                .unwrap_or(DEFAULT_ACCOUNT)
                .to_owned(),
        }
    }
    async fn resolve_token(&self, target: &ModelTarget) -> Result<OAuthToken> {
        if let Some(access_token) = target.explicit_credential() {
            return Ok(OAuthToken {
                access_token: access_token.into(),
                expires_at: 0,
                refresh_token: None,
            });
        }
        let fallback = target.fallback_credential().map(|access_token| OAuthToken {
            access_token: access_token.into(),
            expires_at: 0,
            refresh_token: None,
        });
        self.session.resolve_with_fallback(&Self::key_for(target), fallback).await.map_err(|error| match error {
            ModelError::Provider { status: 401, .. } => ModelError::Provider { status: 401, message: "selected Google AI account requires OAuth; explicitly authorize this account".into() },
            error => error,
        })
    }
    /// Resolve the Code Assist project for this exact bearer and origin.
    /// once and caching the result. `api_base` is the cloudcode-pa base from the
    /// routing target.
    async fn resolve_project(&self, api_base: &str, bearer: &str) -> Result<String> {
        let cache_key = (
            CredentialAuthority::derive("google-ai/project-bearer", bearer),
            api_base.trim_end_matches('/').to_owned(),
        );
        if let Ok(guard) = self.project_cache.lock()
            && let Some(p) = guard.get(&cache_key)
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
            .timeout(Duration::from_secs(30))
            .body(LOAD_CODE_ASSIST_BODY)
            .send()
            .await
            .map_err(|e| ModelError::Provider {
                status: 502,
                message: format!(
                    "Google AI project bootstrap request failed: {}",
                    e.without_url()
                ),
            })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(ModelError::Provider {
                status: status.as_u16(),
                message: "Google AI project bootstrap failed".into(),
            });
        }
        let body: serde_json::Value = resp.json().await.map_err(|_| ModelError::Provider {
            status: 502,
            message: "Google AI project bootstrap returned non-JSON".into(),
        })?;
        let project = body
            .get("cloudaicompanionProject")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ModelError::Provider {
                status: 502,
                message: "Antigravity loadCodeAssist returned no cloudaicompanionProject — \
                          the account may need onboarding in the `agy` CLI first"
                    .into(),
            })?
            .to_string();
        if let Ok(mut guard) = self.project_cache.lock() {
            guard.insert(cache_key, project.clone());
        }
        Ok(project)
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
        target: &ModelTarget,
    ) -> Result<reqwest::Request> {
        let token = self.resolve_token(target).await?;
        if let Some(bytes) = request.body().and_then(reqwest::Body::as_bytes) {
            let mut body: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| {
                ModelError::invalid_request("Google AI auth requires a JSON request")
            })?;
            if body.get("request").is_some() {
                let project = self
                    .resolve_project(&target.api_base, &token.access_token)
                    .await?;
                body["project"] = serde_json::Value::String(project);
                *request.body_mut() = Some(reqwest::Body::from(
                    serde_json::to_vec(&body)
                        .map_err(|_| ModelError::configuration("serializing Google AI request"))?,
                ));
                request
                    .headers_mut()
                    .remove(reqwest::header::CONTENT_LENGTH);
            }
        }
        let headers = request.headers_mut();
        let bearer = format!("Bearer {}", token.access_token);
        headers.insert(
            reqwest::header::AUTHORIZATION,
            HeaderValue::from_str(&bearer).map_err(|e| {
                ModelError::configuration(format!("invalid antigravity bearer: {e}"))
            })?,
        );
        // The Gemini transport default would set `x-goog-api-key`; cloudcode-pa
        // authenticates by Bearer, so drop it.
        headers.remove("x-goog-api-key");
        headers.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_str(&user_agent())
                .map_err(|e| ModelError::configuration(format!("invalid user-agent: {e}")))?,
        );
        headers.insert(
            HeaderName::from_static("client-metadata"),
            HeaderValue::from_str(&client_metadata())
                .map_err(|e| ModelError::configuration(format!("invalid client-metadata: {e}")))?,
        );
        Ok(request)
    }

    async fn prepare_body(&self, body: &mut serde_json::Value, target: &ModelTarget) -> Result<()> {
        // Structural wrapping is independent of account identity. Apply binds
        // the project and credential together on the exact built JSON request.
        let inner = std::mem::replace(body, serde_json::Value::Null);
        *body = serde_json::json!({
            "model": target.service_id,
            "request": inner,
        });
        Ok(())
    }
    async fn refresh_after_unauthorized(
        &self,
        target: &ModelTarget,
        rejected: Option<&HeaderValue>,
    ) -> Result<bool> {
        if target.explicit_credential().is_some() {
            return Ok(false);
        }
        self.session
            .recover(
                &Self::key_for(target),
                rejected
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer ")),
            )
            .await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
