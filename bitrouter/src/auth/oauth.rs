//! OAuth 2.0 Device Authorization Grant (RFC 8628).
//!
//! Implements the device code flow for interactive token acquisition:
//! 1. Request a device code from the authorization server.
//! 2. Display the verification URI and user code.
//! 3. Poll the token endpoint until the user authorizes.

use serde::Deserialize;

use crate::auth::token_store::{OAuthToken, TokenStore};

/// Default GitHub device authorization endpoint.
pub const GITHUB_DEVICE_AUTH_URL: &str = "https://github.com/login/device/code";
/// Default GitHub token endpoint.
pub const GITHUB_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

/// Parameters for a device code flow request.
pub struct DeviceCodeParams {
    pub client_id: String,
    pub scope: Option<String>,
    pub device_auth_url: String,
    pub token_url: String,
}

/// Response from the device authorization endpoint (step 1).
#[derive(Debug, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    /// Polling interval in seconds.
    #[serde(default = "default_interval")]
    pub interval: u64,
}

fn default_interval() -> u64 {
    5
}

/// Token response from the token endpoint (step 3).
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    #[serde(default)]
    expires_in: u64,
    refresh_token: Option<String>,
}

/// Request a device code from the authorization server.
pub fn request_device_code(
    params: &DeviceCodeParams,
) -> Result<DeviceCodeResponse, Box<dyn std::error::Error>> {
    let client = reqwest::blocking::Client::new();
    let mut body = format!("client_id={}", url_encode(&params.client_id));
    if let Some(ref scope) = params.scope {
        body.push_str(&format!("&scope={}", url_encode(scope)));
    }

    let resp = client
        .post(&params.device_auth_url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()?;

    let status = resp.status();
    let body = resp.text()?;
    if !status.is_success() {
        return Err(format!("device code request failed ({status}): {body}").into());
    }

    let device_code: DeviceCodeResponse = serde_json::from_str(&body)
        .map_err(|e| format!("failed to parse device code response: {e}\nbody: {body}"))?;
    Ok(device_code)
}

/// Poll the token endpoint until authorization completes or an error occurs.
///
/// This function blocks the current thread, printing progress to stderr.
/// Returns the resulting [`OAuthToken`] on success.
pub fn poll_for_token(
    params: &DeviceCodeParams,
    device_code: &DeviceCodeResponse,
) -> Result<OAuthToken, Box<dyn std::error::Error>> {
    let client = reqwest::blocking::Client::new();
    let mut interval = std::time::Duration::from_secs(device_code.interval.max(1));

    loop {
        std::thread::sleep(interval);

        let body = format!(
            "client_id={}&device_code={}&grant_type={}",
            url_encode(&params.client_id),
            url_encode(&device_code.device_code),
            url_encode("urn:ietf:params:oauth:grant-type:device_code"),
        );
        let resp = client
            .post(&params.token_url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()?;

        let body = resp.text()?;
        let token_resp: TokenResponse = serde_json::from_str(&body)
            .map_err(|e| format!("failed to parse token response: {e}\nbody: {body}"))?;

        if let Some(access_token) = token_resp.access_token {
            let expires_at = if token_resp.expires_in > 0 {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() + token_resp.expires_in)
                    .unwrap_or(0)
            } else {
                0
            };
            return Ok(OAuthToken {
                access_token,
                expires_at,
                refresh_token: token_resp.refresh_token,
            });
        }

        match token_resp.error.as_deref() {
            Some("authorization_pending") => {
                // User hasn't authorized yet — keep polling.
                continue;
            }
            Some("slow_down") => {
                // Back off by 5 seconds as per RFC 8628 §3.5.
                interval += std::time::Duration::from_secs(5);
                continue;
            }
            Some("expired_token") => {
                return Err("device code expired — please try again".into());
            }
            Some("access_denied") => {
                return Err("authorization was denied by the user".into());
            }
            Some(other) => {
                return Err(format!("OAuth error: {other}").into());
            }
            None => {
                return Err(format!("unexpected token response: {body}").into());
            }
        }
    }
}

/// Run the full device code flow interactively.
///
/// 1. Requests a device code.
/// 2. Prints the verification URI and user code to stderr.
/// 3. Polls the token endpoint until authorization completes.
/// 4. Stores the token in the token store.
pub fn run_device_flow(
    provider_name: &str,
    params: &DeviceCodeParams,
    store: &mut TokenStore,
) -> Result<OAuthToken, Box<dyn std::error::Error>> {
    let device_code = request_device_code(params)?;

    eprintln!();
    eprintln!("  OAuth Device Authorization");
    eprintln!("  ──────────────────────────");
    eprintln!("  Open:  {}", device_code.verification_uri);
    eprintln!("  Code:  {}", device_code.user_code);
    eprintln!();
    eprintln!("  Waiting for authorization...");

    let token = poll_for_token(params, &device_code)?;

    store.set(provider_name, token.clone())?;

    eprintln!("  ✓ Authorized!");
    eprintln!();

    Ok(token)
}

/// Build [`DeviceCodeParams`] from an `AuthConfig::OAuth` variant.
pub fn params_from_oauth_config(
    client_id: &str,
    scope: Option<&str>,
    device_auth_url: Option<&str>,
    token_url: Option<&str>,
) -> DeviceCodeParams {
    DeviceCodeParams {
        client_id: client_id.to_owned(),
        scope: scope.map(str::to_owned),
        device_auth_url: device_auth_url.unwrap_or(GITHUB_DEVICE_AUTH_URL).to_owned(),
        token_url: token_url.unwrap_or(GITHUB_TOKEN_URL).to_owned(),
    }
}

/// Minimal percent-encoding for URL form values (RFC 3986).
fn url_encode(s: &str) -> String {
    let mut encoded = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    encoded
}
