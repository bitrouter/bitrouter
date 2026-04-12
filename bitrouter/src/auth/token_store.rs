//! Persistent OAuth token store.
//!
//! Stores OAuth tokens keyed by provider name in a JSON file
//! (`tokens.json`) within the BitRouter home directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A stored OAuth token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthToken {
    /// The access token used for API requests.
    pub access_token: String,
    /// Unix timestamp when the token expires (0 = non-expiring).
    #[serde(default)]
    pub expires_at: u64,
    /// Optional refresh token for re-acquisition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

/// Persistent OAuth token store backed by a JSON file.
pub struct TokenStore {
    path: PathBuf,
    tokens: HashMap<String, OAuthToken>,
}

impl TokenStore {
    /// Load the token store from disk, or create an empty one if the file
    /// does not exist or cannot be parsed.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let tokens = Self::read_file(&path).unwrap_or_default();
        Self { path, tokens }
    }

    /// Look up a token by provider name.
    ///
    /// Returns `None` if no token is stored or the token has expired.
    pub fn get(&self, provider: &str) -> Option<&OAuthToken> {
        let token = self.tokens.get(provider)?;
        if token.expires_at != 0 {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if now >= token.expires_at {
                return None;
            }
        }
        Some(token)
    }

    /// Store a token for the given provider and persist to disk.
    pub fn set(
        &mut self,
        provider: &str,
        token: OAuthToken,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.tokens.insert(provider.to_owned(), token);
        self.write_file()
    }

    fn read_file(path: &Path) -> Option<HashMap<String, OAuthToken>> {
        let data = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&data).ok()
    }

    fn write_file(&self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.tokens)?;
        std::fs::write(&self.path, json)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_token_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tokens.json");

        let mut store = TokenStore::load(&path);
        assert!(store.get("github-copilot").is_none());

        store
            .set(
                "github-copilot",
                OAuthToken {
                    access_token: "ghu_test123".into(),
                    expires_at: 0,
                    refresh_token: None,
                },
            )
            .expect("write");

        assert_eq!(
            store.get("github-copilot").map(|t| &t.access_token),
            Some(&"ghu_test123".to_owned()),
        );

        // Reload from disk
        let reloaded = TokenStore::load(&path);
        assert_eq!(
            reloaded.get("github-copilot").map(|t| &t.access_token),
            Some(&"ghu_test123".to_owned()),
        );
    }

    #[test]
    fn expired_token_returns_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tokens.json");

        let mut store = TokenStore::load(&path);
        store
            .set(
                "expired-provider",
                OAuthToken {
                    access_token: "expired".into(),
                    expires_at: 1, // already expired
                    refresh_token: None,
                },
            )
            .expect("write");

        assert!(store.get("expired-provider").is_none());
    }

    #[test]
    fn non_expiring_token_always_valid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tokens.json");

        let mut store = TokenStore::load(&path);
        store
            .set(
                "forever",
                OAuthToken {
                    access_token: "eternal".into(),
                    expires_at: 0,
                    refresh_token: None,
                },
            )
            .expect("write");

        assert!(store.get("forever").is_some());
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let store = TokenStore::load("/tmp/nonexistent_tokens_12345.json");
        assert!(store.get("anything").is_none());
    }
}
