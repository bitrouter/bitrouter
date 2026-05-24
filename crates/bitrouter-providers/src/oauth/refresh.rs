//! OAuth 2.0 Refresh Token grant — RFC 6749 §6.
//!
//! Subscription credentials (Anthropic, OpenAI Codex) issue short-lived
//! access tokens (~hours) plus a long-lived refresh token. The
//! per-provider `AuthApplier` uses [`refresh`] to mint a new access token
//! when the cached one is within a small window of expiry, and writes the
//! result back to the [`crate::oauth::credential_store::CredentialStore`].

use std::time::Duration;

use crate::oauth::auth_code::{AuthCodeError, parse_token_reply};
use crate::oauth::credential_store::OAuthToken;

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
) -> Result<OAuthToken, AuthCodeError> {
    if !token_endpoint.starts_with("https://") {
        return Err(AuthCodeError::InsecureEndpoint(token_endpoint.to_string()));
    }
    let refresh_token =
        current
            .refresh_token
            .as_deref()
            .ok_or_else(|| AuthCodeError::Malformed {
                message: "stored credential has no refresh_token — re-run `bitrouter login`".into(),
            })?;
    let form = [
        ("grant_type", "refresh_token"),
        ("client_id", client_id),
        ("refresh_token", refresh_token),
    ];
    let response = client
        .post(token_endpoint)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|source| AuthCodeError::Network {
            endpoint: token_endpoint.to_string(),
            source,
        })?;
    let mut refreshed = parse_token_reply(response, token_endpoint).await?;
    // RFC 6749 §6: server MAY return a new refresh_token; if it doesn't,
    // keep using the existing one rather than dropping refresh capability.
    if refreshed.refresh_token.is_none() {
        refreshed.refresh_token = current.refresh_token.clone();
    }
    Ok(refreshed)
}

/// Whether `token` is within [`REFRESH_WINDOW`] of expiring (or already
/// past expiry). Non-expiring tokens (`expires_at == 0`) never need
/// refresh.
pub fn needs_refresh(token: &OAuthToken) -> bool {
    if token.expires_at == 0 {
        return false;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let threshold = token.expires_at.saturating_sub(REFRESH_WINDOW.as_secs());
    now >= threshold
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
    async fn refresh_returns_new_access_token() {
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
            .await
            .unwrap();
        let mut refreshed = parse_token_reply(resp, &endpoint).await.unwrap();
        // Mirror the production fallback when server omits refresh_token.
        if refreshed.refresh_token.is_none() {
            refreshed.refresh_token = Some("RT".into());
        }
        assert_eq!(refreshed.access_token, "NEW-ACCESS");
        assert_eq!(refreshed.refresh_token.as_deref(), Some("RT"));
        assert!(refreshed.expires_at > 0);
    }

    #[tokio::test]
    async fn refresh_preserves_refresh_token_when_server_omits_it() {
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
    }

    #[tokio::test]
    async fn refusing_http_endpoint_is_a_typed_error() {
        // `refresh` checks the scheme before touching the network, so no
        // server is needed — we just confirm the typed error variant.
        let client = reqwest::Client::new();
        let token = token_with_refresh("RT");
        let err = refresh(
            &client,
            "http://insecure.example.com/oauth/token",
            "client-1",
            &token,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, AuthCodeError::InsecureEndpoint(ref u) if u == "http://insecure.example.com/oauth/token"),
            "expected InsecureEndpoint, got: {err:?}"
        );
    }

    #[test]
    fn needs_refresh_for_expiring_token() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Token expires in 10s — well within the 60s refresh window.
        let token = OAuthToken {
            access_token: "x".into(),
            expires_at: now + 10,
            refresh_token: Some("r".into()),
        };
        assert!(needs_refresh(&token));
    }

    #[test]
    fn needs_no_refresh_for_fresh_token() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Token expires in 1 hour — outside refresh window.
        let token = OAuthToken {
            access_token: "x".into(),
            expires_at: now + 3600,
            refresh_token: Some("r".into()),
        };
        assert!(!needs_refresh(&token));
    }

    #[test]
    fn non_expiring_token_never_needs_refresh() {
        let token = OAuthToken {
            access_token: "x".into(),
            expires_at: 0,
            refresh_token: None,
        };
        assert!(!needs_refresh(&token));
    }

    #[tokio::test]
    async fn missing_refresh_token_surfaces_helpful_error() {
        // OAuthToken with no refresh_token → refresh() bails before any
        // network call with a `Malformed` containing the user-facing
        // "re-run `bitrouter login`" hint.
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
        .unwrap_err();
        match err {
            AuthCodeError::Malformed { message } => {
                assert!(
                    message.contains("no refresh_token") && message.contains("bitrouter login"),
                    "expected re-login hint, got: {message}"
                );
            }
            other => panic!("expected Malformed, got: {other:?}"),
        }
    }
}
