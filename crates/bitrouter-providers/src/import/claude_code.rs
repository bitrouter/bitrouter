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

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

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

/// Where a live Claude Code credential was read from — so a refresh write-back
/// can target the *same* place rather than diverging into a second store.
#[derive(Debug, Clone)]
pub enum ClaudeCodeSource {
    /// The macOS login Keychain generic-password item.
    Keychain {
        /// The generic-password service (`Claude Code-credentials`).
        service: &'static str,
        /// The account the item is keyed under — required for an in-place
        /// upsert so we update Claude Code's item, not a duplicate.
        account: String,
    },
    /// A JSON file on disk (`~/.claude/.credentials.json`).
    File(PathBuf),
}

impl std::fmt::Display for ClaudeCodeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaudeCodeSource::Keychain { service, .. } => {
                write!(f, "macOS Keychain ({service})")
            }
            ClaudeCodeSource::File(path) => write!(f, "{}", path.display()),
        }
    }
}

/// A live Claude Code OAuth credential plus the source it was read from.
#[derive(Debug, Clone)]
pub struct LiveCredential {
    /// The current OAuth token (`expires_at` in seconds).
    pub token: OAuthToken,
    /// Where it came from — the write-back target.
    pub source: ClaudeCodeSource,
}

/// A live, read-write view of Claude Code's own Claude Pro/Max OAuth credential.
///
/// This is the single source of truth shared with the `claude` CLI: bitrouter
/// *reads* the current token and, when it must refresh, *writes the rotated
/// token back to the same place Claude Code stores it* — so the two never
/// diverge and Anthropic's refresh-token rotation (RFC 6749 §6) can't
/// family-revoke the user out of Claude Code. (Contrast tools that copy the
/// token into a private store and refresh independently, e.g. OpenClaw issue
/// #71026; the correct write-back pattern is griffinmartin/opencode-claude-auth.)
///
/// Locations mirror Claude Code: macOS Keychain generic password
/// `Claude Code-credentials`, else `~/.claude/.credentials.json`. Envelope:
/// `{ "claudeAiOauth": { accessToken, refreshToken, expiresAt(ms), … } }`,
/// with any other fields (`scopes`, `subscriptionType`, …) preserved verbatim
/// across a write-back.
#[derive(Debug, Clone)]
pub struct ClaudeCodeStore {
    /// `Some` on macOS (try the Keychain first); `None` elsewhere or in tests.
    keychain_service: Option<&'static str>,
    /// The on-disk credential file (used when the Keychain has no item).
    file_path: PathBuf,
}

impl ClaudeCodeStore {
    /// The store at Claude Code's default system locations: the macOS Keychain
    /// (`Claude Code-credentials`) plus `~/.claude/.credentials.json`. `None`
    /// when no home directory can be resolved.
    pub fn system() -> Option<Self> {
        let file_path = home_dir()?.join(".claude").join(".credentials.json");
        #[cfg(target_os = "macos")]
        let keychain_service = Some(KEYCHAIN_SERVICE);
        #[cfg(not(target_os = "macos"))]
        let keychain_service = None;
        Some(Self {
            keychain_service,
            file_path,
        })
    }

    /// A file-only store at `path`, never touching the Keychain. Used on
    /// non-macOS hosts and in tests so a real Keychain is never read/written.
    pub fn file_only(path: impl Into<PathBuf>) -> Self {
        Self {
            keychain_service: None,
            file_path: path.into(),
        }
    }

    /// Read the current credential, Keychain first (macOS) then the file.
    /// `Ok(None)` when neither source carries a Claude Code OAuth credential.
    pub fn read(&self) -> Result<Option<LiveCredential>, ImportError> {
        if let Some(service) = self.keychain_service
            && let Some(blob) = keychain::read_generic_password(service, None)
            && let Some(token) = parse_blob(&blob, "keychain")?
        {
            // The account is needed to write the rotated token back in place.
            // Fall back to an empty account rather than failing the read — a
            // write-back would then create the item under that account.
            let account = keychain::find_account(service).unwrap_or_default();
            return Ok(Some(LiveCredential {
                token,
                source: ClaudeCodeSource::Keychain { service, account },
            }));
        }
        match from_file(&self.file_path)? {
            Some(imported) => Ok(Some(LiveCredential {
                token: imported.token,
                source: ClaudeCodeSource::File(self.file_path.clone()),
            })),
            None => Ok(None),
        }
    }

    /// Persist a refreshed `token` back to `source`, preserving every other
    /// field Claude Code wrote. File writes are atomic and `0600`; Keychain
    /// writes upsert the existing `(service, account)` item.
    pub fn write_back(
        &self,
        token: &OAuthToken,
        source: &ClaudeCodeSource,
    ) -> Result<(), ImportError> {
        match source {
            ClaudeCodeSource::File(path) => {
                let existing = std::fs::read(path).ok();
                let bytes = updated_envelope_bytes(existing.as_deref(), token)?;
                write_file_atomic_0600(path, &bytes)
            }
            ClaudeCodeSource::Keychain { service, account } => {
                let existing = keychain::read_generic_password(service, Some(account))
                    .or_else(|| keychain::read_generic_password(service, None));
                let bytes = updated_envelope_bytes(existing.as_deref().map(str::as_bytes), token)?;
                // `to_vec_pretty` always emits valid UTF-8, so the lossy
                // conversion is exact here.
                let value = String::from_utf8_lossy(&bytes).into_owned();
                if keychain::write_generic_password(service, account, &value) {
                    Ok(())
                } else {
                    Err(ImportError::Io {
                        path: PathBuf::from(format!("keychain:{service}")),
                        source: std::io::Error::other("security add-generic-password failed"),
                    })
                }
            }
        }
    }
}

/// Merge a refreshed `token` into Claude Code's credential envelope, preserving
/// every other field. `existing` is the current raw blob (file bytes or
/// Keychain value); when absent or unparseable a fresh `{ "claudeAiOauth": {} }`
/// envelope is built. `expiresAt` is written back in **milliseconds**.
fn updated_envelope_bytes(
    existing: Option<&[u8]>,
    token: &OAuthToken,
) -> Result<Vec<u8>, ImportError> {
    let mut root: Value = match existing {
        Some(b) if !b.is_empty() => serde_json::from_slice(b).unwrap_or_else(|_| json!({})),
        _ => json!({}),
    };
    if !root.is_object() {
        root = json!({});
    }
    let obj = root.as_object_mut().expect("root is an object");
    let entry = obj.entry("claudeAiOauth").or_insert_with(|| json!({}));
    if !entry.is_object() {
        *entry = json!({});
    }
    let oauth = entry.as_object_mut().expect("claudeAiOauth is an object");
    oauth.insert("accessToken".to_string(), json!(token.access_token));
    if let Some(rt) = &token.refresh_token {
        oauth.insert("refreshToken".to_string(), json!(rt));
    }
    // Seconds → milliseconds. A non-expiring (`0`) token leaves any existing
    // `expiresAt` untouched rather than writing a misleading `0`.
    if token.expires_at > 0 {
        oauth.insert("expiresAt".to_string(), json!(token.expires_at * 1000));
    }
    serde_json::to_vec_pretty(&root).map_err(|source| ImportError::Json {
        origin: "claude code credentials write-back".to_string(),
        source,
    })
}

/// Write `bytes` to `path` atomically with `0600` permissions (Unix), mirroring
/// the credential store's flush so the token never sits on a world-readable
/// file even for the width of the write.
fn write_file_atomic_0600(path: &Path, bytes: &[u8]) -> Result<(), ImportError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ImportError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let _ = std::fs::remove_file(&tmp);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|source| ImportError::Io {
                path: tmp.clone(),
                source,
            })?;
        file.write_all(bytes).map_err(|source| ImportError::Io {
            path: tmp.clone(),
            source,
        })?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&tmp, bytes).map_err(|source| ImportError::Io {
            path: tmp.clone(),
            source,
        })?;
    }
    std::fs::rename(&tmp, path).map_err(|source| ImportError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn store_read_file_only_returns_token_and_file_source() {
        let path = tmp_file(
            r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r","expiresAt":1700000000000}}"#,
        );
        let store = ClaudeCodeStore::file_only(&path);
        let live = store.read().unwrap().unwrap();
        assert_eq!(live.token.access_token, "a");
        assert_eq!(live.token.refresh_token.as_deref(), Some("r"));
        assert_eq!(live.token.expires_at, 1_700_000_000);
        match &live.source {
            ClaudeCodeSource::File(p) => assert_eq!(p, &path),
            other => panic!("expected File source, got {other:?}"),
        }
    }

    #[test]
    fn store_read_file_only_absent_is_none() {
        let path = std::env::temp_dir().join("bitrouter-ccstore-absent/never.json");
        let store = ClaudeCodeStore::file_only(&path);
        assert!(store.read().unwrap().is_none());
    }

    #[test]
    fn write_back_preserves_unknown_keys_and_updates_token() {
        let path = tmp_file(
            r#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"oldr","expiresAt":1700000000000,"scopes":["user:inference"],"subscriptionType":"max"},"otherTop":42}"#,
        );
        let store = ClaudeCodeStore::file_only(&path);
        store
            .write_back(
                &OAuthToken {
                    access_token: "new".into(),
                    expires_at: 1_700_000_500,
                    refresh_token: Some("newr".into()),
                },
                &ClaudeCodeSource::File(path.clone()),
            )
            .unwrap();
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let o = &raw["claudeAiOauth"];
        assert_eq!(o["accessToken"], "new");
        assert_eq!(o["refreshToken"], "newr");
        // seconds → milliseconds on write-back.
        assert_eq!(o["expiresAt"], 1_700_000_500_000u64);
        // Unknown fields Claude Code wrote must survive the round-trip.
        assert_eq!(o["subscriptionType"], "max");
        assert_eq!(o["scopes"][0], "user:inference");
        assert_eq!(raw["otherTop"], 42);
        // And the rotated token reads back through the store.
        let live = store.read().unwrap().unwrap();
        assert_eq!(live.token.access_token, "new");
        assert_eq!(live.token.refresh_token.as_deref(), Some("newr"));
    }

    #[cfg(unix)]
    #[test]
    fn write_back_creates_fresh_file_0600() {
        use std::os::unix::fs::PermissionsExt;
        // Start from a unique path that does NOT yet exist (macOS Keychain-only
        // machines have no file until something writes one).
        let path = tmp_file("{}");
        std::fs::remove_file(&path).unwrap();
        let store = ClaudeCodeStore::file_only(&path);
        store
            .write_back(
                &OAuthToken {
                    access_token: "a".into(),
                    expires_at: 1_700_000_000,
                    refresh_token: Some("r".into()),
                },
                &ClaudeCodeSource::File(path.clone()),
            )
            .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
        // A freshly-created file is still a valid, readable credential.
        let live = store.read().unwrap().unwrap();
        assert_eq!(live.token.access_token, "a");
    }
}
