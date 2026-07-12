//! Import the OpenAI Codex CLI's ChatGPT OAuth credential.
//!
//! Codex persists its OAuth token as JSON under a `tokens` key in
//! `$CODEX_HOME/auth.json` (default `~/.codex/auth.json`) and/or the macOS
//! login Keychain (generic password, service `Codex Auth`, account
//! `cli|<first-16-hex-of-sha256(codexHome)>`). The access token is a JWT; its
//! `exp` claim gives the expiry (the file carries no explicit one), falling
//! back to a one-hour TTL that the refresh path renews before it lapses.
//!
//! References: OpenAI Codex `login/src/auth/storage.rs`
//! (`AuthDotJson`, `CODEX_AUTH_KEYRING_SERVICE`, `get_auth_file`);
//! OpenClaw `src/agents/cli-credentials.ts`
//! (`computeCodexKeychainAccount`, `readCodexCliCredentials`).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::codex::jwt;
use crate::import::{ImportError, ImportSource, Imported, home_dir, keychain};
use crate::oauth::credential_store::OAuthToken;

/// macOS Keychain generic-password service Codex stores its token under.
const KEYCHAIN_SERVICE: &str = "Codex Auth";
/// Human name for diagnostics.
const CLI_NAME: &str = "Codex";
/// Credential filename inside `$CODEX_HOME`.
const AUTH_FILENAME: &str = "auth.json";
/// Fallback access-token lifetime when the JWT carries no `exp` claim — one
/// hour, matching the OpenClaw reference. The refresh path renews the token
/// before it lapses.
const FALLBACK_TTL_SECS: u64 = 3600;

/// The `{ "tokens": { … } }` envelope used by both the file and the Keychain
/// item.
#[derive(Deserialize)]
struct AuthFile {
    tokens: Option<Tokens>,
}

#[derive(Deserialize)]
struct Tokens {
    access_token: Option<String>,
    refresh_token: Option<String>,
}

/// Import the Codex credential from its default locations:
/// `$CODEX_HOME/auth.json` (default `~/.codex/auth.json`) first, then the macOS
/// Keychain (`Codex Auth`). Returns `Ok(None)` when neither carries one.
pub fn import() -> Result<Option<Imported>, ImportError> {
    let home = codex_home().ok_or(ImportError::NoHome)?;
    import_from_home(&home, || {
        // The Keychain account is derived from the realpath'd home (matching
        // the official Codex auth storage key), so a symlinked $CODEX_HOME
        // still resolves to the same item.
        let resolved = std::fs::canonicalize(&home).unwrap_or_else(|_| home.clone());
        let account = keychain_account(&resolved);
        keychain::read_generic_password(KEYCHAIN_SERVICE, Some(&account))
    })
}

fn import_from_home(
    home: &Path,
    read_keychain: impl FnOnce() -> Option<String>,
) -> Result<Option<Imported>, ImportError> {
    if let Some(imported) = from_file(&home.join(AUTH_FILENAME))? {
        return Ok(Some(imported));
    }
    if let Some(blob) = read_keychain()
        && let Ok(Some(token)) = parse_blob(&blob, "keychain")
    {
        return Ok(Some(Imported {
            token,
            source: ImportSource::Keychain(KEYCHAIN_SERVICE),
        }));
    }
    Ok(None)
}

/// Import from a specific `auth.json`. `Ok(None)` when the file is absent or
/// carries no `tokens` block.
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

/// Resolve `$CODEX_HOME` (default `~/.codex`).
fn codex_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    home_dir().map(|h| h.join(".codex"))
}

/// `cli|<first 16 hex chars of sha256(codex_home_path)>` — the account name
/// Codex uses for its Keychain item.
fn keychain_account(codex_home: &Path) -> String {
    let digest = Sha256::digest(codex_home.to_string_lossy().as_bytes());
    let hex = hex_encode(&digest);
    format!("cli|{}", &hex[..16])
}

/// Lowercase hex encoding — avoids pulling in a `hex` dependency for the one
/// call site.
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Parse a `{ "tokens": { … } }` blob. `Ok(None)` when the block is absent;
/// errors when it is present but missing an access token. Expiry comes from
/// the access token's JWT `exp` claim, else a one-hour fallback.
fn parse_blob(blob: &str, origin: &str) -> Result<Option<OAuthToken>, ImportError> {
    let parsed: AuthFile = serde_json::from_str(blob).map_err(|source| ImportError::Json {
        origin: origin.to_string(),
        source,
    })?;
    let Some(tokens) = parsed.tokens else {
        return Ok(None);
    };
    let access_token = tokens
        .access_token
        .filter(|s| !s.is_empty())
        .ok_or(ImportError::MissingAccessToken { cli: CLI_NAME })?;
    let expires_at = jwt::decode_codex_claims(&access_token)
        .ok()
        .and_then(|c| c.exp)
        .unwrap_or_else(|| now_secs() + FALLBACK_TTL_SECS);
    Ok(Some(OAuthToken {
        access_token,
        expires_at,
        refresh_token: tokens.refresh_token,
    }))
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
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header = URL_SAFE_NO_PAD.encode("{}");
        let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        let sig = URL_SAFE_NO_PAD.encode("sig");
        format!("{header}.{payload}.{sig}")
    }

    fn tmp_file(contents: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-import-codex-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn tmp_home() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-import-codex-home-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn auth_json(access: &str, refresh: &str) -> String {
        format!(r#"{{"tokens":{{"access_token":"{access}","refresh_token":"{refresh}"}}}}"#)
    }

    #[test]
    fn parses_tokens_and_reads_jwt_exp() {
        let jwt = make_jwt_exp(1_700_000_000);
        let json = format!(r#"{{"tokens":{{"access_token":"{jwt}","refresh_token":"r1"}}}}"#);
        let path = tmp_file(&json);
        let imported = from_file(&path).unwrap().unwrap();
        assert_eq!(imported.token.refresh_token.as_deref(), Some("r1"));
        assert_eq!(imported.token.expires_at, 1_700_000_000);
    }

    #[test]
    fn non_jwt_access_token_falls_back_to_ttl() {
        let path = tmp_file(r#"{"tokens":{"access_token":"not-a-jwt"}}"#);
        let imported = from_file(&path).unwrap().unwrap();
        // Fallback is now + 1h, so strictly in the future.
        assert!(imported.token.expires_at >= now_secs());
    }

    #[test]
    fn no_tokens_block_is_none() {
        let path = tmp_file(r#"{"OPENAI_API_KEY":"sk-..."}"#);
        assert!(from_file(&path).unwrap().is_none());
    }

    #[test]
    fn missing_access_token_errors() {
        let path = tmp_file(r#"{"tokens":{"refresh_token":"r1"}}"#);
        let err = from_file(&path).unwrap_err();
        assert!(matches!(err, ImportError::MissingAccessToken { .. }));
    }

    #[test]
    fn import_from_home_prefers_auth_json_without_touching_keychain() {
        let home = tmp_home();
        let file_jwt = make_jwt_exp(1_700_000_000);
        std::fs::write(
            home.join(AUTH_FILENAME),
            auth_json(&file_jwt, "file-refresh"),
        )
        .unwrap();

        let imported = import_from_home(&home, || {
            panic!("auth.json should win before Keychain is read")
        })
        .unwrap()
        .unwrap();

        assert_eq!(imported.token.access_token, file_jwt);
        assert_eq!(
            imported.token.refresh_token.as_deref(),
            Some("file-refresh")
        );
        assert!(
            matches!(imported.source, ImportSource::File(path) if path == home.join(AUTH_FILENAME))
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn import_from_home_uses_keychain_when_auth_json_absent() {
        let home = tmp_home();
        let keychain_jwt = make_jwt_exp(1_800_000_000);

        let imported =
            import_from_home(&home, || Some(auth_json(&keychain_jwt, "keychain-refresh")))
                .unwrap()
                .unwrap();

        assert_eq!(imported.token.access_token, keychain_jwt);
        assert_eq!(
            imported.token.refresh_token.as_deref(),
            Some("keychain-refresh")
        );
        assert!(matches!(
            imported.source,
            ImportSource::Keychain(KEYCHAIN_SERVICE)
        ));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn keychain_account_matches_known_sha256_vector() {
        // sha256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855;
        // first 16 hex chars → "e3b0c44298fc1c14".
        assert_eq!(keychain_account(Path::new("")), "cli|e3b0c44298fc1c14");
    }
}
