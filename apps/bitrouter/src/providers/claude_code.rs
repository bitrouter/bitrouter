//! Application-side Claude Code credential capture and live-store adoption.
//! Request authentication/shaping lives in `bitrouter_ai::providers::claude_code`.
//! This module supplies explicit environment and file/Keychain policy above AI.

pub mod store;

use bitrouter_ai::auth::credentials::OAuthToken;

/// Claude Code's official long-lived OAuth token environment variable.
///
/// Tokens created by `claude setup-token` are process credentials, so bitrouter
/// treats them as in-memory, non-refreshable OAuth bearers.
pub const OAUTH_TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// Whether the current process carries a usable Claude Code OAuth token in the
/// official environment variable.
pub fn oauth_token_env_present() -> bool {
    env_oauth_token().is_some()
}

/// Capture the official environment token as an application-permitted fallback.
pub fn env_oauth_token() -> Option<OAuthToken> {
    let access_token = std::env::var(OAUTH_TOKEN_ENV).ok()?;
    let access_token = access_token.trim();
    if access_token.is_empty() {
        return None;
    }
    Some(OAuthToken {
        access_token: access_token.to_string(),
        expires_at: 0,
        refresh_token: None,
    })
}
