//! Import the Grok CLI's SuperGrok (xAI subscription) OAuth credential.
//!
//! The official Grok CLI (`grok`, from `x.ai/cli`) persists its OAuth session
//! as JSON in `$GROK_HOME/auth.json` (default `~/.grok/auth.json`). The file is
//! an object keyed by `"<issuer>::<client_id>"`; each value carries the access
//! token under `key` (a JWT), a `refresh_token`, an `expires_at`, and an
//! `auth_mode` (`"oidc"` for the subscription login vs an api-key session). We
//! adopt the OIDC entry — the one that carries both a `key` and a
//! `refresh_token` — and map it onto [`OAuthToken`]. Refresh then works exactly
//! as it does for a browser-obtained credential (public OIDC client against
//! `https://auth.x.ai/oauth2/token`).
//!
//! The access token is a JWT; its `exp` claim gives the expiry (matching the
//! sibling `expires_at` field), falling back to a one-hour TTL that the refresh
//! path renews before it lapses.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;

use crate::import::{ImportError, ImportSource, Imported, home_dir};
use crate::oauth::credential_store::OAuthToken;

/// Credential filename inside `$GROK_HOME`.
const AUTH_FILENAME: &str = "auth.json";
/// Fallback access-token lifetime when the JWT carries no `exp` claim — one
/// hour. The refresh path renews the token before it lapses.
const FALLBACK_TTL_SECS: u64 = 3600;

/// One `"<issuer>::<client_id>"` entry in the Grok CLI's `auth.json`. Only the
/// fields we consume are declared; everything else (email, team_id, …) is
/// ignored.
#[derive(Deserialize)]
struct AuthEntry {
    /// The access token — a JWT the xAI API accepts as a Bearer.
    #[serde(default)]
    key: Option<String>,
    /// The long-lived refresh token. Present on the OIDC (subscription) entry;
    /// absent on an api-key session. Entry selection keys on the presence of
    /// `key` + `refresh_token`, which excludes api-key sessions (they carry
    /// neither), so it needs no `auth_mode` check to stay robust.
    #[serde(default)]
    refresh_token: Option<String>,
}

/// Import the Grok credential from `$GROK_HOME/auth.json` (default
/// `~/.grok/auth.json`). Returns `Ok(None)` when the file is absent or carries
/// no subscription (OIDC) session.
pub fn import() -> Result<Option<Imported>, ImportError> {
    let home = grok_home().ok_or(ImportError::NoHome)?;
    from_file(&home.join(AUTH_FILENAME))
}

/// Import from a specific `auth.json`. `Ok(None)` when the file is absent or
/// carries no entry with both an access token and a refresh token.
pub fn from_file(path: &Path) -> Result<Option<Imported>, ImportError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ImportError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let blob = String::from_utf8_lossy(&bytes);
    match parse_blob(blob.as_ref(), &path.display().to_string())? {
        Some(token) => Ok(Some(Imported {
            token,
            source: ImportSource::File(path.to_path_buf()),
        })),
        None => Ok(None),
    }
}

/// Resolve `$GROK_HOME` (default `~/.grok`).
fn grok_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("GROK_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    home_dir().map(|h| h.join(".grok"))
}

/// Parse the `{ "<issuer>::<client_id>": { … } }` map. Selects the first entry
/// carrying both a non-empty `key` and `refresh_token` (the subscription OIDC
/// session; an api-key session has neither a JWT `key` nor a refresh token).
/// `Ok(None)` when the map has no such entry. Expiry comes from the access
/// token's JWT `exp` claim, else a one-hour fallback.
fn parse_blob(blob: &str, origin: &str) -> Result<Option<OAuthToken>, ImportError> {
    let entries: BTreeMap<String, AuthEntry> =
        serde_json::from_str(blob).map_err(|source| ImportError::Json {
            origin: origin.to_string(),
            source,
        })?;
    let Some((access_token, refresh_token)) = entries.into_values().find_map(|e| {
        let key = e.key.filter(|s| !s.is_empty())?;
        let refresh = e.refresh_token.filter(|s| !s.is_empty())?;
        Some((key, refresh))
    }) else {
        return Ok(None);
    };
    let expires_at =
        decode_jwt_exp(&access_token).unwrap_or_else(|| now_secs() + FALLBACK_TTL_SECS);
    Ok(Some(OAuthToken {
        access_token,
        expires_at,
        refresh_token: Some(refresh_token),
    }))
}

/// Decode the `exp` (Unix seconds) claim from a JWT access token's payload.
/// `None` when the string isn't a three-segment JWT, the payload isn't
/// base64url JSON, or it carries no numeric `exp`. Signature is not
/// verified — the token arrived from the OAuth server over TLS.
fn decode_jwt_exp(token: &str) -> Option<u64> {
    #[derive(Deserialize)]
    struct Exp {
        exp: Option<u64>,
    }
    let payload = token
        .split('.')
        .nth(1)
        .filter(|_| token.split('.').count() == 3)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let parsed: Exp = serde_json::from_slice(&bytes).ok()?;
    parsed.exp
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_jwt_exp(exp: u64) -> String {
        let header = URL_SAFE_NO_PAD.encode("{}");
        let payload =
            URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp},"iss":"https://auth.x.ai"}}"#));
        let sig = URL_SAFE_NO_PAD.encode("sig");
        format!("{header}.{payload}.{sig}")
    }

    fn tmp_file(contents: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("bitrouter-import-grok-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn auth_json(entry_key: &str, access: &str, refresh: &str, auth_mode: &str) -> String {
        format!(
            r#"{{"{entry_key}":{{"key":"{access}","refresh_token":"{refresh}","auth_mode":"{auth_mode}","expires_at":"2026-07-09T23:12:46Z"}}}}"#
        )
    }

    #[test]
    fn parses_oidc_entry_and_reads_jwt_exp() {
        let jwt = make_jwt_exp(1_783_638_765);
        let json = auth_json("https://auth.x.ai::b1a00492", &jwt, "rt-abc", "oidc");
        let path = tmp_file(&json);
        let imported = from_file(&path).unwrap().unwrap();
        assert_eq!(imported.token.access_token, jwt);
        assert_eq!(imported.token.refresh_token.as_deref(), Some("rt-abc"));
        assert_eq!(imported.token.expires_at, 1_783_638_765);
    }

    #[test]
    fn non_jwt_access_token_falls_back_to_ttl() {
        let json = auth_json("https://auth.x.ai::b1a00492", "not-a-jwt", "rt", "oidc");
        let path = tmp_file(&json);
        let imported = from_file(&path).unwrap().unwrap();
        assert!(imported.token.expires_at >= now_secs());
    }

    #[test]
    fn api_key_only_entry_is_none() {
        // An api-key session carries neither a JWT `key` nor a refresh token,
        // so there is no subscription credential to adopt.
        let path = tmp_file(r#"{"api":{"api_key":"xai-...","auth_mode":"key"}}"#);
        assert!(from_file(&path).unwrap().is_none());
    }

    #[test]
    fn entry_missing_refresh_token_is_skipped() {
        let jwt = make_jwt_exp(1_783_638_765);
        let json = format!(r#"{{"e":{{"key":"{jwt}"}}}}"#);
        let path = tmp_file(&json);
        assert!(from_file(&path).unwrap().is_none());
    }

    #[test]
    fn absent_file_is_none() {
        let missing = std::env::temp_dir().join("bitrouter-import-grok-absent/nope/auth.json");
        assert!(from_file(&missing).unwrap().is_none());
    }

    #[test]
    fn selects_oidc_entry_when_api_key_entry_also_present() {
        let jwt = make_jwt_exp(1_783_638_765);
        // Two entries: an api-key one (no refresh_token) and the OIDC one.
        let json = format!(
            r#"{{"api":{{"api_key":"xai-x"}},"https://auth.x.ai::c":{{"key":"{jwt}","refresh_token":"rt"}}}}"#
        );
        let path = tmp_file(&json);
        let imported = from_file(&path).unwrap().unwrap();
        assert_eq!(imported.token.access_token, jwt);
        assert_eq!(imported.token.refresh_token.as_deref(), Some("rt"));
    }
}
