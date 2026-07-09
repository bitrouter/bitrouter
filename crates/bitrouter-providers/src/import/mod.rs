//! Importers that adopt another coding CLI's OAuth session into bitrouter's
//! credential store, so `bitrouter providers login claude-code` /
//! `bitrouter providers login openai-codex` can reuse an existing Claude Code /
//! Codex login instead of a fresh browser sign-in.
//!
//! Each importer reads the vendor CLI's own on-disk / Keychain credential,
//! maps it onto [`crate::oauth::credential_store::OAuthToken`], and hands it
//! back for the caller to persist under the matching bitrouter provider id
//! (`claude-code` for Claude Code, `openai-codex` for Codex). Refresh then works
//! exactly as it does for a credential obtained through the browser flow.
//!
//! Credential locations mirror the vendor CLIs and the OpenClaw reference
//! (`src/agents/cli-credentials.ts`,
//! <https://github.com/openclaw/openclaw>):
//! - Claude Code — macOS Keychain `Claude Code-credentials`, else
//!   `~/.claude/.credentials.json`. See [`claude_code`].
//! - Codex — macOS Keychain `Codex Auth`, else `$CODEX_HOME/auth.json`
//!   (default `~/.codex/auth.json`). See [`codex`].

pub mod claude_code;
pub mod codex;
mod keychain;

use std::path::PathBuf;

use crate::oauth::credential_store::OAuthToken;

/// Where an imported credential was read from. Surfaced to the CLI so it can
/// tell the user which source was adopted.
#[derive(Debug, Clone)]
pub enum ImportSource {
    /// The macOS login Keychain; the field is the generic-password service.
    Keychain(&'static str),
    /// A JSON file on disk at the given path.
    File(PathBuf),
}

impl std::fmt::Display for ImportSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportSource::Keychain(service) => write!(f, "macOS Keychain ({service})"),
            ImportSource::File(path) => write!(f, "{}", path.display()),
        }
    }
}

/// A credential imported from a vendor CLI plus where it came from.
#[derive(Debug, Clone)]
pub struct Imported {
    /// The adopted OAuth token, ready to persist in the credential store.
    pub token: OAuthToken,
    /// The source the token was read from.
    pub source: ImportSource,
}

/// Errors raised while importing a vendor CLI credential.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// No home directory could be resolved (neither `HOME` nor `USERPROFILE`).
    #[error("could not resolve the home directory (set HOME)")]
    NoHome,
    /// Reading the credential file failed for a reason other than "absent".
    #[error("reading {path}: {source}")]
    Io {
        /// The path that failed to read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The credential blob wasn't valid JSON.
    #[error("parsing {origin} credentials: {source}")]
    Json {
        /// A human label for what was being parsed (a path, or "keychain").
        origin: String,
        /// The underlying JSON error.
        #[source]
        source: serde_json::Error,
    },
    /// The credential blob parsed but carried no access token.
    #[error("{cli} credential is missing an access token")]
    MissingAccessToken {
        /// The vendor CLI name, for the message.
        cli: &'static str,
    },
}

/// Resolve the user's home directory from `HOME` (Unix) or `USERPROFILE`
/// (Windows). `None` when neither is set.
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()))
        .map(PathBuf::from)
}
