//! On-disk ledger of installed ACP agents.
//!
//! A JSON array of [`InstallRecord`]s stored at
//! `<home>/agents/state.json`.  The file is the authoritative record of
//! *which* agents we installed and *how*, so PATH lookups can be bypassed
//! in favour of a known-good binary path.
//!
//! The file is a cache, not a source of truth for correctness — if it
//! is missing or corrupt, install/uninstall operations still work, and
//! [`load_state`] returns an empty vector so callers can rebuild it.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bitrouter_config::AgentConfig;
use serde::{Deserialize, Serialize};

/// One entry in the install ledger.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstallRecord {
    /// Agent id (registry key, e.g. `"claude-acp"`).
    pub id: String,

    /// Version string recorded at install time.
    pub version: String,

    /// Which distribution method was used.
    pub method: InstallMethod,

    /// Absolute path to the installed binary.  Set for
    /// [`InstallMethod::Binary`]; `None` for `npx`/`uvx` which resolve
    /// through their own runtime shim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_binary_path: Option<PathBuf>,

    /// Install timestamp in Unix seconds.
    pub installed_at: u64,
}

/// Distribution method recorded on install.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InstallMethod {
    Npx,
    Uvx,
    Binary,
}

impl fmt::Display for InstallMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Npx => "npx",
            Self::Uvx => "uvx",
            Self::Binary => "binary",
        })
    }
}

/// Return the current wall clock in Unix seconds.
///
/// Falls back to `0` on clock skew (pre-1970).  Callers only use the
/// value for display and rough ordering, so a sentinel zero is safe.
pub fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read the install ledger.
///
/// Missing file → `Ok(vec![])`.  Corrupt JSON → `Ok(vec![])` with a
/// `tracing` warning so callers can recover by overwriting on next save.
pub async fn load_state(state_file: &Path) -> Result<Vec<InstallRecord>, String> {
    match tokio::fs::read_to_string(state_file).await {
        Ok(raw) => match serde_json::from_str::<Vec<InstallRecord>>(&raw) {
            Ok(records) => Ok(records),
            Err(e) => {
                tracing::warn!(path = %state_file.display(), error = %e, "corrupt install state — starting fresh");
                Ok(Vec::new())
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("failed to read {}: {e}", state_file.display())),
    }
}

/// Write the install ledger atomically (temp + rename).
///
/// Creates the parent directory if it is missing.
pub async fn save_state(state_file: &Path, records: &[InstallRecord]) -> Result<(), String> {
    if let Some(parent) = state_file.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }

    let json = serde_json::to_vec_pretty(records)
        .map_err(|e| format!("failed to serialise install state: {e}"))?;

    let tmp = state_file.with_extension("json.tmp");
    tokio::fs::write(&tmp, &json)
        .await
        .map_err(|e| format!("failed to write {}: {e}", tmp.display()))?;

    tokio::fs::rename(&tmp, state_file).await.map_err(|e| {
        format!(
            "failed to rename {} → {}: {e}",
            tmp.display(),
            state_file.display()
        )
    })
}

/// Insert or replace the record with the matching `id`.
pub async fn upsert_record(state_file: &Path, record: InstallRecord) -> Result<(), String> {
    let mut records = load_state(state_file).await?;
    if let Some(slot) = records.iter_mut().find(|r| r.id == record.id) {
        *slot = record;
    } else {
        records.push(record);
    }
    save_state(state_file, &records).await
}

/// Remove the record for `agent_id`.  No-op if absent.
pub async fn remove_record(state_file: &Path, agent_id: &str) -> Result<(), String> {
    let mut records = load_state(state_file).await?;
    let before = records.len();
    records.retain(|r| r.id != agent_id);
    if records.len() == before {
        return Ok(());
    }
    save_state(state_file, &records).await
}

/// Return the record for `agent_id`, if any.
pub async fn find_record(
    state_file: &Path,
    agent_id: &str,
) -> Result<Option<InstallRecord>, String> {
    let records = load_state(state_file).await?;
    Ok(records.into_iter().find(|r| r.id == agent_id))
}

/// Synchronous read of the ledger — used in config-assembly paths that
/// cannot await.  Returns an empty vector on any error (missing,
/// unreadable, or corrupt) so assembly is tolerant to cold starts.
pub fn load_state_sync(state_file: &Path) -> Vec<InstallRecord> {
    let Ok(raw) = std::fs::read_to_string(state_file) else {
        return Vec::new();
    };
    match serde_json::from_str(&raw) {
        Ok(records) => records,
        Err(e) => {
            tracing::warn!(
                path = %state_file.display(),
                error = %e,
                "corrupt install state (sync read) — starting fresh"
            );
            Vec::new()
        }
    }
}

/// Rewrite each agent's launch info using the recorded install state.
///
/// For agents installed via [`InstallMethod::Binary`], we replace the
/// `binary` field with the recorded `resolved_binary_path`.  Npx/Uvx
/// installs are left untouched — `resolve_launch` handles those by
/// invoking their runtime shim directly.
///
/// This is the read path that pairs with [`upsert_record`]: the
/// installer writes the resolved path; this function lets the TUI and
/// CLI start cold and still find their installed agents.
pub fn overlay_install_state_sync(agents: &mut HashMap<String, AgentConfig>, state_file: &Path) {
    for record in load_state_sync(state_file) {
        if let (InstallMethod::Binary, Some(path)) =
            (record.method, record.resolved_binary_path.as_ref())
            && let Some(cfg) = agents.get_mut(&record.id)
        {
            cfg.binary = path.to_string_lossy().into_owned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn rec(id: &str) -> InstallRecord {
        InstallRecord {
            id: id.to_owned(),
            version: "1.0.0".to_owned(),
            method: InstallMethod::Npx,
            resolved_binary_path: None,
            installed_at: 1_700_000_000,
        }
    }

    #[tokio::test]
    async fn load_missing_returns_empty() -> Result<(), String> {
        let dir = TempDir::new().map_err(|e| e.to_string())?;
        let path = dir.path().join("state.json");
        let records = load_state(&path).await?;
        assert!(records.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn upsert_roundtrip() -> Result<(), String> {
        let dir = TempDir::new().map_err(|e| e.to_string())?;
        let path = dir.path().join("state.json");

        upsert_record(&path, rec("alpha")).await?;
        upsert_record(&path, rec("beta")).await?;

        let records = load_state(&path).await?;
        assert_eq!(records.len(), 2);

        // Upsert same id with different version.
        let mut updated = rec("alpha");
        updated.version = "2.0.0".to_owned();
        upsert_record(&path, updated).await?;

        let records = load_state(&path).await?;
        assert_eq!(records.len(), 2);
        let alpha = records
            .iter()
            .find(|r| r.id == "alpha")
            .ok_or("missing alpha")?;
        assert_eq!(alpha.version, "2.0.0");
        Ok(())
    }

    #[tokio::test]
    async fn remove_is_idempotent() -> Result<(), String> {
        let dir = TempDir::new().map_err(|e| e.to_string())?;
        let path = dir.path().join("state.json");

        upsert_record(&path, rec("alpha")).await?;
        remove_record(&path, "alpha").await?;
        remove_record(&path, "alpha").await?; // no-op

        let records = load_state(&path).await?;
        assert!(records.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn corrupt_file_recovers_as_empty() -> Result<(), String> {
        let dir = TempDir::new().map_err(|e| e.to_string())?;
        let path = dir.path().join("state.json");
        tokio::fs::write(&path, b"this is not json")
            .await
            .map_err(|e| e.to_string())?;

        let records = load_state(&path).await?;
        assert!(records.is_empty());

        // Upsert after corruption overwrites cleanly.
        upsert_record(&path, rec("gamma")).await?;
        let records = load_state(&path).await?;
        assert_eq!(records.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn save_creates_parent_dir() -> Result<(), String> {
        let dir = TempDir::new().map_err(|e| e.to_string())?;
        let path = dir.path().join("nested").join("agents").join("state.json");

        save_state(&path, &[rec("omega")]).await?;
        assert!(path.exists());
        Ok(())
    }
}
