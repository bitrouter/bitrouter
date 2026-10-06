//! Full hosted token envelopes shared by native refresh and caller-driven login.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::fmt;

/// RFC 6749 §5.1 token response shared with application-driven device polling.
/// The union of fields returned by the
/// device, refresh, and any other grant. Optional fields stay `None`
/// when omitted so a new grant doesn't break parsing.
#[derive(Clone, Deserialize)]
pub struct TokenResponse {
    /// Access token returned on success.
    pub access_token: Option<String>,
    #[serde(default)]
    /// OAuth token type when supplied.
    pub token_type: Option<String>,
    /// Seconds until `access_token` expires.
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    /// Replacement refresh token when supplied.
    pub refresh_token: Option<String>,
    /// Optional extension: seconds until the *refresh* token itself
    /// expires. Some AS implementations advertise this; others omit.
    #[serde(default)]
    pub refresh_token_expires_in: Option<u64>,
    #[serde(default)]
    /// Granted scope when supplied.
    pub scope: Option<String>,
    /// Non-standard bitrouter extension: the namespace the issued token
    /// is baked into. `Some` for every device-flow token; absent for a
    /// namespace-null credential. Persisted so the management client can
    /// resolve the implicit namespace for `/v1/namespaces/{nsid}/…` calls.
    #[serde(default)]
    pub namespace_id: Option<String>,
    /// OIDC id_token, used to extract the `sub` claim for `whoami`.
    #[serde(default)]
    pub id_token: Option<String>,
    /// RFC 6749 §5.2 error envelope (lives in the same body shape).
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub error_description: Option<String>,
}

impl fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenResponse")
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "<redacted>"),
            )
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("refresh_token_expires_in", &self.refresh_token_expires_in)
            .field("scope", &self.scope)
            .field("namespace_id", &self.namespace_id)
            .field("id_token", &self.id_token.as_ref().map(|_| "<redacted>"))
            .field("error", &self.error.as_ref().map(|_| "<redacted>"))
            .field(
                "error_description",
                &self.error_description.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Fresh token material returned by a successful token exchange (device
/// success, refresh, …). Mapped onto a [`super::credentials::Credentials`] by the caller,
/// who supplies the missing AS-context fields.
#[derive(Clone)]
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

impl fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &"<redacted>")
            .field("token_type", &self.token_type)
            .field("expires_at", &self.expires_at)
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("refresh_token_expires_at", &self.refresh_token_expires_at)
            .field("scope", &self.scope)
            .field("namespace_id", &self.namespace_id)
            .field("subject", &self.subject)
            .finish()
    }
}

/// Exchange the selected full hosted credential, retaining grant extensions.
/// Endpoint I/O is bounded and error diagnostics never contain payloads or URLs.
pub async fn refresh(
    client: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
    scope: Option<&str>,
) -> crate::error::Result<TokenSet> {
    crate::auth::oauth::require_secure_endpoint(token_endpoint).map_err(|_| {
        crate::error::ModelError::configuration("invalid or insecure hosted token endpoint")
    })?;
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    if let Some(scope) = scope.filter(|s| !s.is_empty()) {
        form.push(("scope", scope));
    }
    let response = client
        .post(token_endpoint)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&form)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|_| crate::error::ModelError::InvalidResponse {
            message: "hosted token exchange failed".into(),
        })?;
    if !response.status().is_success() {
        return Err(crate::error::ModelError::invalid_credential(
            "hosted token exchange was rejected",
        ));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|_| crate::error::ModelError::InvalidResponse {
            message: "hosted token response could not be read".into(),
        })?;
    let reply: TokenResponse =
        serde_json::from_slice(&bytes).map_err(|_| crate::error::ModelError::InvalidResponse {
            message: "hosted token response is malformed".into(),
        })?;
    if reply.error.is_some() {
        return Err(crate::error::ModelError::invalid_credential(
            "hosted token exchange returned an error",
        ));
    }
    let access_token =
        reply
            .access_token
            .clone()
            .ok_or_else(|| crate::error::ModelError::InvalidResponse {
                message: "hosted token response lacks an access token".into(),
            })?;
    Ok(token_set_from_response(access_token, reply))
}

/// Decode the successful envelope using the caller-checked access token.
pub fn token_set_from_response(access_token: String, parsed: TokenResponse) -> TokenSet {
    let now = Utc::now();
    let expires_at = parsed
        .expires_in
        .and_then(|s| i64::try_from(s).ok())
        .and_then(chrono::TimeDelta::try_seconds)
        .and_then(|d| now.checked_add_signed(d))
        // RFC 6749 §4.2.2 leaves expires_in optional. When omitted we
        // treat the token as valid for one hour — a sensible default
        // that is much shorter than typical AS-issued lifetimes, so we
        // err on the side of refreshing too often rather than too
        // rarely.
        .unwrap_or_else(|| now + chrono::Duration::hours(1));
    let refresh_token_expires_at = parsed
        .refresh_token_expires_in
        .and_then(|s| i64::try_from(s).ok())
        .and_then(chrono::TimeDelta::try_seconds)
        .and_then(|d| now.checked_add_signed(d));
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
/// it's only surfaced by `bro cloud whoami` as a hint of "which
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

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers};
    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
    #[test]
    fn token_set_from_response_computes_expires_at() -> Result<()> {
        let response: TokenResponse = serde_json::from_str(
            r#"{"access_token":"AT","token_type":"Bearer","expires_in":600,"refresh_token":"RT"}"#,
        )?;
        let ts = token_set_from_response("AT".into(), response);
        assert_eq!(ts.token_type.as_deref(), Some("Bearer"));
        assert_eq!(ts.refresh_token.as_deref(), Some("RT"));
        // ~ 10 minutes from now, allow generous skew.
        let drift = (ts.expires_at - Utc::now()).num_seconds();
        assert!((595..=605).contains(&drift), "drift was {drift}");
        Ok(())
    }

    #[test]
    fn token_set_defaults_expiry_when_server_omits_it() -> Result<()> {
        let response: TokenResponse = serde_json::from_str(r#"{"access_token":"AT"}"#)?;
        let ts = token_set_from_response("AT".into(), response);
        let drift = (ts.expires_at - Utc::now()).num_seconds();
        // Default of 1 hour, ±30s.
        assert!((3570..=3630).contains(&drift), "drift was {drift}");
        Ok(())
    }

    #[test]
    fn token_set_captures_refresh_token_expires_in() -> Result<()> {
        let response: TokenResponse = serde_json::from_str(
            r#"{"access_token":"AT","expires_in":60,"refresh_token":"RT","refresh_token_expires_in":3600}"#,
        )?;
        let ts = token_set_from_response("AT".into(), response);
        let refresh_expiry = ts
            .refresh_token_expires_at
            .ok_or("refresh expiry was not captured")?;
        let drift = (refresh_expiry - Utc::now()).num_seconds();
        assert!((3595..=3605).contains(&drift), "drift was {drift}");
        Ok(())
    }

    #[test]
    fn token_set_debug_redacts_token_values() {
        let token_set = TokenSet {
            access_token: "access-token-secret".to_owned(),
            token_type: Some("Bearer".to_owned()),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            refresh_token: Some("refresh-token-secret".to_owned()),
            refresh_token_expires_at: None,
            scope: None,
            namespace_id: None,
            subject: None,
        };

        let rendered = format!("{token_set:?}");
        assert!(!rendered.contains("access-token-secret"));
        assert!(!rendered.contains("refresh-token-secret"));
        assert!(rendered.contains("<redacted>"));
    }

    #[tokio::test]
    async fn malformed_refresh_response_does_not_echo_tokens() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"access_token":"refresh-secret""#),
            )
            .mount(&server)
            .await;

        let error = match refresh(
            &reqwest::Client::new(),
            &server.uri(),
            "cid",
            "refresh-token",
            None,
        )
        .await
        {
            Ok(_) => return Err("malformed refresh response unexpectedly succeeded".into()),
            Err(error) => error,
        };

        assert!(!format!("{error:#}").contains("refresh-secret"));
        Ok(())
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

    #[test]
    fn oversized_token_lifetimes_never_wrap_or_panic() -> Result<()> {
        let response: TokenResponse = serde_json::from_value(
            serde_json::json!({"access_token":"AT","expires_in":u64::MAX,"refresh_token_expires_in":u64::MAX}),
        )?;
        let token = token_set_from_response("AT".into(), response);
        assert!(token.expires_at > Utc::now());
        assert!(token.refresh_token_expires_at.is_none());
        Ok(())
    }
}
