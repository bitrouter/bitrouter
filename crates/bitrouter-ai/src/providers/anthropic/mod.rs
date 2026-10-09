//! Anthropic — the Platform-API `AuthApplier` (`x-api-key`).
//!
//! Registered under the provider id `"anthropic"`. This applier covers the
//! Anthropic **Platform API** (pay-as-you-go) only: it resolves a static API
//! key and sets `x-api-key` + `anthropic-version`. It does **no** OAuth, no
//! `ClaudeCodeCli` marker resolution, and no body shaping — the Claude Pro/Max
//! subscription path lives in the separate [`crate::providers::claude_code`] applier
//! (provider id `"claude-code"`).
//!
//! | Source of the key | Outbound headers |
//! |---|---|
//! | `Credential::ApiKey` stored under `"anthropic"` | `x-api-key: <value>`, `anthropic-version: 2023-06-01`. |
//! | _no stored key_ | Fall back to the routing target's inline `api_key` (the `${ANTHROPIC_API_KEY}` env path). |
//! | _neither_ | `401` pointing at `ANTHROPIC_API_KEY` or an Anthropic API-key credential. |
//!
//! [`AnthropicApiKeyApplier::prepare_body`] is a no-op: the Platform API takes
//! the caller's body verbatim.
//!
//! The shared header constants (`anthropic-version`, and the Claude Code
//! constants reused by [`crate::providers::claude_code`]) live in [`headers`].

pub mod headers;

use async_trait::async_trait;
use reqwest::header::HeaderValue;

use crate::auth::{AppliedAuth, AuthApplier, ContinuationAuthority, CredentialAuthority};
use crate::error::{ModelError, Result};
use crate::target::ModelTarget;
use crate::types::AuthScheme;

use crate::auth::credentials::Credential;
use crate::auth::store::DEFAULT_ACCOUNT;

/// Provider id this applier is registered under.
pub const PROVIDER_ID: &str = "anthropic";

/// `AuthApplier` for `provider_name == "anthropic"` — the Anthropic Platform
/// API (`x-api-key`).
///
/// The caller injects the selected credential backend.
/// When no key is stored it falls through to the routing target's inline
/// `api_key` (the `${ANTHROPIC_API_KEY}` env path), preserving existing setups.
pub struct AnthropicApiKeyApplier {
    store: std::sync::Arc<dyn crate::auth::store::CredentialStore>,
}

impl AnthropicApiKeyApplier {
    /// Use an explicit credential backend; no environment or path lookup.
    pub fn new(store: std::sync::Arc<dyn crate::auth::store::CredentialStore>) -> Self {
        Self { store }
    }

    async fn resolve_key(&self, target: &ModelTarget) -> Result<String> {
        if let Some(key) = target.explicit_credential() {
            return Ok(key.to_owned());
        }
        let key = crate::auth::store::CredentialKey {
            provider: PROVIDER_ID.into(),
            account: target
                .account_label
                .as_deref()
                .unwrap_or(DEFAULT_ACCOUNT)
                .to_owned(),
        };
        let transaction = self
            .store
            .begin(&key)
            .await
            .map_err(|failure| ModelError::CredentialStorage { failure })?;
        match transaction.credential() {
            Some(Credential::ApiKey { value }) if !value.is_empty() => Ok(value.clone()),
            None if target.fallback_credential().is_some() => Ok(target.api_key.clone()),
            _ => Err(ModelError::Provider {
                status: 401,
                message: "selected anthropic account requires an API key; explicitly provide or store an API-key credential".into(),
            }),
        }
    }
}

#[async_trait]
impl AuthApplier for AnthropicApiKeyApplier {
    async fn apply(
        &self,
        request: reqwest::Request,
        target: &ModelTarget,
    ) -> Result<reqwest::Request> {
        Ok(self
            .apply_with_authority(request, target)
            .await?
            .into_request())
    }

    async fn apply_with_authority(
        &self,
        mut request: reqwest::Request,
        target: &ModelTarget,
    ) -> Result<AppliedAuth> {
        request.headers_mut().insert(
            "anthropic-version",
            HeaderValue::from_static(headers::ANTHROPIC_VERSION),
        );
        let key = self.resolve_key(target).await?;
        apply_api_key_header(&mut request, &key)?;
        // An explicit workspace selector has not been resolved to a principal.
        // https://platform.claude.com/docs/en/api/overview#authentication
        Ok(
            if request.headers().contains_key("anthropic-workspace-id") {
                AppliedAuth::unproven(request)
            } else {
                AppliedAuth::proven(
                    request,
                    CredentialAuthority::derive("anthropic/api-key", &key),
                )
            },
        )
    }

    async fn continuation_authority(
        &self,
        target: &ModelTarget,
    ) -> Result<Option<CredentialAuthority>> {
        let key = self.resolve_key(target).await?;
        Ok(
            (!key.trim().is_empty())
                .then(|| CredentialAuthority::derive("anthropic/api-key", &key)),
        )
    }

    async fn continuation_authority_proof(
        &self,
        target: &ModelTarget,
    ) -> Result<Option<ContinuationAuthority>> {
        Ok(self
            .continuation_authority(target)
            .await?
            .map(|authority| ContinuationAuthority::new(authority, AuthScheme::XApiKey)))
    }

    async fn prepare_body(
        &self,
        _body: &mut serde_json::Value,
        _target: &ModelTarget,
    ) -> Result<()> {
        // The Platform API takes the caller's body verbatim — never shaped.
        Ok(())
    }
}

fn apply_api_key_header(request: &mut reqwest::Request, key: &str) -> Result<()> {
    let value = HeaderValue::from_str(key).map_err(|e| {
        ModelError::configuration(format!("invalid api key for x-api-key header: {e}"))
    })?;
    request.headers_mut().insert("x-api-key", value);
    // Clear any stale Bearer the protocol layer might have added — the
    // Platform API authenticates via x-api-key only.
    request.headers_mut().remove(reqwest::header::AUTHORIZATION);
    Ok(())
}
