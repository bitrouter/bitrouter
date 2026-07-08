//! On-disk session records — one JSON file per session under
//! `<base_repo>/.bitrouter/sessions/<record_id>.json`.
//!
//! Written at launch and updated at shutdown, records give managers and the
//! `bitrouter acp sessions` CLI a durable view of which sessions ran (or are
//! running) in a repo: identity (all three tiers), worktree, pid, and
//! lifecycle timestamps. They are also the persistence substrate a future
//! `session/load` (v2 warm sessions) will resume from.
//!
//! A record whose `status` is `running` may be stale if the substrate process
//! died without shutting down; consumers should verify `pid` liveness before
//! trusting it.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Lifecycle status persisted in a [`SessionRecord`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordStatus {
    Running,
    Exited,
}

/// The durable form of one session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Stable manager-facing id (also the file name).
    pub record_id: String,
    pub agent_id: String,
    /// ACP wire session id from the upstream `session/new`.
    pub acp_session_id: Option<String>,
    /// Provider-native id from `_meta.agentSessionId`, when exposed.
    pub agent_session_id: Option<String>,
    /// Absolute path of the session's worktree, when one was provisioned.
    pub worktree: Option<PathBuf>,
    /// Pid of the substrate process that owns (owned) the session.
    pub pid: u32,
    /// Unix seconds when the session launched.
    pub started_at: u64,
    pub status: RecordStatus,
    /// Unix seconds when the session shut down; `None` while running.
    pub ended_at: Option<u64>,
}

/// Current time as unix seconds (0 if the clock is before the epoch).
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Reads/writes [`SessionRecord`]s under `<base_repo>/.bitrouter/sessions/`.
pub struct RecordStore {
    dir: PathBuf,
}

impl RecordStore {
    pub fn new(base_repo: &Path) -> Self {
        Self {
            dir: base_repo.join(".bitrouter").join("sessions"),
        }
    }

    /// Write (or overwrite) `record` as `<record_id>.json`, creating the
    /// directory if needed.
    pub async fn write(&self, record: &SessionRecord) -> Result<()> {
        tokio::fs::create_dir_all(&self.dir)
            .await
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let path = self.dir.join(format!("{}.json", record.record_id));
        let json = serde_json::to_string_pretty(record).context("serialising session record")?;
        tokio::fs::write(&path, json)
            .await
            .with_context(|| format!("writing {}", path.display()))
    }

    /// All parseable records in the store, unordered. Missing directory means
    /// no records; unparseable files are skipped with a warning rather than
    /// failing the whole listing.
    pub async fn list(&self) -> Result<Vec<SessionRecord>> {
        let mut records = Vec::new();
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(records),
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", self.dir.display()));
            }
        };
        while let Some(entry) = entries
            .next_entry()
            .await
            .with_context(|| format!("reading {}", self.dir.display()))?
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let raw = match tokio::fs::read_to_string(&path).await {
                Ok(raw) => raw,
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(), "unreadable session record");
                    continue;
                }
            };
            match serde_json::from_str::<SessionRecord>(&raw) {
                Ok(record) => records.push(record),
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(), "invalid session record");
                }
            }
        }
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str) -> SessionRecord {
        SessionRecord {
            record_id: id.to_string(),
            agent_id: "claude".to_string(),
            acp_session_id: Some("u1".to_string()),
            agent_session_id: None,
            worktree: None,
            pid: 4242,
            started_at: 1_750_000_000,
            status: RecordStatus::Running,
            ended_at: None,
        }
    }

    #[tokio::test]
    async fn write_then_list_round_trips() {
        let base = tempfile::tempdir().expect("tempdir");
        let store = RecordStore::new(base.path());

        store.write(&record("r1")).await.expect("write");
        let mut ended = record("r2");
        ended.status = RecordStatus::Exited;
        ended.ended_at = Some(1_750_000_100);
        store.write(&ended).await.expect("write");

        let mut listed = store.list().await.expect("list");
        listed.sort_by(|a, b| a.record_id.cmp(&b.record_id));
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].record_id, "r1");
        assert_eq!(listed[0].status, RecordStatus::Running);
        assert_eq!(listed[1].status, RecordStatus::Exited);
        assert_eq!(listed[1].ended_at, Some(1_750_000_100));
    }

    #[tokio::test]
    async fn list_empty_when_dir_missing() {
        let base = tempfile::tempdir().expect("tempdir");
        let store = RecordStore::new(base.path());
        assert!(store.list().await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn list_skips_invalid_files() {
        let base = tempfile::tempdir().expect("tempdir");
        let store = RecordStore::new(base.path());
        store.write(&record("good")).await.expect("write");
        std::fs::write(
            base.path().join(".bitrouter/sessions/broken.json"),
            "not json",
        )
        .expect("write junk");

        let listed = store.list().await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].record_id, "good");
    }

    #[tokio::test]
    async fn write_updates_existing_record() {
        let base = tempfile::tempdir().expect("tempdir");
        let store = RecordStore::new(base.path());
        let mut r = record("r1");
        store.write(&r).await.expect("write running");
        r.status = RecordStatus::Exited;
        r.ended_at = Some(now_unix());
        store.write(&r).await.expect("write exited");

        let listed = store.list().await.expect("list");
        assert_eq!(listed.len(), 1, "update must overwrite, not duplicate");
        assert_eq!(listed[0].status, RecordStatus::Exited);
    }
}
