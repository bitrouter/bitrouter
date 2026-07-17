//! Import the Antigravity CLI (`agy`) Google OAuth session.
//!
//! The official `agy` CLI stores its Google OAuth credential in the OS keyring
//! (macOS Keychain / Linux Secret Service / Windows Credential Manager) under
//! `(service = "gemini", account = "antigravity")`, using Go's `go-keyring`
//! library. The stored value is `go-keyring-base64:` + base64 of the JSON:
//!
//! ```json
//! {"token":{"access_token":"ya29.…","token_type":"Bearer",
//!           "refresh_token":"1//…","expiry":"2026-07-09T16:05:35.08-04:00"},
//!  "auth_method":"consumer"}
//! ```
//!
//! We adopt the **`consumer`** session (Google personal-login) — the
//! `access_token` is a Google `ya29.…` Bearer accepted by `cloudcode-pa`, the
//! `refresh_token` is a Google `1//…` token, and `expiry` is RFC 3339. The
//! access token is opaque (not a JWT), so expiry comes from the `expiry` field.
//! The "use a Google Cloud project" path (`auth_method != "consumer"`) is a
//! different shape and is not imported here.
//!
//! Refresh needs `agy`'s confidential OAuth client (client id + `GOCSPX-…`
//! secret) — see [`crate::antigravity::agy_client`]; the secret is read from
//! the installed `agy` binary at refresh time rather than shipped here.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;

use crate::import::{ImportError, ImportSource, Imported};
use crate::oauth::credential_store::OAuthToken;

/// Keyring generic-password service `agy` stores its token under.
const KEYCHAIN_SERVICE: &str = "gemini";
/// Keyring account name within that service.
const KEYCHAIN_ACCOUNT: &str = "antigravity";
/// Prefix `go-keyring` prepends to base64-encoded secrets.
const GO_KEYRING_B64_PREFIX: &str = "go-keyring-base64:";
/// The only `auth_method` we import — the Google personal-login session.
const CONSUMER_AUTH_METHOD: &str = "consumer";

/// The `go-keyring`-stored JSON envelope.
#[derive(Deserialize)]
struct AgyCredential {
    token: Option<AgyToken>,
    #[serde(default)]
    auth_method: Option<String>,
}

#[derive(Deserialize)]
struct AgyToken {
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    /// RFC 3339 timestamp, e.g. `2026-07-09T16:05:35.088074-04:00`.
    #[serde(default)]
    expiry: Option<String>,
}

/// Import the Antigravity credential from the OS keyring. Returns `Ok(None)`
/// when no `agy` session is present (or it isn't a `consumer` session).
pub fn import() -> Result<Option<Imported>, ImportError> {
    let Some(raw) = read_keyring()? else {
        return Ok(None);
    };
    match parse_secret(&raw)? {
        Some(token) => Ok(Some(Imported {
            token,
            source: ImportSource::Keychain(KEYCHAIN_SERVICE),
        })),
        None => Ok(None),
    }
}

/// Read the raw `go-keyring` secret string from the OS keyring. `None` when the
/// entry is absent; errors on a keyring backend failure.
fn read_keyring() -> Result<Option<String>, ImportError> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| ImportError::Keyring(e.to_string()))?;
    match entry.get_password() {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(ImportError::Keyring(e.to_string())),
    }
}

/// Parse the raw keyring secret (`go-keyring-base64:` + base64 JSON, or plain
/// JSON) into an [`OAuthToken`]. `Ok(None)` when it isn't a `consumer` session
/// or carries no access token. Pure — unit-tested without a keyring.
fn parse_secret(raw: &str) -> Result<Option<OAuthToken>, ImportError> {
    let json = decode_go_keyring(raw)?;
    let cred: AgyCredential =
        serde_json::from_slice(&json).map_err(|source| ImportError::Json {
            origin: "agy keyring".to_string(),
            source,
        })?;
    // Only adopt the Google personal-login session; the GCP-project path is a
    // different credential shape.
    if cred.auth_method.as_deref() != Some(CONSUMER_AUTH_METHOD) {
        return Ok(None);
    }
    let Some(token) = cred.token else {
        return Ok(None);
    };
    let access_token = match token.access_token.filter(|s| !s.is_empty()) {
        Some(t) => t,
        None => return Ok(None),
    };
    let expires_at = token
        .expiry
        .as_deref()
        .and_then(rfc3339_to_epoch)
        .unwrap_or(0);
    Ok(Some(OAuthToken {
        access_token,
        expires_at,
        refresh_token: token.refresh_token.filter(|s| !s.is_empty()),
    }))
}

/// Strip the `go-keyring-base64:` prefix and base64-decode; a value without the
/// prefix is treated as raw JSON bytes (some backends store it un-encoded).
fn decode_go_keyring(raw: &str) -> Result<Vec<u8>, ImportError> {
    match raw.strip_prefix(GO_KEYRING_B64_PREFIX) {
        Some(b64) => STANDARD
            .decode(b64.trim())
            .map_err(|e| ImportError::Keyring(format!("agy keyring base64 decode: {e}"))),
        None => Ok(raw.as_bytes().to_vec()),
    }
}

/// Parse an RFC 3339 timestamp to Unix seconds. Handles a trailing `Z` or a
/// `±HH:MM` offset and an optional fractional-second part (ignored). `None` on
/// any malformed field. Dep-free (no `chrono`/`time`): the `agy` access token
/// is opaque, so this is the only expiry source.
fn rfc3339_to_epoch(s: &str) -> Option<u64> {
    // Split date and time on 'T'.
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;

    // Separate the time-of-day from the timezone designator: a trailing `Z`
    // (UTC), else a `±HH:MM` offset.
    let (time, tz): (&str, i64) = if let Some(t) = rest.strip_suffix('Z') {
        (t, 0)
    } else {
        let idx = rest.rfind(['+', '-'])?;
        let sign = if &rest[idx..=idx] == "-" { -1 } else { 1 };
        let (t, off) = rest.split_at(idx);
        let off = &off[1..]; // drop the sign
        let (oh, om) = off.split_once(':')?;
        let offset_secs = sign * (oh.parse::<i64>().ok()? * 3600 + om.parse::<i64>().ok()? * 60);
        (t, offset_secs)
    };

    // Time-of-day; drop any fractional seconds.
    let time = time.split('.').next()?;
    let mut tp = time.split(':');
    let hour: i64 = tp.next()?.parse().ok()?;
    let minute: i64 = tp.next()?.parse().ok()?;
    let second: i64 = tp.next()?.parse().ok()?;

    // Days since the Unix epoch (Howard Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;

    let epoch = days * 86400 + hour * 3600 + minute * 60 + second - tz;
    (epoch >= 0).then_some(epoch as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(auth_method: &str, expiry: &str) -> String {
        let json = format!(
            r#"{{"token":{{"access_token":"ya29.AAA","token_type":"Bearer","refresh_token":"1//RT","expiry":"{expiry}"}},"auth_method":"{auth_method}"}}"#
        );
        format!("{GO_KEYRING_B64_PREFIX}{}", STANDARD.encode(json))
    }

    #[test]
    fn parses_consumer_session_with_offset_expiry() {
        let raw = envelope("consumer", "2026-07-09T16:05:35.088074-04:00");
        let tok = parse_secret(&raw).unwrap().unwrap();
        assert_eq!(tok.access_token, "ya29.AAA");
        assert_eq!(tok.refresh_token.as_deref(), Some("1//RT"));
        // 2026-07-09T16:05:35-04:00 == 2026-07-09T20:05:35Z == 1783627535.
        assert_eq!(tok.expires_at, 1_783_627_535);
    }

    #[test]
    fn parses_z_suffix_expiry() {
        assert_eq!(
            rfc3339_to_epoch("2026-07-09T20:05:35Z"),
            Some(1_783_627_535)
        );
        assert_eq!(rfc3339_to_epoch("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn rejects_non_consumer_session() {
        let raw = envelope("gcp", "2026-07-09T20:05:35Z");
        assert!(parse_secret(&raw).unwrap().is_none());
    }

    #[test]
    fn plain_json_without_prefix_is_accepted() {
        let raw = r#"{"token":{"access_token":"ya29.X","refresh_token":"1//Y","expiry":"2026-07-09T20:05:35Z"},"auth_method":"consumer"}"#;
        let tok = parse_secret(raw).unwrap().unwrap();
        assert_eq!(tok.access_token, "ya29.X");
    }

    #[test]
    fn missing_access_token_is_none() {
        let raw = format!(
            "{GO_KEYRING_B64_PREFIX}{}",
            STANDARD.encode(r#"{"token":{"refresh_token":"1//Z"},"auth_method":"consumer"}"#)
        );
        assert!(parse_secret(&raw).unwrap().is_none());
    }

    #[test]
    fn malformed_expiry_falls_back_to_zero() {
        let raw = envelope("consumer", "not-a-date");
        let tok = parse_secret(&raw).unwrap().unwrap();
        assert_eq!(tok.expires_at, 0);
    }
}
