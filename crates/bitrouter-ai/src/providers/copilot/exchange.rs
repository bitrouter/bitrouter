//! Exchange a GitHub OAuth user-to-server access token (`ghu_…`) for a
//! short-lived Copilot internal token.
//!
//! Endpoint: `GET https://api.github.com/copilot_internal/v2/token`
//! Request header: `Authorization: token <github_oauth_token>` (GitHub's
//! legacy `token` scheme, not `Bearer` — same as the regular GitHub REST API
//! accepts for OAuth user tokens; <https://docs.github.com/en/rest/overview/authenticating-to-the-rest-api?apiVersion=2022-11-28#using-tokens-in-the-rest-api>).
//! Response body: `{ "token": "tid=…", "expires_at": <unix-secs>, "refresh_in": <secs>, "endpoints": {…}, "chat_enabled": bool, … }`.
//!
//! Authoritative references for this endpoint shape:
//! - VS Code Copilot Chat (MIT) — `getCopilotToken` in `src/extension/conversation/copilotToken.ts`.
//!   <https://github.com/microsoft/vscode-copilot-chat>
//! - opencode's TypeScript port:
//!   <https://github.com/sst/opencode/blob/dev/packages/opencode/src/auth/copilot.ts>
//!
//! The endpoint is not in GitHub's public REST docs because it's an
//! integration boundary for first-party Copilot clients, but it's the same
//! URL all open-source Copilot clients hit and has been stable for years.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::providers::copilot::headers::EDITOR_VERSION_HEADER_VALUE;

/// The Copilot token-exchange endpoint.
pub const TOKEN_EXCHANGE_URL: &str = "https://api.github.com/copilot_internal/v2/token";

/// Refresh `refresh_window` seconds before `expires_at` — gives in-flight
/// requests headroom rather than racing the expiry.
const REFRESH_WINDOW_SECS: u64 = 60;

/// A short-lived Copilot Bearer plus its expiry.
#[derive(Clone)]
pub struct CopilotToken {
    /// The Bearer value to send on `Authorization` for `api.githubcopilot.com`.
    /// Format is upstream-controlled (`tid=…;…`); treat as opaque.
    pub token: String,
    /// Unix seconds at which the upstream considers the token expired.
    pub expires_at: u64,
}

impl std::fmt::Debug for CopilotToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CopilotToken")
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl CopilotToken {
    /// Whether the token is still good — leaves a 60s buffer so we don't
    /// race the upstream clock.
    pub fn is_fresh(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now.saturating_add(REFRESH_WINDOW_SECS) < self.expires_at
    }
}

/// Errors raised by the token exchange.
#[derive(Debug, thiserror::Error)]
pub enum ExchangeError {
    /// Transport failure (DNS, TCP, TLS, HTTP status).
    #[error("network error at {TOKEN_EXCHANGE_URL}: {0}")]
    Network(#[from] reqwest::Error),
    /// `api.github.com` returned a non-success status. 401 means the GitHub
    /// OAuth token is invalid / revoked / expired; 403 means the user's
    /// account doesn't have Copilot access.
    #[error("Copilot token exchange returned HTTP {status}")]
    UpstreamStatus {
        /// HTTP status code returned by the exchange.
        status: u16,
    },
    /// Response wasn't a JSON object with the expected shape.
    #[error("malformed Copilot token-exchange response")]
    Parse,
    /// A returned bearer is empty or expired.
    #[error("Copilot token exchange returned an unusable bearer")]
    InvalidToken,
}

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
    /// Unix seconds. Always present on success.
    #[serde(default)]
    expires_at: u64,
}

/// Perform the GitHub → Copilot token exchange against the production
/// endpoint at [`TOKEN_EXCHANGE_URL`].
///
/// `github_token` is the `ghu_…` access token issued by the OAuth device-code
/// flow against `github.com/login/device/code`. The returned [`CopilotToken`]
/// is what `api.githubcopilot.com` accepts as a Bearer.
pub async fn exchange_for_copilot_token(
    client: &reqwest::Client,
    github_token: &str,
) -> Result<CopilotToken, ExchangeError> {
    exchange_for_copilot_token_at(client, TOKEN_EXCHANGE_URL, github_token).await
}

/// Variant of [`exchange_for_copilot_token`] that lets the caller override
/// the URL — used by integration tests with `wiremock` to assert the
/// request shape without hitting `api.github.com`.
pub async fn exchange_for_copilot_token_at(
    client: &reqwest::Client,
    url: &str,
    github_token: &str,
) -> Result<CopilotToken, ExchangeError> {
    let response = client
        .get(url)
        .timeout(std::time::Duration::from_secs(30))
        // GitHub legacy `token <oauth_token>` scheme — the REST API accepts
        // both this and `Bearer`, but the exchange endpoint is documented in
        // the open-source clients with `token …`.
        .header(
            reqwest::header::AUTHORIZATION,
            format!("token {github_token}"),
        )
        .header(reqwest::header::ACCEPT, "application/json")
        // Without an Editor-Version header GitHub returns "editor not
        // supported" on the chat endpoints; the exchange itself accepts a
        // missing header but it's good hygiene to send the same identity here.
        .header("Editor-Version", EDITOR_VERSION_HEADER_VALUE)
        .send()
        .await
        .map_err(|error| ExchangeError::Network(error.without_url()))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| ExchangeError::Network(error.without_url()))?;
    if !status.is_success() {
        return Err(ExchangeError::UpstreamStatus {
            status: status.as_u16(),
        });
    }
    let parsed: TokenResponse = serde_json::from_str(&body).map_err(|_| ExchangeError::Parse)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    if parsed.token.is_empty() || parsed.expires_at <= now {
        return Err(ExchangeError::InvalidToken);
    }
    Ok(CopilotToken {
        token: parsed.token,
        expires_at: parsed.expires_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real shape of the response — derived from the VS Code Copilot Chat
    /// extension's `CopilotToken` interface.
    #[test]
    fn parses_token_response() -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>
    {
        let json = r#"{
            "annotations_enabled": true,
            "chat_enabled": true,
            "code_quote_enabled": true,
            "expires_at": 1700000000,
            "refresh_in": 1500,
            "sku": "copilot_for_business",
            "token": "tid=xxx;exp=1700000000;sku=copilot_for_business;…",
            "endpoints": {
              "api": "https://api.githubcopilot.com"
            }
        }"#;
        let parsed: TokenResponse = serde_json::from_str(json)?;
        assert!(parsed.token.starts_with("tid="));
        assert_eq!(parsed.expires_at, 1700000000);
        Ok(())
    }

    #[test]
    fn freshness_respects_60s_window()
    -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        // expires_at 30s from now — buffer says NOT fresh (we want to refresh).
        let token = CopilotToken {
            token: "x".into(),
            expires_at: now + 30,
        };
        assert!(!token.is_fresh());
        // expires_at 10 minutes from now — comfortably fresh.
        let token = CopilotToken {
            token: "x".into(),
            expires_at: now + 600,
        };
        assert!(token.is_fresh());
        Ok(())
    }
}
