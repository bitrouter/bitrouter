//! Persistent store for the BitRouter Cloud `brk_*` API key.
//!
//! One credentials file per bitrouter home:
//!   `<home>/credentials` (default `~/.bitrouter/credentials`)
//!
//! The file is JSON and is created with `0600` permissions on Unix so
//! only the owner can read it. The only secret is `api_key`; `key_id`,
//! `base_url`, and `minted_at` are metadata.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// On-disk shape of `<home>/credentials`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudCredentials {
    /// Raw `brk_*` API key. Sent as `Authorization: Bearer <api_key>`.
    pub api_key: String,
    /// Opaque key id — useful for revocation from the dashboard.
    pub key_id: String,
    /// Cloud base URL the key was minted against (e.g. `https://bitrouter.ai`).
    pub base_url: String,
    /// Unix seconds when the key was minted.
    pub minted_at: u64,
}

impl CloudCredentials {
    /// Path to the credentials file under the given home directory.
    pub fn path(home: &Path) -> PathBuf {
        home.join("credentials")
    }

    /// Load credentials from disk. Returns `None` if the file is missing
    /// or unparseable — callers should treat that as "logged out".
    pub fn load(home: &Path) -> Option<Self> {
        let data = std::fs::read_to_string(Self::path(home)).ok()?;
        serde_json::from_str(&data).ok()
    }

    /// Persist credentials to disk with owner-only permissions on Unix.
    pub fn save(&self, home: &Path) -> Result<(), Box<dyn std::error::Error>> {
        std::fs::create_dir_all(home)?;
        let path = Self::path(home);
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;
        set_owner_only(&path)?;
        Ok(())
    }

    /// Remove the credentials file. A no-op when already absent.
    pub fn delete(home: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let path = Self::path(home);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(unix)]
fn set_owner_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_credentials() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(CloudCredentials::load(dir.path()).is_none());

        let creds = CloudCredentials {
            api_key: "brk_example_secret".into(),
            key_id: "abc123".into(),
            base_url: "https://bitrouter.ai".into(),
            minted_at: 1_700_000_000,
        };
        creds.save(dir.path()).expect("save");

        let loaded = CloudCredentials::load(dir.path()).expect("loaded");
        assert_eq!(loaded.api_key, creds.api_key);
        assert_eq!(loaded.key_id, creds.key_id);
        assert_eq!(loaded.base_url, creds.base_url);
        assert_eq!(loaded.minted_at, creds.minted_at);
    }

    #[test]
    fn delete_missing_file_is_ok() {
        let dir = tempfile::tempdir().expect("tempdir");
        // No credentials file present.
        CloudCredentials::delete(dir.path()).expect("delete missing");
    }

    #[test]
    fn delete_removes_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let creds = CloudCredentials {
            api_key: "brk_x".into(),
            key_id: "id1".into(),
            base_url: "https://bitrouter.ai".into(),
            minted_at: 1,
        };
        creds.save(dir.path()).expect("save");
        assert!(CloudCredentials::load(dir.path()).is_some());
        CloudCredentials::delete(dir.path()).expect("delete");
        assert!(CloudCredentials::load(dir.path()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn credentials_file_is_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let creds = CloudCredentials {
            api_key: "brk_secret".into(),
            key_id: "id".into(),
            base_url: "https://bitrouter.ai".into(),
            minted_at: 0,
        };
        creds.save(dir.path()).expect("save");
        let meta = std::fs::metadata(CloudCredentials::path(dir.path())).expect("meta");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "credentials file must be 0600 (got {mode:o})");
    }
}
