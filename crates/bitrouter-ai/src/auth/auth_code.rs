//! OAuth 2.0 Authorization Code Flow + PKCE — RFC 6749 §4.1, RFC 7636.
//!
//! 1. Mint a [`super::pkce::PkcePair`] and a [`super::pkce::generate_state`] nonce.
//! 2. Build the `/authorize` URL with [`build_authorize_url`] and open it
//!    in the user's browser (or print it for them to paste).
//! 3. Catch the redirect on a [`super::listener::LoopbackListener`], or
//!    accept a manual paste of the full redirect URL when no listener is
//!    feasible.
//! 4. POST `code` + `code_verifier` to the token endpoint with
//!    [`exchange_code`] to get an [`OAuthToken`].
//!
//! Subscription endpoints (claude.ai, auth.openai.com) layer extra
//! authorize-URL params on top of the RFC defaults (`originator`,
//! `id_token_add_organizations`, …). The [`AuthCodeParams::extra_authorize`]
//! field carries those — see [`crate::providers::login`] for provider inputs.

use std::collections::BTreeMap;
use std::time::Duration;

use url::Url;

use crate::auth::credentials::OAuthToken;
use crate::auth::oauth::{OAuthError, parse_token_reply};
use crate::auth::pkce::CHALLENGE_METHOD;

/// Inputs to the Authorization Code flow.
#[derive(Debug, Clone)]
pub struct AuthCodeParams {
    /// Public OAuth client id registered with the upstream.
    pub client_id: String,
    /// `/authorize` endpoint the browser is redirected to. MUST be HTTPS.
    pub authorize_endpoint: String,
    /// `/token` endpoint the code is POSTed to. MUST be HTTPS.
    pub token_endpoint: String,
    /// `scope` parameter (space-separated). Empty is allowed for servers
    /// that derive scope from the client registration.
    pub scope: String,
    /// Provider-specific extras to splat into the `/authorize` URL —
    /// `originator`, `id_token_add_organizations`, etc. Sorted output for
    /// deterministic URLs (handy in tests).
    pub extra_authorize: BTreeMap<String, String>,
}

/// Build the `/authorize` URL — RFC 6749 §4.1.1 + RFC 7636 §4.3 query
/// params, plus whatever extras [`AuthCodeParams::extra_authorize`]
/// declared. The caller opens this in a browser.
pub fn build_authorize_url(
    params: &AuthCodeParams,
    redirect_uri: &str,
    state: &str,
    code_challenge: &str,
) -> Result<String, OAuthError> {
    require_https(&params.authorize_endpoint)?;
    let mut url = Url::parse(&params.authorize_endpoint)?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code");
        q.append_pair("client_id", &params.client_id);
        q.append_pair("redirect_uri", redirect_uri);
        if !params.scope.is_empty() {
            q.append_pair("scope", &params.scope);
        }
        q.append_pair("state", state);
        q.append_pair("code_challenge", code_challenge);
        q.append_pair("code_challenge_method", CHALLENGE_METHOD);
        for (k, v) in &params.extra_authorize {
            q.append_pair(k, v);
        }
    }
    Ok(url.into())
}

/// POST `code` + `code_verifier` to the token endpoint and parse the
/// resulting [`OAuthToken`]. `redirect_uri` MUST match what was sent on
/// the `/authorize` request.
pub async fn exchange_code(
    client: &reqwest::Client,
    params: &AuthCodeParams,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<OAuthToken, OAuthError> {
    require_https(&params.token_endpoint)?;
    exchange_validated_code(client, params, code, code_verifier, redirect_uri).await
}

async fn exchange_validated_code(
    client: &reqwest::Client,
    params: &AuthCodeParams,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<OAuthToken, OAuthError> {
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", params.client_id.as_str()),
        ("code", code),
        ("code_verifier", code_verifier),
        ("redirect_uri", redirect_uri),
    ];
    let response = client
        .post(&params.token_endpoint)
        .timeout(Duration::from_secs(30))
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|source| OAuthError::Network {
            endpoint: super::oauth::diagnostic_endpoint(&params.token_endpoint),
            source: source.without_url(),
        })?;
    let token = parse_token_reply(response, &params.token_endpoint).await?;
    if token.access_token.is_empty() {
        return Err(OAuthError::Malformed {
            message: "authorization response contains an empty access token".into(),
        });
    }
    Ok(token)
}

/// Parse a redirect URL (or just `code=…&state=…` query body) the user
/// pasted at the manual-fallback prompt. Accepts either:
///
/// - `http://127.0.0.1:1455/auth/callback?code=…&state=…`
/// - `code=…&state=…`
/// - bare `<code>` — `state` ends up empty; callers usually re-prompt.
#[derive(Clone, PartialEq, Eq)]
pub struct PastedRedirect {
    /// The `code` we'll exchange.
    pub code: String,
    /// The `state` echoed back — caller compares against the value sent.
    pub state: Option<String>,
}

impl std::fmt::Debug for PastedRedirect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PastedRedirect")
            .field("code", &"<redacted>")
            .field("state", &self.state.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Parse a pasted redirect URL or query body. Returns `None` if no `code`
/// could be extracted at all.
pub fn parse_pasted_redirect(input: &str) -> Option<PastedRedirect> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Full URL — let the url crate handle the parsing.
    if let Ok(url) = Url::parse(trimmed) {
        let mut code = None;
        let mut state = None;
        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                _ => {}
            }
        }
        if let Some(code) = code {
            return Some(PastedRedirect { code, state });
        }
        // URL but no `code` query — probably a stale paste.
        return None;
    }
    // Bare `code=…&state=…` body.
    if trimmed.contains('=') && trimmed.contains("code=") {
        let mut code = None;
        let mut state = None;
        for pair in trimmed.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            match k.trim() {
                "code" => code = Some(v.to_string()),
                "state" => state = Some(v.to_string()),
                _ => {}
            }
        }
        if let Some(code) = code {
            return Some(PastedRedirect { code, state });
        }
    }
    // Bare token — assume it's just the `code` value.
    if !trimmed.contains(char::is_whitespace) && !trimmed.contains('/') {
        return Some(PastedRedirect {
            code: trimmed.to_string(),
            state: None,
        });
    }
    None
}

fn require_https(url: &str) -> Result<(), OAuthError> {
    if url.starts_with("https://") {
        Ok(())
    } else {
        Err(OAuthError::InsecureEndpoint(
            super::oauth::diagnostic_endpoint(url),
        ))
    }
}

#[cfg(test)]
mod tests {
    type TestResult = Result<(), Box<dyn std::error::Error>>;
    use super::*;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn params() -> AuthCodeParams {
        let mut extra = BTreeMap::new();
        extra.insert("originator".into(), "bitrouter".into());
        AuthCodeParams {
            client_id: "client_1".into(),
            authorize_endpoint: "https://authz.example.com/oauth/authorize".into(),
            token_endpoint: "https://authz.example.com/oauth/token".into(),
            scope: "read write".into(),
            extra_authorize: extra,
        }
    }

    #[test]
    fn builds_authorize_url_with_all_params() -> TestResult {
        let url = build_authorize_url(
            &params(),
            "http://127.0.0.1:1455/auth/callback",
            "STATE",
            "CHALLENGE",
        )?;
        let parsed = Url::parse(&url)?;
        let q: BTreeMap<String, String> = parsed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["client_id"], "client_1");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:1455/auth/callback");
        assert_eq!(q["scope"], "read write");
        assert_eq!(q["state"], "STATE");
        assert_eq!(q["code_challenge"], "CHALLENGE");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["originator"], "bitrouter");
        Ok(())
    }

    #[test]
    fn rejects_http_authorize_endpoint() -> TestResult {
        let mut p = params();
        p.authorize_endpoint = "http://insecure.example.com/oauth/authorize".into();
        let err = build_authorize_url(&p, "http://127.0.0.1/cb", "s", "c")
            .err()
            .ok_or("expected failure")?;
        assert!(matches!(err, OAuthError::InsecureEndpoint(_)));
        Ok(())
    }

    #[tokio::test]
    async fn exchange_code_returns_oauth_token() -> TestResult {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code=AUTHCODE"))
            .and(body_string_contains("code_verifier=VERIFIER"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "ACCESS",
                "refresh_token": "REFRESH",
                "expires_in": 3600
            })))
            .expect(1)
            .mount(&server)
            .await;
        let mut p = params();
        p.authorize_endpoint =
            format!("{}/oauth/authorize", server.uri()).replace("http://", "https://"); // formality — not used here
        p.token_endpoint = format!("{}/oauth/token", server.uri());
        // Bypass HTTPS check for the wiremock server.
        let result = wiremock_exchange(&p, "AUTHCODE", "VERIFIER", "http://127.0.0.1/cb").await;
        let token = result?;
        assert_eq!(token.access_token, "ACCESS");
        assert_eq!(token.refresh_token.as_deref(), Some("REFRESH"));
        assert!(token.expires_at > 0);
        Ok(())
    }

    #[tokio::test]
    async fn exchange_code_surfaces_oauth_error() -> TestResult {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "code already redeemed"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let mut p = params();
        p.token_endpoint = format!("{}/oauth/token", server.uri());
        let err = wiremock_exchange(&p, "x", "y", "http://127.0.0.1/cb")
            .await
            .err()
            .ok_or("expected failure")?;
        match err {
            OAuthError::OAuthError { error, description } => {
                assert_eq!(error, "invalid_grant");
                assert!(description.is_none());
            }
            other => return Err(format!("unexpected: {other:?}").into()),
        }
        Ok(())
    }

    /// Test helper — POSTs to `params.token_endpoint` directly (no HTTPS
    /// check) so we can hit wiremock's `http://127.0.0.1:<port>` server.
    /// Production calls go through [`exchange_code`].
    async fn wiremock_exchange(
        params: &AuthCodeParams,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<OAuthToken, OAuthError> {
        exchange_validated_code(
            &reqwest::Client::new(),
            params,
            code,
            verifier,
            redirect_uri,
        )
        .await
    }

    #[test]
    fn parses_pasted_full_url() -> TestResult {
        let got = parse_pasted_redirect("http://127.0.0.1:1455/auth/callback?code=AC&state=ST")
            .ok_or("missing pasted redirect")?;
        assert_eq!(got.code, "AC");
        assert_eq!(got.state.as_deref(), Some("ST"));
        Ok(())
    }

    #[test]
    fn parses_pasted_query_body() -> TestResult {
        let got = parse_pasted_redirect("code=AC&state=ST").ok_or("missing pasted redirect")?;
        assert_eq!(got.code, "AC");
        assert_eq!(got.state.as_deref(), Some("ST"));
        Ok(())
    }

    #[test]
    fn parses_bare_code() -> TestResult {
        let got = parse_pasted_redirect("just-a-code-value").ok_or("missing pasted redirect")?;
        assert_eq!(got.code, "just-a-code-value");
        assert!(got.state.is_none());
        Ok(())
    }

    #[test]
    fn rejects_empty_paste() -> TestResult {
        assert!(parse_pasted_redirect("").is_none());
        assert!(parse_pasted_redirect("   ").is_none());
        Ok(())
    }
}
