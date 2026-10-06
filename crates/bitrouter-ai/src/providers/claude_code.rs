//! `claude-code` — the Claude Pro/Max **subscription** `AuthApplier`.
//!
//! Registered under the provider id `"claude-code"`. This applier resolves an
//! OAuth credential (or a [`Credential::ClaudeCodeCli`](crate::auth::credentials::Credential::ClaudeCodeCli) marker pointing at the
//! live `~/.claude` session) and shapes the request as a first-party Claude
//! Code call:
//!
//! | Stored credential | Outbound headers |
//! |---|---|
//! | `Credential::Oauth` (Claude Pro/Max)  | `Authorization: Bearer sk-ant-oat…`, `anthropic-beta: claude-code-20250219,oauth-2025-04-20`, `anthropic-version: 2023-06-01`, `user-agent: claude-cli/…`, `x-app: cli`. **No `x-api-key`** — the upstream rejects OAuth requests that also carry `x-api-key`. |
//! | `Credential::ClaudeCodeCli`           | Same as above, but the token is read live from Claude Code's own store (`~/.claude`) and any refresh is written back there. |
//! | _no credential in store_              | `401` — there is no API-key fallback on the subscription path. |
//!
//! The OAuth branch refreshes the access token via
//! [`crate::auth::oauth::refresh`] if it's within
//! [`crate::auth::oauth::REFRESH_WINDOW`] of expiry, writes the new token
//! back through an account transaction. Each call reads the selected slot;
//! concurrent callers share committed rotations without a stale token cache.
//!
//! ## Explicit route boundary and upstream adaptation
//!
//! Resolving an explicit `claude-code:<model>` target is the operator's
//! authorization to spend the subscription. Downstream clients speak normal
//! Anthropic Messages; [`ClaudeCodeAuthApplier::apply`] adds the OAuth and
//! Claude Code agent-profile headers required by the upstream. Bare canonical
//! Claude models never auto-cascade onto subscription providers, while genuine
//! Claude Code traffic can still be auto-routed by the app-layer ingress
//! detector. [`ClaudeCodeAuthApplier::prepare_body`] prepends the current
//! Claude Agent SDK identity when a standard Messages client did not already
//! supply a recognized Claude Code identity, preserving all client system
//! instructions and remaining idempotent for genuine Claude Code.
//!
//! The caller injects an `OAuthSession` and explicitly permits any captured
//! fallback token. Interactive login, client registration, environment capture
//! and live CLI/file storage are supplied above this integration.

use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};

use crate::auth::AuthApplier;
use crate::error::{ModelError, Result};
use crate::target::ModelTarget;

use crate::auth::store::DEFAULT_ACCOUNT;
use crate::auth::store::{CredentialKey, OAuthSession};
use crate::providers::anthropic::headers;

use crate::auth::credentials::OAuthToken;

/// Provider id this applier is registered under.
pub const PROVIDER_ID: &str = "claude-code";

/// Selected-account Claude subscription authentication and request adaptation.
/// The session owns refresh transactions; the application supplies any live CLI source.
pub struct ClaudeCodeAuthApplier {
    session: OAuthSession,
    fallback: Option<OAuthToken>,
}

impl ClaudeCodeAuthApplier {
    /// Use an explicitly assembled session without environment/path discovery.
    pub fn new(session: OAuthSession) -> Self {
        Self {
            session,
            fallback: None,
        }
    }
    /// Permit a captured non-refreshable fallback only while the selected slot is absent.
    pub fn with_fallback_token(mut self, token: Option<OAuthToken>) -> Self {
        self.fallback = token.map(|mut token| {
            token.refresh_token = None;
            token
        });
        self
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
    async fn resolve_credential(&self, target: &ModelTarget) -> Result<OAuthToken> {
        if let Some(access_token) = target.explicit_credential() {
            return Ok(OAuthToken {
                access_token: access_token.into(),
                expires_at: 0,
                refresh_token: None,
            });
        }
        let fallback = target
            .fallback_credential()
            .map(|access_token| OAuthToken {
                access_token: access_token.into(),
                expires_at: 0,
                refresh_token: None,
            })
            .or_else(|| self.fallback.clone());
        self.session.resolve_with_fallback(&Self::key_for(target), fallback).await.map_err(|error| match error {
            ModelError::Provider { status: 401, .. } => ModelError::Provider { status: 401, message: "selected Claude Code account has no usable subscription OAuth credential; explicitly authorize it or renew the permitted CLI session".into() },
            error => error,
        })
    }
}

#[async_trait]
impl AuthApplier for ClaudeCodeAuthApplier {
    async fn apply(
        &self,
        mut request: reqwest::Request,
        target: &ModelTarget,
    ) -> Result<reqwest::Request> {
        // Reaching this applier means routing already resolved an explicit
        // `claude-code:<model>` target. That explicit target is the
        // subscription-use boundary. Downstream clients speak normal Anthropic
        // Messages; this applier owns the OAuth/Claude-Code upstream
        // transformation below.
        let token = self.resolve_credential(target).await?;
        // `anthropic-version` is mandatory regardless of credential type.
        request.headers_mut().insert(
            "anthropic-version",
            HeaderValue::from_static(headers::ANTHROPIC_VERSION),
        );
        let bearer = format!("Bearer {}", token.access_token);
        let auth = HeaderValue::from_str(&bearer).map_err(|e| {
            ModelError::configuration(format!(
                "invalid claude-code OAuth bearer for Authorization: {e}"
            ))
        })?;
        let headers_mut = request.headers_mut();
        headers_mut.insert(reqwest::header::AUTHORIZATION, auth);
        // OAuth requests must NOT carry x-api-key — the upstream
        // returns 401 when both auth schemes are present.
        headers_mut.remove("x-api-key");
        // Merge — never overwrite — the OAuth-required betas with any
        // the client already sent. Claude Code appends feature betas
        // (e.g. `context-management-2025-06-27`, interleaved thinking,
        // prompt caching) alongside the matching request-body fields;
        // clobbering the header would leave those fields with no
        // enabling beta and the upstream 400s ("Extra inputs are not
        // permitted").
        let client_betas: Vec<String> = headers_mut
            .get_all("anthropic-beta")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_string)
            .collect();
        let beta_value = merged_beta_value(client_betas.iter().map(String::as_str));
        let beta_header = HeaderValue::from_str(&beta_value).map_err(|e| {
            ModelError::configuration(format!("invalid anthropic-beta header: {e}"))
        })?;
        headers_mut.insert(HeaderName::from_static("anthropic-beta"), beta_header);
        // The subscription endpoint expects first-party-CLI-shaped
        // requests; mirror Claude Code's user-agent + x-app so the
        // OAuth credential is admitted. (Reference: OpenClaw
        // `src/llm/providers/anthropic.ts`.)
        headers_mut.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_static(headers::CLAUDE_CODE_USER_AGENT),
        );
        headers_mut.insert(
            HeaderName::from_static("x-app"),
            HeaderValue::from_static(headers::CLAUDE_CODE_X_APP),
        );
        Ok(request)
    }

    async fn prepare_body(
        &self,
        body: &mut serde_json::Value,
        _target: &ModelTarget,
    ) -> Result<()> {
        unwrap_litellm_extra_body(body)?;
        // A/B against the first-party CLI shows the subscription endpoint
        // returns a generic 429 for an OAuth-shaped request that lacks a
        // recognized agent identity. Headers alone are insufficient. Add the
        // current SDK identity only on the explicit claude-code route, retain
        // every client system block, and do nothing for genuine/legacy Claude
        // Code bodies that already carry an identity.
        if system_has_agent_identity(body) {
            return Ok(());
        }
        let object = body.as_object_mut().ok_or_else(|| {
            ModelError::configuration("claude-code request body must be a JSON object")
        })?;
        let identity = serde_json::json!({
            "type": "text",
            "text": headers::CLAUDE_AGENT_SYSTEM_PROMPT,
        });
        let system = match object.remove("system") {
            None | Some(serde_json::Value::Null) => serde_json::Value::Array(vec![identity]),
            Some(serde_json::Value::String(text)) => serde_json::Value::Array(vec![
                identity,
                serde_json::json!({ "type": "text", "text": text }),
            ]),
            Some(serde_json::Value::Array(mut blocks)) => {
                blocks.insert(0, identity);
                serde_json::Value::Array(blocks)
            }
            Some(other) => {
                object.insert("system".to_string(), other);
                return Err(ModelError::Provider {
                    status: 400,
                    message: "claude-code request has an invalid Anthropic system field".into(),
                });
            }
        };
        object.insert("system".to_string(), system);
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

/// Normalize LiteLLM's provider-extension container before calling Anthropic.
///
/// Some normal agent harnesses serialize `extra_body` instead of merging it
/// into the provider request. Anthropic rejects that wrapper. Merge extension
/// fields into the top level (without overriding explicit top-level values)
/// and drop `session_id`: BitRouter already receives the session through
/// headers, while Anthropic does not accept it as a Messages body field.
fn unwrap_litellm_extra_body(body: &mut serde_json::Value) -> Result<()> {
    let object = body.as_object_mut().ok_or_else(|| {
        ModelError::configuration("claude-code request body must be a JSON object")
    })?;
    let Some(extra_body) = object.remove("extra_body") else {
        return Ok(());
    };
    let serde_json::Value::Object(extra_body) = extra_body else {
        return Err(ModelError::Provider {
            status: 400,
            message: "claude-code request has an invalid LiteLLM extra_body field".into(),
        });
    };
    for (key, value) in extra_body {
        if key != "session_id" {
            object.entry(key).or_insert(value);
        }
    }
    Ok(())
}

/// Whether the rendered Messages body already carries a current or legacy
/// Claude Code identity. Genuine current CLI traffic places a billing marker
/// first and the identity in the next block, so scan all system text blocks
/// rather than assuming index zero.
fn system_has_agent_identity(body: &serde_json::Value) -> bool {
    fn is_identity(text: &str) -> bool {
        let text = text.trim_start();
        text.starts_with(headers::CLAUDE_AGENT_SYSTEM_PROMPT)
            || text.starts_with(headers::LEGACY_CLAUDE_CODE_SYSTEM_PROMPT)
    }

    match body.as_object().and_then(|object| object.get("system")) {
        Some(serde_json::Value::String(text)) => is_identity(text),
        Some(serde_json::Value::Array(blocks)) => blocks.iter().any(|block| {
            block
                .get("text")
                .and_then(serde_json::Value::as_str)
                .is_some_and(is_identity)
        }),
        _ => false,
    }
}

/// Merge the OAuth-required `anthropic-beta` values (which the Claude Pro/Max
/// subscription endpoint demands) with any the client already sent, deduping
/// while keeping the required values first. Real Claude Code traffic carries
/// feature betas next to matching request-body fields, so the union — not an
/// overwrite — is what keeps those requests valid upstream.
fn merged_beta_value<'a>(client_betas: impl Iterator<Item = &'a str>) -> String {
    let mut out: Vec<String> = headers::OAUTH_BETA_VALUES
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    for raw in client_betas {
        for beta in raw.split(',') {
            let beta = beta.trim();
            if !beta.is_empty() && !out.iter().any(|x| x == beta) {
                out.push(beta.to_string());
            }
        }
    }
    out.join(",")
}
