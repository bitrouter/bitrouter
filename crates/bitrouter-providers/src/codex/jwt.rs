//! Minimal JWT payload decoder.
//!
//! OpenAI Codex's OAuth access token is a JWT whose payload claims carry the
//! `chatgpt_account_id` the Codex backend expects as a request header. We
//! only need to read the payload — signature verification is the upstream's
//! job, not ours (we just received this token from the OAuth server over
//! TLS, we're trusting that channel).
//!
//! JWT shape: three base64url-no-padding segments separated by dots —
//! `header.payload.signature`. Decode the middle one as UTF-8 JSON.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;

/// Decoded subset of the Codex JWT payload — only the claims we need.
///
/// OpenAI nests Codex-specific claims under
/// `"https://api.openai.com/auth"` because the JWT may be consumed by
/// several services; the namespace prevents collisions.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CodexClaims {
    /// `exp` claim — Unix seconds until the access token expires. Optional
    /// here because the OAuth `expires_in` is the authoritative value;
    /// `exp` is just a useful sanity check.
    #[serde(default)]
    pub exp: Option<u64>,
    /// Email associated with the ChatGPT account, when present in the
    /// `https://api.openai.com/profile` claim block.
    #[serde(default)]
    pub email: Option<String>,
    /// The `chatgpt_account_id` Codex's backend uses to scope quotas + the
    /// `chatgpt-account-id` request header.
    #[serde(default)]
    pub chatgpt_account_id: Option<String>,
}

#[derive(Deserialize)]
struct RawPayload {
    #[serde(default)]
    exp: Option<u64>,
    #[serde(default, rename = "https://api.openai.com/auth")]
    auth: Option<AuthClaim>,
    #[serde(default, rename = "https://api.openai.com/profile")]
    profile: Option<ProfileClaim>,
}

#[derive(Deserialize)]
struct AuthClaim {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
}

#[derive(Deserialize)]
struct ProfileClaim {
    #[serde(default)]
    email: Option<String>,
}

/// Errors raised by the JWT payload decoder.
#[derive(Debug, thiserror::Error)]
pub enum JwtError {
    /// The string didn't have three `.`-separated segments.
    #[error("malformed JWT — expected three segments, got {0}")]
    Segments(usize),
    /// The payload segment wasn't valid base64url.
    #[error("malformed JWT payload base64: {0}")]
    Base64(#[from] base64::DecodeError),
    /// The decoded payload wasn't valid UTF-8.
    #[error("malformed JWT payload utf8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    /// The decoded payload wasn't valid JSON.
    #[error("malformed JWT payload JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Pull the claims we care about out of a Codex JWT access token. Errors
/// surface every step where the input could be malformed so the caller can
/// log a precise diagnostic when an upstream change breaks decoding.
pub fn decode_codex_claims(jwt: &str) -> Result<CodexClaims, JwtError> {
    let segments: Vec<&str> = jwt.split('.').collect();
    if segments.len() != 3 {
        return Err(JwtError::Segments(segments.len()));
    }
    let payload_bytes = URL_SAFE_NO_PAD.decode(segments[1])?;
    let payload_str = String::from_utf8(payload_bytes)?;
    let raw: RawPayload = serde_json::from_str(&payload_str)?;
    Ok(CodexClaims {
        exp: raw.exp,
        email: raw.profile.and_then(|p| p.email),
        chatgpt_account_id: raw.auth.and_then(|a| a.chatgpt_account_id),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal three-segment JWT whose payload is the given JSON
    /// string. Header and signature are placeholders (`{}` and `sig`) —
    /// we don't verify either.
    fn make_jwt(payload_json: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode("{}");
        let payload = URL_SAFE_NO_PAD.encode(payload_json);
        let sig = URL_SAFE_NO_PAD.encode("sig");
        format!("{header}.{payload}.{sig}")
    }

    #[test]
    fn extracts_chatgpt_account_id_from_namespaced_claim() {
        let jwt = make_jwt(
            r#"{
              "exp": 1700000000,
              "https://api.openai.com/auth": {"chatgpt_account_id": "acct-abc-123"},
              "https://api.openai.com/profile": {"email": "user@example.com"}
            }"#,
        );
        let claims = decode_codex_claims(&jwt).unwrap();
        assert_eq!(claims.chatgpt_account_id.as_deref(), Some("acct-abc-123"));
        assert_eq!(claims.email.as_deref(), Some("user@example.com"));
        assert_eq!(claims.exp, Some(1700000000));
    }

    #[test]
    fn returns_empty_claims_when_namespaced_blocks_absent() {
        let jwt = make_jwt(r#"{"exp": 1700000000}"#);
        let claims = decode_codex_claims(&jwt).unwrap();
        assert!(claims.chatgpt_account_id.is_none());
        assert!(claims.email.is_none());
    }

    #[test]
    fn rejects_non_three_segment_input() {
        let err = decode_codex_claims("only.two").unwrap_err();
        assert!(matches!(err, JwtError::Segments(2)));
        let err = decode_codex_claims("one").unwrap_err();
        assert!(matches!(err, JwtError::Segments(1)));
    }

    #[test]
    fn rejects_non_base64_payload() {
        let err = decode_codex_claims("aaa.not!base64!.bbb").unwrap_err();
        assert!(matches!(err, JwtError::Base64(_)));
    }

    #[test]
    fn rejects_non_json_payload() {
        // Valid base64url of `not-json` — passes base64 but fails JSON.
        let payload = URL_SAFE_NO_PAD.encode(b"not-json");
        let jwt = format!("aaa.{payload}.bbb");
        let err = decode_codex_claims(&jwt).unwrap_err();
        assert!(matches!(err, JwtError::Json(_)));
    }
}
