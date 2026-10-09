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
//! the application Grok CLI import — mirroring the Codex import. The access token is a
//! JWT the xAI API accepts as a Bearer; the applier refreshes it via the public
//! OIDC client against `https://auth.x.ai/oauth2/token` when it is within
//! [`crate::auth::oauth::REFRESH_WINDOW`] of expiry, writes the new token
//! back to the store, and caches it in memory.
//!
//! ## Auth client
//!
//! SuperGrok's OIDC client is public (PKCE, no client secret), so refresh needs
//! only the client id — the standard [`crate::auth::oauth::refresh`] path.
//! The client id + token endpoint are the ones the official Grok CLI ships
//! with (confirmed in `~/.grok/auth.json`, keyed `<issuer>::<client_id>`). A
//! native browser login could be added later via application OAuth registration;
//! today the only path is importing the Grok CLI session.

use async_trait::async_trait;
use base64::Engine;
use reqwest::header::HeaderValue;

use crate::auth::{AppliedAuth, AuthApplier, CredentialAuthority};
use crate::error::{ModelError, Result};
use crate::target::ModelTarget;

use crate::auth::credentials::OAuthToken;
use crate::auth::store::DEFAULT_ACCOUNT;
use crate::auth::store::{CredentialKey, OAuthSession};

/// Provider id this applier is registered under.
pub const PROVIDER_ID: &str = "supergrok";

/// SuperGrok's public OIDC client id — the one the official Grok CLI ships with
/// (confirmed as the `<issuer>::<client_id>` key in `~/.grok/auth.json`).
pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";

/// OIDC token endpoint for the refresh-token grant (from `auth.x.ai`'s
/// `/.well-known/openid-configuration`).
pub const TOKEN_ENDPOINT: &str = "https://auth.x.ai/oauth2/token";

/// `AuthApplier` for `provider_name == "supergrok"`.
pub struct SuperGrokAuthApplier {
    session: OAuthSession,
}

impl SuperGrokAuthApplier {
    /// Bind the selected-account session without discovering files or opening login.
    pub fn new(session: OAuthSession) -> Self {
        Self { session }
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
        if let Some(token) = target.explicit_credential() {
            return Ok(OAuthToken {
                access_token: token.into(),
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
            ModelError::Provider { status: 401, .. } => ModelError::Provider { status: 401, message: "selected supergrok account requires subscription OAuth; explicitly authorize this account".into() },
            error => error,
        })
    }
    fn continuation_authority(token: &OAuthToken) -> Option<CredentialAuthority> {
        let payload = token.access_token.split('.').nth(1)?;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .ok()?;
        let payload: serde_json::Value = serde_json::from_slice(&payload).ok()?;
        let subject = payload.get("sub")?.as_str()?.trim();
        (!subject.is_empty())
            .then(|| CredentialAuthority::derive("supergrok/oidc-subject", subject))
    }
}

#[async_trait]
impl AuthApplier for SuperGrokAuthApplier {
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
        let token = self.resolve_token(target).await?;
        let bearer = format!("Bearer {}", token.access_token);
        let auth = HeaderValue::from_str(&bearer).map_err(|e| {
            ModelError::configuration(format!("invalid SuperGrok bearer for Authorization: {e}"))
        })?;
        request
            .headers_mut()
            .insert(reqwest::header::AUTHORIZATION, auth);
        Ok(match Self::continuation_authority(&token) {
            Some(authority) => AppliedAuth::proven(request, authority),
            None => AppliedAuth::unproven(request),
        })
    }

    async fn continuation_authority(
        &self,
        target: &ModelTarget,
    ) -> Result<Option<CredentialAuthority>> {
        let token = self.resolve_token(target).await?;
        Ok(Self::continuation_authority(&token))
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
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.strip_prefix("Bearer ")),
            )
            .await?;
        Ok(true)
    }
}
