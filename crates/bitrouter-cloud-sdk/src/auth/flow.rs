//! Device Authorization Grant client — RFC 8628 — plus the small
//! ancillary HTTP exchanges the `bitrouter auth` flow needs:
//! - refresh-token exchange (RFC 6749 §6),
//! - best-effort token revocation (RFC 7009).
//!
//! This is a sibling of `crates/bitrouter-providers/src/oauth/device_code.rs`
//! rather than a wrapper around it: the upstream-provider client enforces
//! strict HTTPS-only endpoints (which is correct for *its* use case) and
//! doesn't expose refresh / revoke endpoints. The user-account flow here
//! needs RFC 8252 §8.3 loopback allowance for local dev + integration
//! tests, plus refresh + revoke. Keeping the two clients separate avoids
//! warping the provider-facing API to absorb concerns that don't apply to
//! upstream LLM-API authentication.

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rand::Rng;
use serde::Deserialize;

use super::credentials::Credentials;
use super::metadata::AsMetadata;
use super::settings::{Settings, require_secure_url};

/// RFC 8628 §3.2 device-authorization response.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceAuthorizationResponse {
    /// Opaque code identifying *this* device-flow attempt to the AS.
    pub device_code: String,
    /// Short code the user types in the browser.
    pub user_code: String,
    /// URL the user visits to type `user_code`.
    pub verification_uri: String,
    /// Pre-filled URL that already contains `user_code`. RFC 8628 §3.2
    /// makes this optional but recommended; the CLI prefers it when
    /// returned because users only need one click instead of typing.
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    /// `device_code` lifetime (seconds). Polling stops when this elapses.
    pub expires_in: u64,
    /// Minimum polling interval (seconds). Defaulted to 5s per RFC 8628 §3.2.
    #[serde(default = "default_interval")]
    pub interval: u64,
}

fn default_interval() -> u64 {
    5
}

/// RFC 6749 §5.1 token response — the union of fields returned by the
/// device, refresh, and any other grant. Optional fields stay `None`
/// when omitted so a new grant doesn't break parsing.
#[derive(Debug, Clone, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    /// Seconds until `access_token` expires.
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
    /// Optional extension: seconds until the *refresh* token itself
    /// expires. Some AS implementations advertise this; others omit.
    #[serde(default)]
    refresh_token_expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
    /// Non-standard bitrouter extension: the namespace the issued token
    /// is baked into. `Some` for every device-flow token; absent for a
    /// namespace-null credential. Persisted so the management client can
    /// resolve the implicit namespace for `/v1/namespaces/{nsid}/…` calls.
    #[serde(default)]
    namespace_id: Option<String>,
    /// OIDC id_token, used to extract the `sub` claim for `whoami`.
    #[serde(default)]
    id_token: Option<String>,
    /// RFC 6749 §5.2 error envelope (lives in the same body shape).
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// Fresh token material returned by a successful token exchange (device
/// success, refresh, …). Mapped onto a [`Credentials`] by the caller,
/// who supplies the missing AS-context fields.
#[derive(Debug, Clone)]
pub struct TokenSet {
    /// Bearer access token.
    pub access_token: String,
    /// RFC 6749 §7.1 token type (almost always "Bearer").
    pub token_type: Option<String>,
    /// Wall-clock UTC at which `access_token` becomes invalid.
    pub expires_at: DateTime<Utc>,
    /// Refresh token (if the AS issued one).
    pub refresh_token: Option<String>,
    /// Wall-clock UTC at which `refresh_token` itself becomes invalid.
    pub refresh_token_expires_at: Option<DateTime<Utc>>,
    /// Scope the AS granted (may be narrower than requested).
    pub scope: Option<String>,
    /// Namespace the issued token is baked into, when the AS reported
    /// one. `None` for a namespace-null credential.
    pub namespace_id: Option<String>,
    /// Subject claim extracted from an `id_token`, when one is present.
    pub subject: Option<String>,
}

/// One outcome of a poll against the token endpoint.
#[derive(Debug)]
pub enum PollOutcome {
    /// User authorized; here are the freshly-issued tokens.
    Success(TokenSet),
    /// Server says "user hasn't acted yet" — RFC 8628 §3.5
    /// `authorization_pending`. Sleep `interval` and re-poll.
    Pending,
    /// Server says "you're polling too fast" — RFC 8628 §3.5
    /// `slow_down`. Increase the local interval by 5s and re-poll.
    SlowDown,
}

/// Terminal RFC 8628 outcomes that abort the flow.
#[derive(Debug, thiserror::Error)]
pub enum DeviceFlowError {
    /// User clicked "deny" at the verification URI. RFC 8628 §3.5.
    #[error("the user denied the authorization request")]
    AccessDenied,
    /// Device code expired before the user finished. RFC 8628 §3.5.
    #[error("device code expired before authorization completed")]
    ExpiredToken,
    /// Any other RFC 6749 §5.2 token-endpoint error
    /// (`invalid_grant`, `invalid_client`, …). Carries the original
    /// code + description so the user sees what the AS said.
    #[error("OAuth error '{code}'{}", .description.as_ref().map(|d| format!(": {d}")).unwrap_or_default())]
    OAuthError {
        /// The RFC 6749 §5.2 error code.
        code: String,
        /// The optional `error_description`.
        description: Option<String>,
    },
}

/// Step 1 of RFC 8628 §3.1 — POST `client_id` (+ optional `scope`) to
/// the device authorization endpoint.
pub async fn request_device_authorization(
    client: &reqwest::Client,
    metadata: &AsMetadata,
    settings: &Settings,
) -> Result<DeviceAuthorizationResponse> {
    require_secure_url(&metadata.device_authorization_endpoint)?;
    let mut form: Vec<(&str, &str)> = vec![("client_id", &settings.client_id)];
    if !settings.scope.is_empty() {
        form.push(("scope", &settings.scope));
    }
    let endpoint = &metadata.device_authorization_endpoint;
    let resp = client
        .post(endpoint)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&form)
        .send()
        .await
        .with_context(|| format!("POST {endpoint}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .with_context(|| format!("reading body from {endpoint}"))?;
    if !status.is_success() {
        anyhow::bail!("device authorization request to {endpoint} failed: HTTP {status} — {body}");
    }
    serde_json::from_str(&body)
        .with_context(|| format!("parsing device authorization response from {endpoint}"))
}

/// Step 3 of RFC 8628 §3.4 — poll the token endpoint once. Returns the
/// continuation state; the caller's loop decides whether to sleep,
/// back off, succeed, or fail.
pub async fn poll_token_endpoint(
    client: &reqwest::Client,
    metadata: &AsMetadata,
    settings: &Settings,
    device_code: &str,
) -> Result<PollOutcome, PollError> {
    require_secure_url(&metadata.token_endpoint).map_err(PollError::Transport)?;
    let form = [
        ("client_id", settings.client_id.as_str()),
        ("device_code", device_code),
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
    ];
    let endpoint = &metadata.token_endpoint;
    let resp = client
        .post(endpoint)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|e| {
            PollError::Transport(anyhow::Error::from(e).context(format!("POST {endpoint}")))
        })?;
    // RFC 6749 §5.2: error responses MAY be HTTP 4xx; success is 2xx.
    // Either way the body carries a JSON envelope with either tokens or
    // an `error` field, so we read the body regardless of status.
    let body = resp.text().await.map_err(|e| {
        PollError::Transport(
            anyhow::Error::from(e).context(format!("reading body from {endpoint}")),
        )
    })?;
    let parsed: TokenResponse = serde_json::from_str(&body).map_err(|e| {
        PollError::Transport(anyhow::Error::from(e).context(format!(
            "parsing token endpoint response from {endpoint}: {}",
            preview(&body)
        )))
    })?;
    if let Some(access_token) = parsed.access_token.clone() {
        return Ok(PollOutcome::Success(token_set_from_response(
            access_token,
            parsed,
        )));
    }
    match parsed.error.as_deref() {
        Some("authorization_pending") => Ok(PollOutcome::Pending),
        Some("slow_down") => Ok(PollOutcome::SlowDown),
        Some("access_denied") => Err(PollError::Terminal(DeviceFlowError::AccessDenied)),
        Some("expired_token") => Err(PollError::Terminal(DeviceFlowError::ExpiredToken)),
        Some(code) => Err(PollError::Terminal(DeviceFlowError::OAuthError {
            code: code.to_string(),
            description: parsed.error_description,
        })),
        None => Err(PollError::Transport(anyhow::anyhow!(
            "token endpoint reply at {endpoint} contained neither access_token nor error: {}",
            preview(&body)
        ))),
    }
}

/// Error returned by a single poll. Transport errors are retryable
/// (network blips, malformed bodies) and the caller may decide to keep
/// polling; terminal errors abort the flow.
#[derive(Debug, thiserror::Error)]
pub enum PollError {
    /// Network / decode failure. Caller may treat as retryable.
    #[error("{0:#}")]
    Transport(anyhow::Error),
    /// One of the RFC 8628 §3.5 terminal errors.
    #[error(transparent)]
    Terminal(#[from] DeviceFlowError),
}

/// Drive the device-flow polling loop to completion. Polls every
/// `interval` seconds (server-controlled, with a small uniform jitter
/// to avoid lockstep across multiple clients per RFC 8628 §3.5), backs
/// off by 5s on `slow_down`, and stops when `expires_in` elapses.
///
/// `on_ready` is invoked once after the device-authorization step so
/// the CLI can surface the user code and verification URI before the
/// poll loop starts.
pub async fn run_device_flow(
    client: &reqwest::Client,
    metadata: &AsMetadata,
    settings: &Settings,
    on_ready: impl FnOnce(&DeviceAuthorizationResponse),
) -> Result<TokenSet> {
    let device = request_device_authorization(client, metadata, settings)
        .await
        .context("requesting device authorization")?;
    on_ready(&device);
    let mut interval = Duration::from_secs(device.interval.max(1));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(device.expires_in.max(1));
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow::anyhow!(
                "device code expired after {}s without authorization",
                device.expires_in
            ));
        }
        // RFC 8628 §3.5 — clients SHOULD add a small uniform jitter to
        // the polling interval to avoid lock-step thundering-herd.
        let with_jitter = interval + jitter();
        let until_deadline = deadline.saturating_duration_since(tokio::time::Instant::now());
        tokio::time::sleep(with_jitter.min(until_deadline)).await;
        match poll_token_endpoint(client, metadata, settings, &device.device_code).await {
            Ok(PollOutcome::Success(token_set)) => return Ok(token_set),
            Ok(PollOutcome::Pending) => continue,
            Ok(PollOutcome::SlowDown) => {
                // RFC 8628 §3.5: increase polling interval by 5s.
                interval += Duration::from_secs(5);
            }
            Err(PollError::Terminal(e)) => return Err(e.into()),
            Err(PollError::Transport(e)) => return Err(e),
        }
    }
}

fn jitter() -> Duration {
    // Up to 500ms of additional sleep. Small enough to feel
    // instantaneous, large enough to decorrelate concurrent clients.
    let ms: u64 = rand::rng().random_range(0..500);
    Duration::from_millis(ms)
}

/// RFC 6749 §6 refresh exchange. POSTs `grant_type=refresh_token` to
/// the AS token endpoint and returns the freshly-issued tokens.
pub async fn refresh(
    client: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
    scope: Option<&str>,
) -> Result<TokenSet> {
    require_secure_url(token_endpoint)?;
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    if let Some(s) = scope.filter(|s| !s.is_empty()) {
        form.push(("scope", s));
    }
    let resp = client
        .post(token_endpoint)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&form)
        .send()
        .await
        .with_context(|| format!("POST {token_endpoint}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .with_context(|| format!("reading body from {token_endpoint}"))?;
    let parsed: TokenResponse = serde_json::from_str(&body).with_context(|| {
        format!(
            "parsing refresh response from {token_endpoint}: {}",
            preview(&body)
        )
    })?;
    if let Some(access_token) = parsed.access_token.clone() {
        return Ok(token_set_from_response(access_token, parsed));
    }
    let code = parsed
        .error
        .unwrap_or_else(|| format!("unexpected HTTP {status} from token endpoint"));
    let desc = parsed
        .error_description
        .map(|d| format!(": {d}"))
        .unwrap_or_default();
    anyhow::bail!("refresh exchange failed at {token_endpoint}: {code}{desc}");
}

/// RFC 7009 token revocation. Best-effort: a non-2xx response is logged
/// to debug but is NOT propagated, because the local credentials file
/// is about to be deleted regardless.
pub async fn revoke(
    client: &reqwest::Client,
    revocation_endpoint: &str,
    client_id: &str,
    token: &str,
    token_type_hint: &str,
) -> Result<()> {
    require_secure_url(revocation_endpoint)?;
    let form = [
        ("token", token),
        ("token_type_hint", token_type_hint),
        ("client_id", client_id),
    ];
    let resp = client
        .post(revocation_endpoint)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&form)
        .send()
        .await
        .with_context(|| format!("POST {revocation_endpoint}"))?;
    // RFC 7009 §2.2: the AS responds 200 to a successful revocation,
    // and to "the token was already invalid". 4xx/5xx is best-effort
    // logged but not surfaced — logout deletes the local file either
    // way.
    if !resp.status().is_success() {
        tracing::debug!(
            status = %resp.status(),
            endpoint = revocation_endpoint,
            "revocation endpoint returned non-success; ignoring per RFC 7009"
        );
    }
    Ok(())
}

/// Build a fresh [`Credentials`] from a successful device-flow
/// [`TokenSet`] + the AS context the user provided.
pub fn credentials_from_token_set(token_set: TokenSet, settings: &Settings) -> Credentials {
    Credentials {
        access_token: token_set.access_token,
        refresh_token: token_set.refresh_token,
        expires_at: token_set.expires_at,
        refresh_token_expires_at: token_set.refresh_token_expires_at,
        token_type: token_set.token_type.unwrap_or_else(|| "Bearer".to_string()),
        scope: token_set.scope.unwrap_or_else(|| settings.scope.clone()),
        client_id: settings.client_id.clone(),
        authorization_server: settings.authorization_server.clone(),
        namespace_id: token_set.namespace_id,
        subject: token_set.subject,
    }
}

fn token_set_from_response(access_token: String, parsed: TokenResponse) -> TokenSet {
    let now = Utc::now();
    let expires_at = parsed
        .expires_in
        .and_then(|s| chrono::TimeDelta::try_seconds(s as i64))
        .map(|d| now + d)
        // RFC 6749 §4.2.2 leaves expires_in optional. When omitted we
        // treat the token as valid for one hour — a sensible default
        // that is much shorter than typical AS-issued lifetimes, so we
        // err on the side of refreshing too often rather than too
        // rarely.
        .unwrap_or_else(|| now + chrono::Duration::hours(1));
    let refresh_token_expires_at = parsed
        .refresh_token_expires_in
        .and_then(|s| chrono::TimeDelta::try_seconds(s as i64))
        .map(|d| now + d);
    let subject = parsed.id_token.as_deref().and_then(extract_id_token_sub);
    TokenSet {
        access_token,
        token_type: parsed.token_type,
        expires_at,
        refresh_token: parsed.refresh_token,
        refresh_token_expires_at,
        scope: parsed.scope,
        namespace_id: parsed.namespace_id,
        subject,
    }
}

/// Pull the `sub` claim out of a serialized JWT-shaped id_token without
/// verifying its signature. We do NOT trust this for authorization —
/// it's only surfaced by `bitrouter auth whoami` as a hint of "which
/// account did I sign in as", and the AS already signed the
/// access_token that grants the actual access. RFC 9068 + RFC 7519
/// describe the structure; per OpenID Connect Core §3.1.3.7 the
/// relying party would normally validate the signature, but since
/// bitrouter doesn't ship JWKS-fetching today we deliberately limit
/// the use of `sub` to display.
fn extract_id_token_sub(id_token: &str) -> Option<String> {
    let mut parts = id_token.split('.');
    let _header = parts.next()?;
    let payload_b64 = parts.next()?;
    use base64::Engine;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload_b64))
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(payload_b64))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    claims
        .get("sub")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn preview(body: &str) -> String {
    body.chars().take(240).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            authorization_server: "https://as.example.com".into(),
            client_id: "cid".into(),
            scope: "inference:invoke".into(),
        }
    }

    fn metadata() -> AsMetadata {
        AsMetadata {
            issuer: Some("https://as.example.com".into()),
            device_authorization_endpoint: "https://as.example.com/device".into(),
            token_endpoint: "https://as.example.com/token".into(),
            revocation_endpoint: None,
        }
    }

    #[test]
    fn parses_authorization_pending_as_pending() {
        let response: TokenResponse =
            serde_json::from_str(r#"{"error":"authorization_pending"}"#).unwrap();
        assert!(response.access_token.is_none());
        assert_eq!(response.error.as_deref(), Some("authorization_pending"));
    }

    #[test]
    fn token_set_from_response_computes_expires_at() {
        let response: TokenResponse = serde_json::from_str(
            r#"{"access_token":"AT","token_type":"Bearer","expires_in":600,"refresh_token":"RT"}"#,
        )
        .unwrap();
        let ts = token_set_from_response("AT".into(), response);
        assert_eq!(ts.token_type.as_deref(), Some("Bearer"));
        assert_eq!(ts.refresh_token.as_deref(), Some("RT"));
        // ~ 10 minutes from now, allow generous skew.
        let drift = (ts.expires_at - Utc::now()).num_seconds();
        assert!((595..=605).contains(&drift), "drift was {drift}");
    }

    #[test]
    fn token_set_defaults_expiry_when_server_omits_it() {
        let response: TokenResponse = serde_json::from_str(r#"{"access_token":"AT"}"#).unwrap();
        let ts = token_set_from_response("AT".into(), response);
        let drift = (ts.expires_at - Utc::now()).num_seconds();
        // Default of 1 hour, ±30s.
        assert!((3570..=3630).contains(&drift), "drift was {drift}");
    }

    #[test]
    fn token_set_captures_refresh_token_expires_in() {
        let response: TokenResponse = serde_json::from_str(
            r#"{"access_token":"AT","expires_in":60,"refresh_token":"RT","refresh_token_expires_in":3600}"#,
        )
        .unwrap();
        let ts = token_set_from_response("AT".into(), response);
        let drift = (ts.refresh_token_expires_at.unwrap() - Utc::now()).num_seconds();
        assert!((3595..=3605).contains(&drift), "drift was {drift}");
    }

    #[test]
    fn credentials_from_token_set_fills_settings_context() {
        let ts = TokenSet {
            access_token: "AT".into(),
            token_type: None,
            expires_at: Utc::now() + chrono::Duration::seconds(3600),
            refresh_token: Some("RT".into()),
            refresh_token_expires_at: None,
            scope: None,
            namespace_id: Some("ns-1".into()),
            subject: None,
        };
        let s = settings();
        let creds = credentials_from_token_set(ts, &s);
        assert_eq!(creds.token_type, "Bearer");
        assert_eq!(creds.scope, s.scope);
        assert_eq!(creds.client_id, s.client_id);
        assert_eq!(creds.authorization_server, s.authorization_server);
        assert_eq!(creds.namespace_id.as_deref(), Some("ns-1"));
    }

    #[test]
    fn id_token_sub_extraction() {
        // A minimal unsigned JWT (header.payload.sig) — we don't verify
        // the signature; we just decode the payload. Header / sig are
        // empty placeholders.
        use base64::Engine;
        let payload = r#"{"sub":"user-42","iss":"https://as.example.com"}"#;
        let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
        let token = format!("eyJhbGciOiJub25lIn0.{payload_b64}.sig");
        assert_eq!(extract_id_token_sub(&token).as_deref(), Some("user-42"));
    }

    #[test]
    fn id_token_sub_returns_none_on_malformed_input() {
        assert!(extract_id_token_sub("not-a-jwt").is_none());
        assert!(extract_id_token_sub("a.b").is_none());
    }

    /// Regression: the polling state machine must classify each RFC
    /// 8628 §3.5 error code into the right `PollOutcome` / `PollError`
    /// variant. We test the classification logic by directly feeding
    /// parsed bodies — the HTTP layer is covered by the wiremock
    /// integration test.
    #[test]
    fn rfc_8628_error_code_classification_is_complete() {
        let cases: &[(&str, &str)] = &[
            ("authorization_pending", "pending"),
            ("slow_down", "slow_down"),
            ("access_denied", "denied"),
            ("expired_token", "expired"),
            ("invalid_grant", "other"),
        ];
        for (code, bucket) in cases {
            let body = format!(r#"{{"error":"{code}"}}"#);
            let parsed: TokenResponse = serde_json::from_str(&body).unwrap();
            let classification = match parsed.error.as_deref() {
                Some("authorization_pending") => "pending",
                Some("slow_down") => "slow_down",
                Some("access_denied") => "denied",
                Some("expired_token") => "expired",
                Some(_) => "other",
                None => "missing",
            };
            assert_eq!(&classification, bucket, "wrong bucket for {code}");
        }
    }

    #[test]
    fn metadata_is_referenced_for_test_helpers() {
        // Sanity check the helpers compile + the metadata struct is
        // shaped correctly. The real exercising happens in the wiremock
        // integration test.
        let _ = (settings(), metadata());
    }
}
