//! Resolve the authorization-server URL, client id, and scope from CLI
//! flag → env var → built-in default. This is the only place each input
//! is read, so the precedence rule lives in exactly one location.
//!
//! The AS URL and client id are intentionally NOT defaulted to any
//! particular deployment — bitrouter is an open-source project and the
//! device-flow client must work against any RFC 8628 compliant
//! authorization server the operator chooses.

use anyhow::{Context, Result};

/// Default `scope` value used when neither the flag nor the env var
/// supplies one. Per RFC 6749 §3.3, scope is a space-delimited list of
/// strings. These two scopes cover the most common API-key shape
/// (invoke an inference request, read usage). Users whose authorization
/// server uses different scope names override with `--scope` or
/// [`SCOPE_ENV`].
pub const DEFAULT_SCOPE: &str = "inference:invoke usage:read";

/// Environment variable for the authorization server base URL.
pub const AS_ENV: &str = "BITROUTER_OAUTH_AS";

/// Environment variable for the public OAuth client id.
pub const CLIENT_ID_ENV: &str = "BITROUTER_OAUTH_CLIENT_ID";

/// Environment variable for the OAuth `scope` parameter.
pub const SCOPE_ENV: &str = "BITROUTER_OAUTH_SCOPE";

/// The fully-resolved inputs for one device-flow run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Authorization-server base URL (no trailing slash). Used as the
    /// base for the RFC 8414 well-known metadata path.
    pub authorization_server: String,
    /// Public OAuth client id the user has registered with their AS.
    pub client_id: String,
    /// Space-delimited `scope` value (RFC 6749 §3.3).
    pub scope: String,
}

/// Pure resolution: flag → env var → default. Takes the env-var values
/// as inputs so the function is trivially testable. Trims trailing `/`
/// from the AS URL so the well-known metadata path joins cleanly.
pub fn resolve(
    flag_authorization_server: Option<&str>,
    flag_client_id: Option<&str>,
    flag_scope: Option<&str>,
    env_authorization_server: Option<&str>,
    env_client_id: Option<&str>,
    env_scope: Option<&str>,
) -> Result<Settings> {
    let authorization_server = first_non_empty(flag_authorization_server, env_authorization_server)
        .with_context(|| {
            format!(
                "no authorization server set — pass `--oauth-as <URL>` or set the {AS_ENV} env var"
            )
        })?
        .trim_end_matches('/')
        .to_string();
    require_secure_url(&authorization_server)?;
    let client_id = first_non_empty(flag_client_id, env_client_id)
        .with_context(|| {
            format!(
                "no OAuth client id set — pass `--client-id <ID>` or set the {CLIENT_ID_ENV} env var"
            )
        })?
        .to_string();
    let scope = first_non_empty(flag_scope, env_scope)
        .unwrap_or(DEFAULT_SCOPE)
        .to_string();
    Ok(Settings {
        authorization_server,
        client_id,
        scope,
    })
}

/// Convenience: pull each env var off the live process.
pub fn resolve_from_env(
    flag_authorization_server: Option<&str>,
    flag_client_id: Option<&str>,
    flag_scope: Option<&str>,
) -> Result<Settings> {
    let as_env = std::env::var(AS_ENV).ok();
    let cid_env = std::env::var(CLIENT_ID_ENV).ok();
    let scope_env = std::env::var(SCOPE_ENV).ok();
    resolve(
        flag_authorization_server,
        flag_client_id,
        flag_scope,
        as_env.as_deref().filter(|s| !s.is_empty()),
        cid_env.as_deref().filter(|s| !s.is_empty()),
        scope_env.as_deref().filter(|s| !s.is_empty()),
    )
}

fn first_non_empty<'a>(a: Option<&'a str>, b: Option<&'a str>) -> Option<&'a str> {
    a.filter(|v| !v.is_empty()).or(b.filter(|v| !v.is_empty()))
}

/// Refuse to send a client credential over plain HTTP unless the endpoint
/// is on a loopback interface — the RFC 8252 §8.3 exception kept around
/// for development against a local AS and for the test suite. RFC 9700
/// §2.1.1 reiterates that production token-endpoint traffic must be TLS.
pub fn require_secure_url(url: &str) -> Result<()> {
    if url.starts_with("https://") {
        return Ok(());
    }
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://localhost")
        || lower.starts_with("http://127.0.0.1")
        || lower.starts_with("http://[::1]")
    {
        return Ok(());
    }
    anyhow::bail!(
        "refusing to use insecure URL '{url}' — OAuth endpoints must be https:// \
         (loopback addresses are allowed for local testing per RFC 8252 §8.3)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_beats_env() {
        let r = resolve(
            Some("https://flag.example.com"),
            Some("flag-id"),
            None,
            Some("https://env.example.com"),
            Some("env-id"),
            None,
        )
        .unwrap();
        assert_eq!(r.authorization_server, "https://flag.example.com");
        assert_eq!(r.client_id, "flag-id");
        assert_eq!(r.scope, DEFAULT_SCOPE);
    }

    #[test]
    fn env_used_when_flag_absent() {
        let r = resolve(
            None,
            None,
            None,
            Some("https://env.example.com/"),
            Some("env-id"),
            Some("custom:scope"),
        )
        .unwrap();
        // Trailing slash trimmed.
        assert_eq!(r.authorization_server, "https://env.example.com");
        assert_eq!(r.client_id, "env-id");
        assert_eq!(r.scope, "custom:scope");
    }

    #[test]
    fn missing_as_url_errors_clearly() {
        let err = resolve(None, Some("cid"), None, None, None, None).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("BITROUTER_OAUTH_AS"), "msg: {msg}");
    }

    #[test]
    fn missing_client_id_errors_clearly() {
        let err =
            resolve(Some("https://as.example.com"), None, None, None, None, None).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("BITROUTER_OAUTH_CLIENT_ID"), "msg: {msg}");
    }

    #[test]
    fn empty_strings_treated_as_unset() {
        let err = resolve(Some(""), Some("cid"), None, None, None, None).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("authorization server"), "msg: {msg}");
    }

    #[test]
    fn rejects_plain_http_for_non_loopback() {
        let err = resolve(
            Some("http://as.example.com"),
            Some("cid"),
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("insecure URL"), "msg: {msg}");
    }

    #[test]
    fn allows_loopback_http_per_rfc_8252() {
        let r = resolve(
            Some("http://127.0.0.1:8080"),
            Some("cid"),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(r.authorization_server, "http://127.0.0.1:8080");
    }
}
