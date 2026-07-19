//! The Antigravity OAuth client — pinned public id, secret read from the local
//! `agy` binary at runtime.
//!
//! `agy` is a Google **confidential** (installed-app) OAuth client: refreshing
//! its `1//…` refresh token requires the client id **and** a `GOCSPX-…` secret.
//! Google embeds that secret in every copy of the binary (installed-app secrets
//! are not treated as confidential by design — see Google's OAuth docs), but we
//! deliberately do **not** vendor it into bitrouter. Instead we read it out of
//! the `agy` executable already on the user's machine — the same binary that
//! minted the token we're refreshing. So refresh works only when `agy` is
//! installed, which is exactly the precondition for having an `agy` session at
//! all.
//!
//! The client id is a **public** identifier (safe to embed); only the secret is
//! extracted. `agy` ships more than one OAuth client, so [`extract_secrets`]
//! returns every `GOCSPX-…` candidate and the caller tries each until the
//! refresh succeeds.

use std::path::PathBuf;

/// Antigravity's public OAuth client id (confirmed in the `agy` binary and the
/// stored `~/.gemini` session). Public identifier — safe to embed.
pub const CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";

/// Google's OAuth token endpoint — the confidential refresh grant target.
pub const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";

/// Marker every Google OAuth client secret starts with.
const SECRET_PREFIX: &[u8] = b"GOCSPX-";
/// Body length after `GOCSPX-`. Google OAuth client secrets are a fixed
/// `GOCSPX-` + 28 base64url-ish chars; taking exactly this many cleanly splits
/// secrets that sit alnum-adjacent to each other or to trailing data (both seen
/// in the `agy` binary), which a greedy scan cannot disambiguate.
const SECRET_BODY_LEN: usize = 28;

/// Errors resolving the `agy` OAuth client.
#[derive(Debug, thiserror::Error)]
pub enum AgyClientError {
    /// The `agy` binary couldn't be located.
    #[error(
        "cannot find the `agy` binary to refresh the Antigravity token — install it \
         (https://antigravity.google/docs/cli/install) or set AGY_BIN"
    )]
    NotFound,
    /// Reading the binary failed.
    #[error("reading the `agy` binary at {path}: {source}")]
    Io {
        /// Path that failed to read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// No `GOCSPX-…` secret was found in the binary (unexpected — a format
    /// change on Google's side).
    #[error(
        "no OAuth client secret found in the `agy` binary at {0} — its format may have changed"
    )]
    NoSecret(PathBuf),
}

/// Locate the installed `agy` binary. Resolution order: `$AGY_BIN`, then `PATH`
/// (`which`-style), then the platform default install location.
pub fn locate_agy() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("AGY_BIN").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(p) = which_on_path("agy") {
        return Some(p);
    }
    default_install_path().filter(|p| p.is_file())
}

/// Read the `agy` binary and return every distinct `GOCSPX-…` secret candidate,
/// in first-seen order. The caller tries each against the refresh endpoint.
pub fn extract_secrets() -> Result<Vec<String>, AgyClientError> {
    let path = locate_agy().ok_or(AgyClientError::NotFound)?;
    let bytes = std::fs::read(&path).map_err(|source| AgyClientError::Io {
        path: path.clone(),
        source,
    })?;
    let secrets = scan_secrets(&bytes);
    if secrets.is_empty() {
        return Err(AgyClientError::NoSecret(path));
    }
    Ok(secrets)
}

/// Scan a byte buffer for `GOCSPX-…` secrets. Pure — unit-tested on synthetic
/// buffers. Each candidate is `GOCSPX-` plus exactly [`SECRET_BODY_LEN`]
/// base64url-ish bytes (`[A-Za-z0-9_-]`); an occurrence with fewer valid body
/// bytes is skipped. Duplicates are dropped, first-seen order preserved.
fn scan_secrets(bytes: &[u8]) -> Vec<String> {
    let prefix = SECRET_PREFIX.len();
    let total = prefix + SECRET_BODY_LEN;
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i + prefix <= bytes.len() {
        if &bytes[i..i + prefix] != SECRET_PREFIX {
            i += 1;
            continue;
        }
        if i + total <= bytes.len()
            && bytes[i + prefix..i + total]
                .iter()
                .all(|&b| is_secret_byte(b))
            && let Ok(s) = std::str::from_utf8(&bytes[i..i + total])
        {
            let s = s.to_string();
            if !out.contains(&s) {
                out.push(s);
            }
        }
        // Advance past this prefix; overlapping `GOCSPX-` starts can't occur
        // inside a valid body, so skipping one byte is sufficient and safe.
        i += 1;
    }
    out
}

fn is_secret_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// Minimal `which`: scan `PATH` for an executable named `name`.
fn which_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

/// Platform default install path for `agy`.
fn default_install_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(|d| PathBuf::from(d).join("agy").join("bin").join("agy.exe"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("bin").join("agy"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic secrets built at runtime — a `GOCSPX-` + 28-char shape matching
    // the real format, WITHOUT a literal Google secret in source (bitrouter
    // ships no Google secret, not even in tests; push-protection enforces it).
    // The prefix is assembled from a byte slice so the source never contains the
    // `GOCSPX-<body>` token a secret scanner keys on.
    fn fake_secret(body_char: char) -> String {
        let prefix = std::str::from_utf8(SECRET_PREFIX).unwrap();
        let body: String = std::iter::repeat_n(body_char, SECRET_BODY_LEN).collect();
        format!("{prefix}{body}")
    }

    #[test]
    fn extracts_two_concatenated_secrets_and_stops_at_trailing_junk() {
        // Mirrors the real `strings` output: two secrets back-to-back, then a
        // non-secret word glued on with no separator.
        let s1 = fake_secret('a');
        let s2 = fake_secret('b');
        let blob = format!("noise\0{s1}{s2}https://x");
        let secrets = scan_secrets(blob.as_bytes());
        assert_eq!(secrets, vec![s1, s2.clone()]);
        // The trailing "https://x" must not be glued onto the second secret.
        assert!(!secrets[1].contains("https"));
    }

    #[test]
    fn dedups_repeated_secrets() {
        let s1 = fake_secret('a');
        let blob = format!("{s1} xx {s1}");
        assert_eq!(scan_secrets(blob.as_bytes()), vec![s1]);
    }

    #[test]
    fn no_secret_yields_empty() {
        assert!(scan_secrets(b"nothing to see here").is_empty());
    }

    #[test]
    fn prefix_with_short_body_is_ignored() {
        // "GOCSPX-" with fewer than 28 valid body chars → not a secret.
        assert!(scan_secrets(b"GOCSPX-tooShort rest").is_empty());
    }
}
