//! Authorization Server metadata — RFC 8414.
//!
//! Spec: <https://www.rfc-editor.org/rfc/rfc8414>.
//!
//! Given an AS base URL `<issuer>`, the metadata document is fetched
//! from `<issuer>/.well-known/oauth-authorization-server` and JSON-decoded
//! into [`AsMetadata`]. Only the fields the device-flow client actually
//! needs (`device_authorization_endpoint`, `token_endpoint`,
//! `revocation_endpoint`) are extracted; everything else is silently
//! ignored so an AS adding new fields doesn't break this client.

use anyhow::{Context, Result};
use serde::Deserialize;

use super::settings::require_secure_url;

/// The slice of RFC 8414 metadata bitrouter consumes.
///
/// `device_authorization_endpoint` (RFC 8628 §4) is the only endpoint
/// the device flow strictly requires alongside `token_endpoint` (RFC
/// 6749 §3.2). `revocation_endpoint` (RFC 7009 §3) is best-effort — when
/// the AS doesn't advertise one, `bitrouter cloud logout` skips the
/// network call and just deletes the local file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AsMetadata {
    /// REQUIRED by RFC 8414 §2 — the authorization server's issuer
    /// identifier. We don't verify it matches the user-supplied AS URL
    /// (some deployments serve the same metadata at multiple aliases),
    /// but we capture it for diagnostics.
    #[serde(default)]
    pub issuer: Option<String>,
    /// RFC 8628 §4 — endpoint the device flow POSTs to for the
    /// device_code + user_code pair.
    pub device_authorization_endpoint: String,
    /// RFC 6749 §3.2 — endpoint the device flow polls for the access
    /// token (and the refresh flow exchanges refresh tokens at).
    pub token_endpoint: String,
    /// RFC 7009 §3 — endpoint `logout` POSTs the token to for
    /// revocation. Optional; `None` when absent.
    #[serde(default)]
    pub revocation_endpoint: Option<String>,
}

/// Compose the well-known metadata URL for a given AS base URL.
/// Per RFC 8414 §3.1, any path component on the issuer gets inserted
/// after the `.well-known/oauth-authorization-server` suffix; for the
/// common path-less case the URL becomes
/// `<scheme>://<host>/.well-known/oauth-authorization-server`.
pub fn metadata_url(authorization_server: &str) -> Result<String> {
    let trimmed = authorization_server.trim_end_matches('/');
    let parsed = reqwest::Url::parse(trimmed)
        .with_context(|| format!("authorization server URL '{trimmed}' is not a valid URL"))?;
    let host = parsed
        .host_str()
        .with_context(|| format!("authorization server URL '{trimmed}' has no host"))?;
    let scheme = parsed.scheme();
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    let path = parsed.path().trim_end_matches('/');
    if path.is_empty() {
        Ok(format!(
            "{scheme}://{host}{port}/.well-known/oauth-authorization-server"
        ))
    } else {
        // RFC 8414 §3.1: `https://example.com/issuer1` → `https://example.com/.well-known/oauth-authorization-server/issuer1`.
        Ok(format!(
            "{scheme}://{host}{port}/.well-known/oauth-authorization-server{path}"
        ))
    }
}

/// Fetch + parse the AS metadata document. The returned metadata is
/// cached by the caller for the process lifetime (the AS metadata
/// changes rarely enough that a per-process cache is sufficient).
pub async fn fetch(client: &reqwest::Client, authorization_server: &str) -> Result<AsMetadata> {
    let url = metadata_url(authorization_server)?;
    let resp = client
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .with_context(|| format!("fetching AS metadata at {url}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .with_context(|| format!("reading AS metadata body from {url}"))?;
    if !status.is_success() {
        anyhow::bail!("AS metadata request to {url} failed: HTTP {status} — {body}");
    }
    let parsed: AsMetadata = serde_json::from_str(&body)
        .with_context(|| format!("parsing AS metadata JSON from {url}"))?;
    // RFC 9700 §2.1.1: token-endpoint traffic must be TLS in production.
    // Apply the same loopback-allowance rule we use for the AS URL itself.
    require_secure_url(&parsed.device_authorization_endpoint).with_context(|| {
        format!(
            "metadata at {url} advertises an insecure device_authorization_endpoint: {}",
            parsed.device_authorization_endpoint
        )
    })?;
    require_secure_url(&parsed.token_endpoint).with_context(|| {
        format!(
            "metadata at {url} advertises an insecure token_endpoint: {}",
            parsed.token_endpoint
        )
    })?;
    if let Some(rev) = parsed.revocation_endpoint.as_deref() {
        require_secure_url(rev).with_context(|| {
            format!("metadata at {url} advertises an insecure revocation_endpoint: {rev}")
        })?;
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_url_for_root_issuer() {
        let url = metadata_url("https://as.example.com").unwrap();
        assert_eq!(
            url,
            "https://as.example.com/.well-known/oauth-authorization-server"
        );
    }

    #[test]
    fn metadata_url_trims_trailing_slash() {
        let url = metadata_url("https://as.example.com/").unwrap();
        assert_eq!(
            url,
            "https://as.example.com/.well-known/oauth-authorization-server"
        );
    }

    #[test]
    fn metadata_url_preserves_path_component_per_rfc_8414() {
        // RFC 8414 §3.1: the path follows the well-known segment.
        let url = metadata_url("https://as.example.com/issuer1").unwrap();
        assert_eq!(
            url,
            "https://as.example.com/.well-known/oauth-authorization-server/issuer1"
        );
    }

    #[test]
    fn metadata_url_keeps_explicit_port() {
        let url = metadata_url("http://127.0.0.1:8080").unwrap();
        assert_eq!(
            url,
            "http://127.0.0.1:8080/.well-known/oauth-authorization-server"
        );
    }

    #[test]
    fn parses_minimal_metadata() {
        let json = r#"{
          "issuer": "https://as.example.com",
          "device_authorization_endpoint": "https://as.example.com/device",
          "token_endpoint": "https://as.example.com/token"
        }"#;
        let m: AsMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(m.issuer.as_deref(), Some("https://as.example.com"));
        assert_eq!(
            m.device_authorization_endpoint,
            "https://as.example.com/device"
        );
        assert_eq!(m.token_endpoint, "https://as.example.com/token");
        assert!(m.revocation_endpoint.is_none());
    }

    #[test]
    fn parses_metadata_with_revocation() {
        let json = r#"{
          "issuer": "https://as.example.com",
          "device_authorization_endpoint": "https://as.example.com/device",
          "token_endpoint": "https://as.example.com/token",
          "revocation_endpoint": "https://as.example.com/revoke",
          "unknown_field_for_forward_compat": "ignored"
        }"#;
        let m: AsMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(
            m.revocation_endpoint.as_deref(),
            Some("https://as.example.com/revoke")
        );
    }
}
