//! Import Claude Code's stored Claude Pro/Max OAuth credential.
//!
//! Claude Code persists its subscription OAuth token as JSON under a
//! `claudeAiOauth` key — in the macOS login Keychain (generic password,
//! service `Claude Code-credentials`) and/or `~/.claude/.credentials.json`.
//! We read the Keychain first (macOS), then the file. `expiresAt` is in
//! milliseconds since the epoch; the credential store tracks seconds.
//!
//! Reference: OpenClaw `src/agents/cli-credentials.ts`
//! (<https://github.com/openclaw/openclaw>).

use std::path::Path;

use serde::Deserialize;

use crate::import::{ImportError, ImportSource, Imported, home_dir, keychain};
use crate::oauth::credential_store::OAuthToken;

/// macOS Keychain generic-password service Claude Code stores its token under.
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
/// Human name for diagnostics.
const CLI_NAME: &str = "Claude Code";

/// The `{ "claudeAiOauth": { … } }` envelope used by both the file and the
/// Keychain item.
#[derive(Deserialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<ClaudeAiOauth>,
}

#[derive(Deserialize)]
struct ClaudeAiOauth {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "refreshToken")]
    refresh_token: Option<String>,
    /// Milliseconds since the epoch.
    #[serde(rename = "expiresAt")]
    expires_at: Option<u64>,
}

/// Import the Claude Code credential from its default locations: the macOS
/// Keychain (`Claude Code-credentials`) first, then
/// `~/.claude/.credentials.json`. Returns `Ok(None)` when neither carries a
/// Claude Code OAuth credential.
pub fn import() -> Result<Option<Imported>, ImportError> {
    // Keychain first (macOS). A malformed or absent item falls through to the
    // file, which is the authoritative error surface.
    if let Some(blob) = keychain::read_generic_password(KEYCHAIN_SERVICE, None)
        && let Ok(Some(token)) = parse_blob(&blob, "keychain")
    {
        return Ok(Some(Imported {
            token,
            source: ImportSource::Keychain(KEYCHAIN_SERVICE),
        }));
    }
    let path = home_dir()
        .ok_or(ImportError::NoHome)?
        .join(".claude")
        .join(".credentials.json");
    from_file(&path)
}

/// Import from a specific `.credentials.json`. `Ok(None)` when the file is
/// absent or carries no `claudeAiOauth` block.
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

/// Parse a `{ "claudeAiOauth": { … } }` blob. `Ok(None)` when the block is
/// absent; errors when it is present but missing an access token.
fn parse_blob(blob: &str, origin: &str) -> Result<Option<OAuthToken>, ImportError> {
    let parsed: CredentialsFile =
        serde_json::from_str(blob).map_err(|source| ImportError::Json {
            origin: origin.to_string(),
            source,
        })?;
    let Some(oauth) = parsed.claude_ai_oauth else {
        return Ok(None);
    };
    let access_token = oauth
        .access_token
        .filter(|s| !s.is_empty())
        .ok_or(ImportError::MissingAccessToken { cli: CLI_NAME })?;
    // `expiresAt` is milliseconds since the epoch; the store tracks seconds.
    let expires_at = oauth.expires_at.map(|ms| ms / 1000).unwrap_or(0);
    Ok(Some(OAuthToken {
        access_token,
        expires_at,
        refresh_token: oauth.refresh_token,
    }))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn tmp_file(contents: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-import-claude-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".credentials.json");
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn parses_oauth_and_converts_expiry_ms_to_secs() {
        let path = tmp_file(
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat-1","refreshToken":"r1","expiresAt":1700000000000}}"#,
        );
        let imported = from_file(&path).unwrap().unwrap();
        assert_eq!(imported.token.access_token, "sk-ant-oat-1");
        assert_eq!(imported.token.refresh_token.as_deref(), Some("r1"));
        // 1_700_000_000_000 ms / 1000 = 1_700_000_000 s.
        assert_eq!(imported.token.expires_at, 1_700_000_000);
        assert!(matches!(imported.source, ImportSource::File(_)));
    }

    #[test]
    fn missing_file_is_none() {
        let path = std::env::temp_dir().join("bitrouter-import-claude-absent/never.json");
        assert!(from_file(&path).unwrap().is_none());
    }

    #[test]
    fn no_oauth_block_is_none() {
        let path = tmp_file(r#"{"somethingElse":true}"#);
        assert!(from_file(&path).unwrap().is_none());
    }

    #[test]
    fn missing_access_token_errors() {
        let path = tmp_file(r#"{"claudeAiOauth":{"refreshToken":"r1"}}"#);
        let err = from_file(&path).unwrap_err();
        assert!(matches!(err, ImportError::MissingAccessToken { .. }));
    }
}
