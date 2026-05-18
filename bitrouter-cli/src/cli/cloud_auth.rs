//! BitRouter Cloud device-authorization client (RFC 8628 flavor).
//!
//! Implements `bitrouter login`, `bitrouter logout`, `bitrouter whoami`:
//!
//!   login  → POST  {base}/api/auth/device/code   (better-auth plugin)
//!            poll  {base}/api/device/token       (our custom route)
//!            save  ~/.bitrouter/credentials
//!
//! The token endpoint is intentionally *not* the plugin's
//! `/api/auth/device/token`. That one returns a browser session token;
//! our custom `/api/device/token` mints a `brk_*` API key instead so the
//! CLI identity matches what the node verifies on the Bearer path.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::cli::cloud_credentials::CloudCredentials;

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Client identifier sent with the device-code request.
const CLIENT_ID: &str = "bitrouter-cli";

/// Fallback cloud URL when neither `--url` nor `BITROUTER_CLOUD_URL` is set.
pub const DEFAULT_CLOUD_URL: &str = "https://bitrouter.ai";

/// RFC 8628 §3.4 — `slow_down` backs off the polling interval by 5s.
const SLOW_DOWN_BACKOFF_SECS: u64 = 5;

/// Resolve the BitRouter Cloud base URL.
///
/// Precedence:
///   1. explicit `--url` flag
///   2. `BITROUTER_CLOUD_URL` env var
///   3. [`DEFAULT_CLOUD_URL`]
///
/// Trailing slashes are trimmed so path concatenation is safe.
pub fn resolve_cloud_url(explicit: Option<&str>) -> String {
    let raw = explicit
        .map(str::to_owned)
        .or_else(|| std::env::var("BITROUTER_CLOUD_URL").ok())
        .unwrap_or_else(|| DEFAULT_CLOUD_URL.to_owned());
    raw.trim_end_matches('/').to_owned()
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default = "default_expires_in")]
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

fn default_interval() -> u64 {
    5
}

fn default_expires_in() -> u64 {
    900
}

#[derive(Debug, Deserialize)]
struct ApiKeyResponse {
    api_key: String,
    key_id: String,
}

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// Run the full device-authorization flow and persist the resulting
/// `brk_*` key to `<home>/credentials`.
///
/// Blocks the calling thread on `reqwest::blocking` — callers running
/// inside a Tokio runtime should wrap the call in `block_in_place`.
pub fn run_login(home: &Path, base_url: &str) -> Result {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let device = request_device_code(&client, base_url)?;

    eprintln!();
    eprintln!("  Sign in to BitRouter Cloud");
    eprintln!("  ──────────────────────────");
    let open_url = device
        .verification_uri_complete
        .as_deref()
        .unwrap_or(device.verification_uri.as_str());
    eprintln!("  Open:  {open_url}");
    eprintln!("  Code:  {}", device.user_code);
    eprintln!();
    eprintln!(
        "  Waiting for approval (times out in {}s)...",
        device.expires_in
    );

    let minted = poll_for_api_key(&client, base_url, &device)?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let creds = CloudCredentials {
        api_key: minted.api_key,
        key_id: minted.key_id,
        base_url: base_url.to_owned(),
        minted_at: now,
    };
    creds.save(home)?;

    eprintln!("  ✓ Logged in");
    eprintln!(
        "    Credentials saved to {}",
        CloudCredentials::path(home).display()
    );
    eprintln!();
    Ok(())
}

/// Delete stored credentials. No-op if the file is missing.
pub fn run_logout(home: &Path) -> Result {
    let path = CloudCredentials::path(home);
    let existed = path.exists();
    CloudCredentials::delete(home)?;
    if existed {
        println!("Logged out. Credentials at {} removed.", path.display());
    } else {
        println!("Not logged in (no credentials file).");
    }
    Ok(())
}

/// Print the stored key id and cloud URL. Does not print the api_key.
pub fn run_whoami(home: &Path) -> Result {
    match CloudCredentials::load(home) {
        Some(creds) => {
            println!("Logged in to {}", creds.base_url);
            println!("  key id:    {}", creds.key_id);
            println!("  minted at: {}", creds.minted_at);
        }
        None => {
            println!("Not logged in. Run `bitrouter login` to authenticate.");
        }
    }
    Ok(())
}

// ── internals ─────────────────────────────────────────────────────────

fn request_device_code(
    client: &reqwest::blocking::Client,
    base_url: &str,
) -> Result<DeviceCodeResponse> {
    let url = format!("{base_url}/api/auth/device/code");
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "client_id": CLIENT_ID }))
        .send()?;

    let status = resp.status();
    let body = resp.text()?;
    if !status.is_success() {
        return Err(format!("device code request failed ({status}): {body}").into());
    }
    let parsed: DeviceCodeResponse = serde_json::from_str(&body)
        .map_err(|e| format!("failed to parse device code response: {e}\nbody: {body}"))?;
    Ok(parsed)
}

fn poll_for_api_key(
    client: &reqwest::blocking::Client,
    base_url: &str,
    device: &DeviceCodeResponse,
) -> Result<ApiKeyResponse> {
    let url = format!("{base_url}/api/device/token");
    let mut interval = Duration::from_secs(device.interval.max(1));
    let deadline =
        std::time::Instant::now() + Duration::from_secs(device.expires_in.max(device.interval));

    loop {
        if std::time::Instant::now() >= deadline {
            return Err("device code expired before approval — please try again".into());
        }
        std::thread::sleep(interval);

        let resp = client
            .post(&url)
            .json(&serde_json::json!({
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
                "device_code": device.device_code,
                "client_id": CLIENT_ID,
            }))
            .send()?;

        let status = resp.status();
        let body = resp.text()?;

        if status.is_success() {
            let minted: ApiKeyResponse = serde_json::from_str(&body)
                .map_err(|e| format!("failed to parse token response: {e}\nbody: {body}"))?;
            return Ok(minted);
        }

        // 4xx: RFC 8628 error envelope `{error, error_description}`.
        let err: ErrorResponse = serde_json::from_str(&body)
            .map_err(|e| format!("unexpected token error ({status}): {e}\nbody: {body}"))?;
        match err.error.as_deref() {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval += Duration::from_secs(SLOW_DOWN_BACKOFF_SECS);
                continue;
            }
            Some("expired_token") => {
                return Err("device code expired — please try again".into());
            }
            Some("access_denied") => {
                return Err("approval denied in the browser".into());
            }
            Some(other) => {
                let detail = err
                    .error_description
                    .as_deref()
                    .unwrap_or("(no description)");
                return Err(format!("login failed: {other} — {detail}").into());
            }
            None => {
                return Err(format!("unexpected token response ({status}): {body}").into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Env-var precedence tests are bundled into a single serial test so
    // parallel test threads don't race on the shared `BITROUTER_CLOUD_URL`.
    #[test]
    fn resolve_cloud_url_precedence() {
        // Ensure a clean baseline.
        // SAFETY: we scope all env-var writes to this single test and
        // restore the original state on exit.
        let saved = std::env::var("BITROUTER_CLOUD_URL").ok();
        unsafe { std::env::remove_var("BITROUTER_CLOUD_URL") };

        // 1. Nothing set → default, trailing slashes trimmed on explicit.
        assert_eq!(resolve_cloud_url(None), DEFAULT_CLOUD_URL);
        assert_eq!(
            resolve_cloud_url(Some("https://example.com///")),
            "https://example.com"
        );

        // 2. Env var supplies the URL and is trimmed.
        unsafe { std::env::set_var("BITROUTER_CLOUD_URL", "http://localhost:3000/") };
        assert_eq!(resolve_cloud_url(None), "http://localhost:3000");

        // 3. Explicit flag beats env var.
        assert_eq!(
            resolve_cloud_url(Some("http://from-flag")),
            "http://from-flag"
        );

        // Restore baseline for any later test in the same binary.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("BITROUTER_CLOUD_URL", v),
                None => std::env::remove_var("BITROUTER_CLOUD_URL"),
            }
        }
    }
}
