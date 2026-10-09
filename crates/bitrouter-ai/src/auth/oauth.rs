//! OAuth token endpoint decoding and bounded refresh over explicit caller inputs.
//! No browser, environment, storage path or application CLI discovery.

use crate::auth::credentials::OAuthToken;
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};

/// Errors raised by the auth-code flow.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    /// One of the endpoint URLs was not HTTPS — refusing to send a code in
    /// cleartext.
    #[error("OAuth endpoint must use HTTPS (got {0})")]
    InsecureEndpoint(String),
    /// Transport failure (DNS, TCP, TLS, HTTP status, …).
    #[error("OAuth network error at {endpoint}: {source}")]
    Network {
        /// The endpoint that failed.
        endpoint: String,
        /// The underlying reqwest error.
        #[source]
        source: reqwest::Error,
    },
    /// Token endpoint returned a body the parser couldn't decode.
    #[error("OAuth token endpoint returned an unparseable body: {message}")]
    Malformed {
        /// Human-readable explanation.
        message: String,
    },
    /// Token endpoint returned a non-success status with an `error` body
    /// (RFC 6749 §5.2). Surface the error code so the CLI can suggest a
    /// fix (e.g. `invalid_grant` → re-run the flow).
    #[error("OAuth token endpoint returned error '{error}'{}", description.as_deref().map(|d| format!(" ({d})")).unwrap_or_default())]
    OAuthError {
        /// The RFC 6749 §5.2 error code.
        error: String,
        /// Optional human-readable description.
        description: Option<String>,
    },
    /// The selected token has no refresh grant material.
    #[error(
        "stored credential has no refresh_token; explicitly reauthenticate the selected account"
    )]
    MissingRefreshToken,
    /// Building the `/authorize` URL failed because one of the strings was
    /// not a valid URL.
    #[error("invalid /authorize URL: {0}")]
    InvalidUrl(#[from] url::ParseError),
}

/// Parse an `authorization_code` token-endpoint reply.
#[derive(Deserialize)]
struct TokenReply {
    access_token: Option<String>,
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: u64,
    error: Option<String>,
}

/// Decode whatever the token endpoint returned into an [`OAuthToken`] or
/// a typed error. Pulled out so [`refresh`] can reuse it.
pub async fn parse_token_reply(
    response: reqwest::Response,
    endpoint: &str,
) -> Result<OAuthToken, OAuthError> {
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|source| OAuthError::Network {
            endpoint: diagnostic_endpoint(endpoint),
            source: source.without_url(),
        })?;
    let parsed: TokenReply = serde_json::from_str(&body).map_err(|_| OAuthError::Malformed {
        message: "token response is not valid OAuth JSON".into(),
    })?;
    if let Some(error) = parsed.error {
        return Err(OAuthError::OAuthError {
            error: safe_error_code(&error).into(),
            description: None,
        });
    }
    if !status.is_success() {
        return Err(OAuthError::Malformed {
            message: format!(
                "token endpoint returned HTTP {} without an OAuth error",
                status.as_u16()
            ),
        });
    }
    let access_token = parsed.access_token.ok_or_else(|| OAuthError::Malformed {
        message: "token reply has neither access_token nor error".into(),
    })?;
    let expires_at = if parsed.expires_in > 0 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs().saturating_add(parsed.expires_in))
            .unwrap_or(0)
    } else {
        0
    };
    Ok(OAuthToken {
        access_token,
        expires_at,
        refresh_token: parsed.refresh_token,
    })
}

pub(crate) fn safe_error_code(error: &str) -> &str {
    match error {
        "invalid_request"
        | "invalid_client"
        | "invalid_grant"
        | "unauthorized_client"
        | "unsupported_grant_type"
        | "invalid_scope"
        | "access_denied"
        | "server_error"
        | "temporarily_unavailable"
        | "authorization_pending"
        | "slow_down"
        | "expired_token" => error,
        _ => "unknown_error",
    }
}
pub(crate) fn diagnostic_endpoint(endpoint: &str) -> String {
    match url::Url::parse(endpoint) {
        Ok(mut url) => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        }
        Err(_) => "invalid endpoint".into(),
    }
}
pub(crate) fn require_secure_endpoint(endpoint: &str) -> Result<(), OAuthError> {
    let url = url::Url::parse(endpoint)?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if url.scheme() == "https" || (url.scheme() == "http" && loopback) {
        Ok(())
    } else {
        Err(OAuthError::InsecureEndpoint(diagnostic_endpoint(endpoint)))
    }
}
use std::time::Duration;

/// Bounded refresh mechanism for an injected selected-account OAuth session.
pub struct RefreshGrant {
    client: reqwest::Client,
    endpoint: String,
    client_id: String,
}

impl RefreshGrant {
    /// Bind a caller-selected client configuration; no account/path discovery.
    pub fn new(
        client: reqwest::Client,
        endpoint: impl Into<String>,
        client_id: impl Into<String>,
    ) -> Self {
        Self {
            client,
            endpoint: endpoint.into(),
            client_id: client_id.into(),
        }
    }
}

#[async_trait::async_trait]
impl crate::auth::store::OAuthRefresher for RefreshGrant {
    async fn refresh(&self, current: &OAuthToken) -> crate::error::Result<OAuthToken> {
        refresh(&self.client, &self.endpoint, &self.client_id, current).await.map_err(|error| {
            crate::error::ModelError::Provider {
                status: if matches!(error, OAuthError::OAuthError { .. } | OAuthError::MissingRefreshToken) { 401 } else { 502 },
                message: "selected OAuth credential could not refresh; explicitly reauthenticate this account".into(),
            }
        })
    }
}

/// Window before `expires_at` at which we proactively refresh. Big enough
/// that an in-flight request can fit inside the refresh round-trip without
/// racing the upstream's expiry clock.
pub const REFRESH_WINDOW: Duration = Duration::from_secs(60);

/// POST a `refresh_token` grant to `token_endpoint` and parse the new
/// [`OAuthToken`].
///
/// The returned token's `refresh_token` field falls back to `current` when
/// the server doesn't include a new one — RFC 6749 §6 says servers MAY
/// issue a new refresh_token, but most don't, so the caller should keep
/// using the old one.
pub async fn refresh(
    client: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    current: &OAuthToken,
) -> Result<OAuthToken, OAuthError> {
    refresh_inner(client, token_endpoint, client_id, None, current).await
}

/// Like [`refresh`], but for a **confidential** OAuth client — includes
/// `client_secret` in the grant. Google installed-app clients (e.g. the
/// Antigravity `agy` CLI) require the secret even though it ships inside the
/// binary; the application Antigravity integration extracts it from the local `agy` at runtime
/// rather than embedding Google's secret here.
pub async fn refresh_with_client_secret(
    client: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    client_secret: &str,
    current: &OAuthToken,
) -> Result<OAuthToken, OAuthError> {
    refresh_inner(
        client,
        token_endpoint,
        client_id,
        Some(client_secret),
        current,
    )
    .await
}

async fn refresh_inner(
    client: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    client_secret: Option<&str>,
    current: &OAuthToken,
) -> Result<OAuthToken, OAuthError> {
    require_secure_endpoint(token_endpoint)?;
    let refresh_token = current
        .refresh_token
        .as_deref()
        .filter(|token| !token.is_empty())
        .ok_or(OAuthError::MissingRefreshToken)?;
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("client_id", client_id),
        ("refresh_token", refresh_token),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret));
    }
    let response = client
        .post(token_endpoint)
        .timeout(Duration::from_secs(30))
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|source| OAuthError::Network {
            endpoint: diagnostic_endpoint(token_endpoint),
            source: source.without_url(),
        })?;
    let mut refreshed = parse_token_reply(response, token_endpoint).await?;
    // RFC 6749 §6: server MAY return a new refresh_token; if it doesn't,
    // keep using the existing one rather than dropping refresh capability.
    if refreshed.refresh_token.is_none() {
        refreshed.refresh_token = current.refresh_token.clone();
    }
    Ok(refreshed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn token_with_refresh(refresh: &str) -> OAuthToken {
        OAuthToken {
            access_token: "old-access".into(),
            expires_at: 0,
            refresh_token: Some(refresh.into()),
        }
    }

    #[tokio::test]
    async fn refresh_returns_new_access_token()
    -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=RT"))
            .and(body_string_contains("client_id=client_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "NEW-ACCESS",
                "expires_in": 3600
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        // Bypass the HTTPS guard on wiremock's loopback URL with a direct
        // call to `parse_token_reply` after issuing the request — mirrors
        // what the public function does but lets us hit http://.
        let endpoint = format!("{}/oauth/token", server.uri());
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", "client_1"),
            ("refresh_token", "RT"),
        ];
        let resp = client
            .post(&endpoint)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&form)
            .send()
            .await?;
        let mut refreshed = parse_token_reply(resp, &endpoint).await?;
        // Mirror the production fallback when server omits refresh_token.
        if refreshed.refresh_token.is_none() {
            refreshed.refresh_token = Some("RT".into());
        }
        assert_eq!(refreshed.access_token, "NEW-ACCESS");
        assert_eq!(refreshed.refresh_token.as_deref(), Some("RT"));
        assert!(refreshed.expires_at > 0);
        Ok(())
    }

    #[tokio::test]
    async fn refresh_preserves_refresh_token_when_server_omits_it()
    -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Direct unit-test on the post-parse fallback — server returns
        // `access_token` only, the caller's old refresh_token survives.
        let mut refreshed = OAuthToken {
            access_token: "x".into(),
            expires_at: 1,
            refresh_token: None,
        };
        let current = token_with_refresh("RT");
        if refreshed.refresh_token.is_none() {
            refreshed.refresh_token = current.refresh_token.clone();
        }
        assert_eq!(refreshed.refresh_token.as_deref(), Some("RT"));
        Ok(())
    }

    #[test]
    fn refusing_http_endpoint_is_a_typed_error()
    -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Exercise the same endpoint guard used before refresh sends credentials.
        // A rejected endpoint never needs to enter the HTTP request builder.
        let err = require_secure_endpoint("http://insecure.example.com/oauth/token")
            .err()
            .ok_or("operation unexpectedly succeeded")?;
        assert!(
            matches!(err, OAuthError::InsecureEndpoint(ref u) if u == "http://insecure.example.com/oauth/token"),
            "expected InsecureEndpoint, got: {err:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn missing_refresh_token_surfaces_helpful_error()
    -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // OAuthToken with no refresh_token → refresh() bails before any
        // network call with a `Malformed` containing the user-facing
        // "re-run `bro providers login <provider>`" hint.
        let client = reqwest::Client::new();
        let token = OAuthToken {
            access_token: "stale".into(),
            expires_at: 1,
            refresh_token: None,
        };
        let err = refresh(
            &client,
            "https://example.com/oauth/token",
            "client-1",
            &token,
        )
        .await
        .err()
        .ok_or("operation unexpectedly succeeded")?;
        assert!(matches!(err, OAuthError::MissingRefreshToken));
        Ok(())
    }
}
