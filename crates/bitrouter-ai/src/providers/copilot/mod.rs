//! GitHub Copilot — `AuthApplier` plus the GitHub→Copilot token exchange.
//!
//! Two-step authentication:
//!
//! 1. The user runs `bro providers login github-copilot` once, which
//!    drives the OAuth Device Authorization Grant against `github.com` and
//!    stores a long-lived GitHub user-to-server access token (e.g. `ghu_…`) in
//!    the injected selected-account storage.
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

use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};

use crate::auth::{AppliedAuth, AuthApplier, CredentialAuthority};
use crate::error::{ModelError, Result};
use crate::target::ModelTarget;

use crate::auth::credentials::OAuthToken;
use crate::auth::store::DEFAULT_ACCOUNT;
use crate::auth::store::{CredentialKey, CredentialStore as AccountStore};
use exchange::{CopilotToken, exchange_for_copilot_token_at};

/// Provider id used throughout the codebase. Matches the TOML filename stem.
pub const PROVIDER_ID: &str = "github-copilot";

#[derive(Clone)]
struct CopilotAuth {
    token: CopilotToken,
    continuation_authority: CredentialAuthority,
}

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
    exchange_client: reqwest::Client,
    exchange_url: String,
    store: Arc<dyn AccountStore>,
    cache: tokio::sync::Mutex<Option<CopilotAuth>>,
}

impl CopilotAuthApplier {
    /// Bind explicit exchange configuration and selected-account storage.
    /// Construction reads no files/environment and never opens login.
    pub fn new(
        client: reqwest::Client,
        url: impl Into<String>,
        store: Arc<dyn AccountStore>,
    ) -> Self {
        Self {
            exchange_client: client,
            exchange_url: url.into(),
            store,
            cache: tokio::sync::Mutex::new(None),
        }
    }
    async fn obtain_copilot_auth(
        &self,
        target: &ModelTarget,
        rejected: Option<&str>,
    ) -> Result<CopilotAuth> {
        // Keep the selected store lease through exchange. This is an ephemeral
        // derived token; the long-lived GitHub credential is never overwritten.
        let transaction = if target.explicit_credential().is_some() {
            None
        } else {
            let key = CredentialKey {
                provider: PROVIDER_ID.into(),
                account: target
                    .account_label
                    .as_deref()
                    .unwrap_or(DEFAULT_ACCOUNT)
                    .to_owned(),
            };
            Some(
                self.store
                    .begin(&key)
                    .await
                    .map_err(|failure| ModelError::CredentialStorage { failure })?,
            )
        };
        let github = if let Some(access) = target.explicit_credential() {
            OAuthToken {
                access_token: access.into(),
                expires_at: 0,
                refresh_token: None,
            }
        } else {
            match transaction.as_ref().and_then(|transaction| transaction.credential()) {
                Some(credential) => credential.as_oauth().filter(|token| token.is_valid() && !token.access_token.is_empty()).cloned(),
                None => target.fallback_credential().map(|access_token| OAuthToken { access_token: access_token.into(), expires_at: 0, refresh_token: None }),
            }.ok_or_else(|| ModelError::Provider { status: 401, message: "selected GitHub Copilot account requires usable OAuth; explicitly authorize this account".into() })?
        };
        let authority =
            CredentialAuthority::derive("github-copilot/stored-github-token", &github.access_token);
        let mut cache = self.cache.lock().await;
        if let Some(auth) = cache.as_ref()
            && auth.continuation_authority == authority
            && auth.token.is_fresh()
            && rejected.is_none_or(|rejected| rejected != auth.token.token)
        {
            return Ok(auth.clone());
        }
        let token = exchange_for_copilot_token_at(
            &self.exchange_client,
            &self.exchange_url,
            &github.access_token,
        )
        .await
        .map_err(|_| ModelError::Provider {
            status: 502,
            message: "selected GitHub credential could not exchange a Copilot token".into(),
        })?;
        let auth = CopilotAuth {
            token,
            continuation_authority: authority,
        };
        *cache = Some(auth.clone());
        Ok(auth)
    }
    /// Resolve an ephemeral Copilot token for this exact selected source/account.
    pub async fn obtain_copilot_token(&self, target: &ModelTarget) -> Result<CopilotToken> {
        Ok(self.obtain_copilot_auth(target, None).await?.token)
    }
}

#[async_trait]
impl AuthApplier for CopilotAuthApplier {
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
        let auth = self.obtain_copilot_auth(target, None).await?;
        let bearer = format!("Bearer {}", auth.token.token);
        let value = HeaderValue::from_str(&bearer).map_err(|e| {
            ModelError::configuration(format!("invalid Copilot bearer for Authorization: {e}"))
        })?;
        request
            .headers_mut()
            .insert(reqwest::header::AUTHORIZATION, value);
        for (name, value) in headers::copilot_request_headers() {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                ModelError::configuration(format!("invalid Copilot header name '{name}': {e}"))
            })?;
            let value = HeaderValue::from_str(&value).map_err(|e| {
                ModelError::configuration(format!("invalid Copilot header value: {e}"))
            })?;
            request.headers_mut().insert(name, value);
        }
        Ok(AppliedAuth::proven(request, auth.continuation_authority))
    }

    async fn continuation_authority(
        &self,
        target: &ModelTarget,
    ) -> Result<Option<CredentialAuthority>> {
        Ok(Some(
            self.obtain_copilot_auth(target, None)
                .await?
                .continuation_authority,
        ))
    }
    async fn refresh_after_unauthorized(
        &self,
        target: &ModelTarget,
        rejected: Option<&HeaderValue>,
    ) -> Result<bool> {
        let rejected = rejected
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        self.obtain_copilot_auth(target, rejected).await?;
        Ok(true)
    }
}
