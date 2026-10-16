//! Credential values independent of filesystem and account selection.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// One stored OAuth credential.
///
/// `Debug` redacts `access_token` and `refresh_token` so a future
/// `tracing::error!(?token, …)` can't dump the credential to the log stream.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthToken {
    /// The credential the upstream API expects on `Authorization: Bearer …`.
    pub access_token: String,
    /// Unix seconds at which `access_token` becomes invalid. `0` means
    /// non-expiring (treat as valid forever).
    #[serde(default)]
    pub expires_at: u64,
    /// Optional refresh token. Some providers (GitHub OAuth Apps) don't issue
    /// one; OAuth Device Flow is then re-run when the access token expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

impl std::fmt::Debug for OAuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthToken")
            .field(
                "access_token",
                &if self.access_token.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .field("expires_at", &self.expires_at)
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl OAuthToken {
    /// Whether `access_token` is still valid at the current wall-clock time.
    /// Non-expiring tokens (`expires_at == 0`) always count as valid.
    pub fn is_valid(&self) -> bool {
        if self.expires_at == 0 {
            return true;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now < self.expires_at
    }
}

/// One stored credential — either a static API key or an OAuth credential.
///
/// Adjacently tagged (`{ "type": …, "data": { … } }`) so the OAuth variant
/// can wrap the existing [`OAuthToken`] as-is without duplicating its fields.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Credential {
    /// A static API key — what `bro providers login <provider>` stores
    /// when the user picks "paste an API key" instead of a browser OAuth flow.
    /// Treated as never-expiring.
    ApiKey {
        /// The plaintext key value (e.g. `sk-ant-api03-…`, `sk-…`).
        value: String,
    },
    /// An OAuth credential plus optional refresh metadata.
    Oauth(OAuthToken),
    /// A tokenless marker meaning "resolve the credential live from the Claude
    /// Code CLI's own store (`~/.claude`) at request time, and write any
    /// refresh back there". No token is copied into this store, so bitrouter
    /// and Claude Code share one credential and can't refresh-rotate each other
    /// out (RFC 6749 §6). Set by `bro providers login claude-code`;
    /// consumed by the Claude Code `AuthApplier`. Serialized as the
    /// unit-variant `{"type": "claude_code_cli"}`.
    ClaudeCodeCli,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credential::ApiKey { value } => f
                .debug_struct("ApiKey")
                .field(
                    "value",
                    &if value.is_empty() {
                        "<empty>"
                    } else {
                        "<redacted>"
                    },
                )
                .finish(),
            Credential::Oauth(token) => f.debug_tuple("Oauth").field(token).finish(),
            Credential::ClaudeCodeCli => f.write_str("ClaudeCodeCli"),
        }
    }
}

impl Credential {
    /// Whether this credential is currently usable. API keys are always
    /// considered valid; OAuth credentials defer to [`OAuthToken::is_valid`].
    pub fn is_valid(&self) -> bool {
        match self {
            Credential::ApiKey { .. } => true,
            Credential::Oauth(t) => t.is_valid(),
            // Liveness is resolved at request time from the Claude Code store;
            // the marker is never itself "expired".
            Credential::ClaudeCodeCli => true,
        }
    }

    /// Build an OAuth credential from an [`OAuthToken`].
    pub fn from_oauth_token(token: OAuthToken) -> Self {
        Credential::Oauth(token)
    }

    /// Build an API-key credential.
    pub fn api_key(value: impl Into<String>) -> Self {
        Credential::ApiKey {
            value: value.into(),
        }
    }

    /// Borrow the inner OAuth token, if this is the OAuth variant.
    pub fn as_oauth(&self) -> Option<&OAuthToken> {
        match self {
            Credential::Oauth(t) => Some(t),
            Credential::ApiKey { .. } | Credential::ClaudeCodeCli => None,
        }
    }

    /// Borrow the API-key value, if this is the API-key variant.
    pub fn as_api_key(&self) -> Option<&str> {
        match self {
            Credential::ApiKey { value } => Some(value.as_str()),
            Credential::Oauth(_) | Credential::ClaudeCodeCli => None,
        }
    }

    /// Short, log-safe description of the credential kind. Used by CLI
    /// messages — never includes the credential itself.
    pub fn kind_label(&self) -> &'static str {
        match self {
            Credential::ApiKey { .. } => "API key",
            Credential::Oauth(_) => "OAuth",
            Credential::ClaudeCodeCli => "Claude Code session",
        }
    }
}
