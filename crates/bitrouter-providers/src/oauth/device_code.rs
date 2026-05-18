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

use crate::oauth::token_store::OAuthToken;

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
#[derive(Debug, Clone, Deserialize)]
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

/// Events streamed back to the caller as the flow progresses.
#[derive(Debug, Clone)]
pub enum FlowEvent {
    /// Initial device authorization completed; show the user the code +
    /// verification URI. Polling for the access token begins after this.
    UserPromptReady(DeviceCodeResponse),
    /// Still waiting for the user to type the code in their browser.
    StillPending,
    /// Server asked us to back off — polling interval increased by 5s.
    SlowedDown(Duration),
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
            .user_agent(concat!("bitrouter-providers/", env!("CARGO_PKG_VERSION")))
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
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&form)
            .send()
            .await
            .map_err(|source| FlowError::Network {
                endpoint: self.params.device_authorization_endpoint.clone(),
                source,
            })?;
        let endpoint = self.params.device_authorization_endpoint.clone();
        let body = response
            .error_for_status()
            .map_err(|source| FlowError::Network {
                endpoint: endpoint.clone(),
                source,
            })?
            .text()
            .await
            .map_err(|source| FlowError::Network {
                endpoint: endpoint.clone(),
                source,
            })?;
        serde_json::from_str(&body).map_err(|e| FlowError::Malformed {
            endpoint,
            message: format!(
                "device code response: {e}; body preview: {}",
                preview(&body)
            ),
        })
    }

    /// Step 3 of RFC 8628 §3.4 — poll the token endpoint once. Returns
    /// `Ok(Some(token))` on success, `Ok(None)` to keep polling, or an error
    /// for terminal cases.
    pub async fn poll_once(&self, device_code: &str) -> Result<PollOutcome, FlowError> {
        let form = [
            ("client_id", self.params.client_id.as_str()),
            ("device_code", device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ];
        let response = self
            .client
            .post(&self.params.token_endpoint)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&form)
            .send()
            .await
            .map_err(|source| FlowError::Network {
                endpoint: self.params.token_endpoint.clone(),
                source,
            })?;
        let endpoint = self.params.token_endpoint.clone();
        // RFC 8628 §3.5: error replies are still HTTP 200 in some servers
        // and 4xx in others. Read the body first; let the JSON's `error`
        // field be the source of truth.
        let body = response.text().await.map_err(|source| FlowError::Network {
            endpoint: endpoint.clone(),
            source,
        })?;
        let parsed: TokenResponse =
            serde_json::from_str(&body).map_err(|e| FlowError::Malformed {
                endpoint,
                message: format!("token response: {e}; body preview: {}", preview(&body)),
            })?;
        if let Some(access_token) = parsed.access_token {
            let expires_at = if parsed.expires_in > 0 {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() + parsed.expires_in)
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
        match parsed.error.as_deref() {
            Some("authorization_pending") => Ok(PollOutcome::Pending),
            Some("slow_down") => Ok(PollOutcome::SlowDown),
            Some("access_denied") => Err(FlowError::AccessDenied),
            Some("expired_token") => Err(FlowError::DeviceCodeExpired),
            Some(other) => Err(FlowError::OAuthError(other.to_string())),
            None => Err(FlowError::Malformed {
                endpoint: self.params.token_endpoint.clone(),
                message: format!("token endpoint reply with neither token nor error: {body}"),
            }),
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
        Err(FlowError::InsecureEndpoint(url.to_string()))
    }
}

fn preview(body: &str) -> String {
    body.chars().take(240).collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_http_endpoints() {
        let params = DeviceCodeParams {
            client_id: "test".into(),
            scope: None,
            device_authorization_endpoint: "http://example.com/device".into(),
            token_endpoint: "https://example.com/token".into(),
        };
        let err = DeviceCodeFlow::new(params).unwrap_err();
        assert!(matches!(err, FlowError::InsecureEndpoint(_)));
    }

    /// The device-code response sample from GitHub's official docs:
    /// <https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps#response-parameters>.
    #[test]
    fn parses_github_device_code_response() {
        let json = r#"{
          "device_code": "3584d83530557fdd1f46af8289938c8ef79f9dc5",
          "user_code": "WDJB-MJHT",
          "verification_uri": "https://github.com/login/device",
          "expires_in": 900,
          "interval": 5
        }"#;
        let resp: DeviceCodeResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.user_code, "WDJB-MJHT");
        assert_eq!(resp.interval, 5);
        assert!(resp.verification_uri_complete.is_none());
    }

    #[test]
    fn defaults_interval_when_server_omits_it() {
        let json = r#"{
          "device_code": "x",
          "user_code": "y",
          "verification_uri": "https://example.com"
        }"#;
        let resp: DeviceCodeResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.interval, 5);
    }
}
