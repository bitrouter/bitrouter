//! OAuth 2.0 Device Authorization Grant — RFC 8628.
//!
//! Spec: <https://www.rfc-editor.org/rfc/rfc8628>.
//! GitHub OAuth's device-flow profile: <https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps#device-flow>.
//!
//! ## Flow
//!
//! 1. POST `client_id` (+ optional `scope`) to the **device authorization
//!    endpoint** → server returns `device_code`, `user_code`,
//!    `verification_uri`, polling `interval`.
//! 2. Surface `verification_uri` + `user_code` to the human; they type the
//!    code in a browser to authorise the device.
//! 3. POST `client_id` + `device_code` + grant-type
//!    `urn:ietf:params:oauth:grant-type:device_code` to the **token
//!    endpoint** every `interval` seconds. RFC 8628 §3.5 reserved error
//!    codes:
//!    - `authorization_pending` — user hasn't acted; keep polling.
//!    - `slow_down` — back off `interval` by 5s and keep polling.
//!    - `access_denied` — user clicked "deny"; abort.
//!    - `expired_token` — `device_code` expired; abort.
//! 4. On success the token endpoint returns `{ access_token, expires_in?, refresh_token? }`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::auth::credentials::OAuthToken;

/// Inputs the device-code flow needs. Both URLs MUST be HTTPS — sending an
/// OAuth credential over `http://` would leak it to anyone on the path.
#[derive(Debug, Clone)]
pub struct DeviceCodeParams {
    /// OAuth client id.
    pub client_id: String,
    /// Optional `scope` parameter (RFC 6749 §3.3).
    pub scope: Option<String>,
    /// Device authorization endpoint (RFC 8628 §3.1).
    pub device_authorization_endpoint: String,
    /// Token endpoint (RFC 6749 §3.2).
    pub token_endpoint: String,
}

/// Response from the device authorization endpoint (RFC 8628 §3.2).
#[derive(Clone, Deserialize)]
pub struct DeviceCodeResponse {
    /// Long opaque code that proves the device's identity to the server.
    pub device_code: String,
    /// Short code the user types in the browser.
    pub user_code: String,
    /// URI the user visits to type `user_code`. GitHub returns
    /// `https://github.com/login/device`.
    pub verification_uri: String,
    /// Pre-encoded URI that includes `user_code` — surface this when present.
    /// `serde` keeps it absent when the server omits it.
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    /// Polling interval (seconds). Defaulted to 5s per RFC 8628 §3.5.
    #[serde(default = "default_interval")]
    pub interval: u64,
    /// Lifetime of `device_code` (seconds).
    #[serde(default)]
    pub expires_in: u64,
}

impl std::fmt::Debug for DeviceCodeResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceCodeResponse")
            .field("device_code", &"<redacted>")
            .field("user_code", &"<redacted>")
            .field("verification_uri", &self.verification_uri)
            .field(
                "verification_uri_complete",
                &self
                    .verification_uri_complete
                    .as_ref()
                    .map(|_| "<redacted>"),
            )
            .field("interval", &self.interval)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

fn default_interval() -> u64 {
    5
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    #[serde(default)]
    expires_in: u64,
    refresh_token: Option<String>,
}

/// Errors raised by the device-code flow.
#[derive(Debug, thiserror::Error)]
pub enum FlowError {
    /// Transport failure (DNS, TCP, TLS, HTTP status, …).
    #[error("OAuth network error at {endpoint}: {source}")]
    Network {
        /// The endpoint that failed.
        endpoint: String,
        /// The underlying reqwest error.
        #[source]
        source: reqwest::Error,
    },
    /// Endpoint URL isn't HTTPS — refusing to send a credential in cleartext.
    #[error("OAuth endpoint must use HTTPS (got {0})")]
    InsecureEndpoint(String),
    /// Token endpoint returned `access_denied` — the user clicked "deny".
    #[error("the user denied the OAuth authorization request")]
    AccessDenied,
    /// Token endpoint returned `expired_token` — the device code expired
    /// before the user authorised. Re-run the flow.
    #[error("the device code expired before authorization completed")]
    DeviceCodeExpired,
    /// Token endpoint returned a recognised RFC 8628 error other than the
    /// above (e.g. `invalid_grant`, `invalid_client`).
    #[error("OAuth token endpoint returned error '{0}'")]
    OAuthError(String),
    /// Server returned a body the parser couldn't decode.
    #[error("OAuth server returned an unparseable body at {endpoint}: {message}")]
    Malformed {
        /// The endpoint whose body was malformed.
        endpoint: String,
        /// Human-readable explanation.
        message: String,
    },
}

/// Driver for the device-code flow.
#[derive(Debug)]
pub struct DeviceCodeFlow {
    client: reqwest::Client,
    params: DeviceCodeParams,
}

impl DeviceCodeFlow {
    /// New flow over a fresh reqwest client.
    pub fn new(params: DeviceCodeParams) -> Result<Self, FlowError> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("bitrouter-ai/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|source| FlowError::Network {
                endpoint: "client-build".into(),
                source,
            })?;
        Self::with_client(client, params)
    }

    /// New flow over a caller-owned reqwest client.
    pub fn with_client(
        client: reqwest::Client,
        params: DeviceCodeParams,
    ) -> Result<Self, FlowError> {
        require_https(&params.device_authorization_endpoint)?;
        require_https(&params.token_endpoint)?;
        Ok(Self { client, params })
    }

    /// Step 1 of RFC 8628 §3.1 — request a device + user code.
    pub async fn request_device_code(&self) -> Result<DeviceCodeResponse, FlowError> {
        let mut form = vec![("client_id", self.params.client_id.as_str())];
        if let Some(scope) = &self.params.scope {
            form.push(("scope", scope.as_str()));
        }
        let response = self
            .client
            .post(&self.params.device_authorization_endpoint)
            .timeout(Duration::from_secs(30))
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&form)
            .send()
            .await
            .map_err(|source| FlowError::Network {
                endpoint: super::oauth::diagnostic_endpoint(
                    &self.params.device_authorization_endpoint,
                ),
                source: source.without_url(),
            })?;
        let endpoint =
            super::oauth::diagnostic_endpoint(&self.params.device_authorization_endpoint);
        let body = response
            .error_for_status()
            .map_err(|source| FlowError::Network {
                endpoint: endpoint.clone(),
                source: source.without_url(),
            })?
            .text()
            .await
            .map_err(|source| FlowError::Network {
                endpoint: endpoint.clone(),
                source: source.without_url(),
            })?;
        serde_json::from_str(&body).map_err(|_| FlowError::Malformed {
            endpoint,
            message: "device authorization response is not valid JSON".into(),
        })
    }

    /// Step 3 of RFC 8628 §3.4 — poll the token endpoint once. Returns
    /// a token, pending/backoff state, or an error for terminal cases.
    pub async fn poll_once(&self, device_code: &str) -> Result<PollOutcome, FlowError> {
        let form = [
            ("client_id", self.params.client_id.as_str()),
            ("device_code", device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ];
        let response = self
            .client
            .post(&self.params.token_endpoint)
            .timeout(Duration::from_secs(30))
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&form)
            .send()
            .await
            .map_err(|source| FlowError::Network {
                endpoint: super::oauth::diagnostic_endpoint(&self.params.token_endpoint),
                source: source.without_url(),
            })?;
        let endpoint = super::oauth::diagnostic_endpoint(&self.params.token_endpoint);
        let status = response.status();
        // RFC 8628 §3.5: error replies are still HTTP 200 in some servers
        // and 4xx in others. Read the body first; let the JSON's `error`
        // field be the source of truth.
        let body = response.text().await.map_err(|source| FlowError::Network {
            endpoint: endpoint.clone(),
            source: source.without_url(),
        })?;
        let parsed: TokenResponse =
            serde_json::from_str(&body).map_err(|_| FlowError::Malformed {
                endpoint: endpoint.clone(),
                message: "token response is not valid OAuth JSON".into(),
            })?;
        if let Some(error) = parsed.error.as_deref() {
            return match error {
                "authorization_pending" => Ok(PollOutcome::Pending),
                "slow_down" => Ok(PollOutcome::SlowDown),
                "access_denied" => Err(FlowError::AccessDenied),
                "expired_token" => Err(FlowError::DeviceCodeExpired),
                other => Err(FlowError::OAuthError(
                    super::oauth::safe_error_code(other).into(),
                )),
            };
        }
        if !status.is_success() {
            return Err(FlowError::Malformed {
                endpoint,
                message: format!(
                    "token endpoint returned HTTP {} without an OAuth error",
                    status.as_u16()
                ),
            });
        }
        if let Some(access_token) = parsed.access_token {
            if access_token.is_empty() {
                return Err(FlowError::Malformed {
                    endpoint,
                    message: "token response contains an empty access token".into(),
                });
            }
            let expires_at = if parsed.expires_in > 0 {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs().saturating_add(parsed.expires_in))
                    .unwrap_or(0)
            } else {
                0
            };
            return Ok(PollOutcome::Token(OAuthToken {
                access_token,
                expires_at,
                refresh_token: parsed.refresh_token,
            }));
        }
        Err(FlowError::Malformed {
            endpoint,
            message: "token endpoint reply has neither token nor error".into(),
        })
    }

    /// Poll an already issued code with its server-provided expiration and
    /// interval. Each pending/slow-down response retains the original deadline.
    /// Invoke immediately after requesting the code; `expires_in` is the
    /// remaining lifetime at this call's start. Dropping this future stops its
    /// sleep and any in-flight HTTP request.
    pub async fn wait_for_token(
        &self,
        device: &DeviceCodeResponse,
    ) -> Result<OAuthToken, FlowError> {
        let deadline = tokio::time::Instant::now()
            .checked_add(Duration::from_secs(device.expires_in))
            .ok_or(FlowError::DeviceCodeExpired)?;
        let mut interval = Duration::from_secs(device.interval.max(1));
        loop {
            let outcome = tokio::time::timeout_at(deadline, async {
                tokio::time::sleep(interval).await;
                if tokio::time::Instant::now() >= deadline {
                    return Err(FlowError::DeviceCodeExpired);
                }
                self.poll_once(&device.device_code).await
            })
            .await
            .map_err(|_| FlowError::DeviceCodeExpired)??;
            if tokio::time::Instant::now() >= deadline {
                return Err(FlowError::DeviceCodeExpired);
            }
            match outcome {
                PollOutcome::Token(token) => return Ok(token),
                PollOutcome::Pending => {}
                PollOutcome::SlowDown => interval = interval.saturating_add(Duration::from_secs(5)),
            }
        }
    }
}

/// One poll's outcome.
#[derive(Debug)]
pub enum PollOutcome {
    /// User authorised; here is the access token.
    Token(OAuthToken),
    /// Keep polling at the current interval.
    Pending,
    /// Server asked us to back off; increase the interval by 5s.
    SlowDown,
}

fn require_https(url: &str) -> Result<(), FlowError> {
    if url.starts_with("https://") {
        Ok(())
    } else {
        Err(FlowError::InsecureEndpoint(
            super::oauth::diagnostic_endpoint(url),
        ))
    }
}

#[cfg(test)]
mod tests {
    type TestResult = Result<(), Box<dyn std::error::Error>>;
    use super::*;

    #[test]
    fn rejects_http_endpoints() -> TestResult {
        let params = DeviceCodeParams {
            client_id: "test".into(),
            scope: None,
            device_authorization_endpoint: "http://example.com/device".into(),
            token_endpoint: "https://example.com/token".into(),
        };
        let err = DeviceCodeFlow::new(params)
            .err()
            .ok_or("expected failure")?;
        assert!(matches!(err, FlowError::InsecureEndpoint(_)));
        Ok(())
    }

    /// The device-code response sample from GitHub's official docs:
    /// <https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps#response-parameters>.
    #[test]
    fn parses_github_device_code_response() -> TestResult {
        let json = r#"{
          "device_code": "3584d83530557fdd1f46af8289938c8ef79f9dc5",
          "user_code": "WDJB-MJHT",
          "verification_uri": "https://github.com/login/device",
          "expires_in": 900,
          "interval": 5
        }"#;
        let resp: DeviceCodeResponse = serde_json::from_str(json)?;
        assert_eq!(resp.user_code, "WDJB-MJHT");
        assert_eq!(resp.interval, 5);
        assert!(resp.verification_uri_complete.is_none());
        Ok(())
    }

    #[test]
    fn defaults_interval_when_server_omits_it() -> TestResult {
        let json = r#"{
          "device_code": "x",
          "user_code": "y",
          "verification_uri": "https://example.com"
        }"#;
        let resp: DeviceCodeResponse = serde_json::from_str(json)?;
        assert_eq!(resp.interval, 5);
        Ok(())
    }

    fn local_flow(server: &wiremock::MockServer) -> DeviceCodeFlow {
        // Constructor HTTPS admission is tested separately. Exercise the same
        // production HTTP/poll implementation over a local fixture here.
        DeviceCodeFlow {
            client: reqwest::Client::new(),
            params: DeviceCodeParams {
                client_id: "caller-client".into(),
                scope: Some("caller-scope".into()),
                device_authorization_endpoint: format!("{}/device", server.uri()),
                token_endpoint: format!("{}/token", server.uri()),
            },
        }
    }

    fn issued_code(expires_in: u64, interval: u64) -> DeviceCodeResponse {
        DeviceCodeResponse {
            device_code: "selected-device".into(),
            user_code: "selected-user".into(),
            verification_uri: "https://selected.invalid/verify".into(),
            verification_uri_complete: None,
            expires_in,
            interval,
        }
    }

    #[tokio::test]
    async fn expiration_during_sleep_prevents_any_token_request() -> TestResult {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"access_token":"too-late"})),
            )
            .expect(0)
            .mount(&server)
            .await;
        let result = local_flow(&server)
            .wait_for_token(&issued_code(1, 10))
            .await;
        assert!(matches!(result, Err(FlowError::DeviceCodeExpired)));
        server.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn pending_and_slow_down_keep_the_original_expiration() -> TestResult {
        for code in ["authorization_pending", "slow_down"] {
            let server = wiremock::MockServer::start().await;
            wiremock::Mock::given(wiremock::matchers::path("/token"))
                .respond_with(
                    wiremock::ResponseTemplate::new(400)
                        .set_body_json(serde_json::json!({"error":code})),
                )
                .expect(1)
                .mount(&server)
                .await;
            let result = local_flow(&server).wait_for_token(&issued_code(2, 1)).await;
            assert!(matches!(result, Err(FlowError::DeviceCodeExpired)));
            server.verify().await;
        }
        Ok(())
    }

    #[tokio::test]
    async fn expiration_bounds_an_actual_in_flight_token_request() -> TestResult {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(5))
                    .set_body_json(serde_json::json!({"access_token":"too-late"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let result = local_flow(&server).wait_for_token(&issued_code(2, 1)).await;
        assert!(matches!(result, Err(FlowError::DeviceCodeExpired)));
        server.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn actual_device_and_token_http_preserve_inputs_and_safe_failures() -> TestResult {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers};
        let server = MockServer::start().await;
        Mock::given(matchers::path("/device"))
            .and(matchers::body_string_contains("scope=caller-scope"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "device_code":"selected-device", "user_code":"selected-user",
                "verification_uri":"https://selected.invalid/verify", "expires_in":10
            })))
            .expect(1)
            .mount(&server)
            .await;
        let flow = local_flow(&server);
        let device = flow.request_device_code().await?;
        assert_eq!(device.device_code, "selected-device");
        assert_eq!(device.interval, 5);
        for (status, body) in [
            (200, r#"{"access_token":"reply-secret""#),
            (500, r#"{"access_token":"reply-secret"}"#),
            (400, r#"{"error":"reply-secret"}"#),
            (200, r#"{"access_token":"","refresh_token":"reply-secret"}"#),
        ] {
            Mock::given(matchers::path("/token"))
                .and(matchers::body_string_contains("client_id=caller-client"))
                .and(matchers::body_string_contains(
                    "device_code=selected-device",
                ))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .up_to_n_times(1)
                .expect(1)
                .mount(&server)
                .await;
            let error = flow
                .poll_once(&device.device_code)
                .await
                .err()
                .ok_or("invalid token reply succeeded")?;
            assert!(!format!("{error:?} {error}").contains("reply-secret"));
        }
        server.verify().await;
        Ok(())
    }
}
