//! On-disk session records — one JSON file per session under
//! `<base_repo>/.bitrouter/sessions/<record_id>.json`.
//!
//! Written at launch and updated at shutdown, records give managers a durable
//! view of which sessions ran (or are running) in a repo: identity (all three
//! tiers), pid, and lifecycle timestamps.
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
    /// directory if needed. The write is atomic (sibling temp file +
    /// rename), so a crash mid-write can never truncate a record.
    pub async fn write(&self, record: &SessionRecord) -> Result<()> {
        if let Some(dot_dir) = self.dir.parent() {
            crate::dotdir::ensure_self_ignored(dot_dir)
                .with_context(|| format!("creating {}", dot_dir.display()))?;
        }
        tokio::fs::create_dir_all(&self.dir)
            .await
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let path = self.dir.join(format!("{}.json", record.record_id));
        let json = serde_json::to_string_pretty(record).context("serialising session record")?;
        write_atomic(&path, &json).await
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

/// Atomic file write: a sibling temp file renamed into place, so readers
/// only ever see a complete document. The temp name carries the pid so two
/// processes writing the same path can't collide on it.
pub(crate) async fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "state".to_string());
    let tmp = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
    tokio::fs::write(&tmp, contents)
        .await
        .with_context(|| format!("writing {}", tmp.display()))?;
    match tokio::fs::rename(&tmp, path).await {
        Ok(()) => Ok(()),
        Err(e) => {
            // Best-effort: don't leave the temp file behind on failure.
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(e).with_context(|| format!("renaming into {}", path.display()))
        }
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
            pid: 4242,
            started_at: 1_750_000_000,
            status: RecordStatus::Running,
            ended_at: None,
        }
    }

    /// Records written before the worktree fields were dropped must still
    /// load. Every other test here round-trips through `write` -> `list`,
    /// which can never catch a schema break — this one parses an on-disk
    /// literal, so it is the only guard on the direction users actually hit
    /// (a new binary reading records an older one left behind).
    #[test]
    fn legacy_record_with_dropped_worktree_fields_still_parses() {
        let legacy = r#"{
            "record_id": "r1",
            "agent_id": "claude",
            "acp_session_id": "u1",
            "agent_session_id": null,
            "pid": 4242,
            "started_at": 1750000000,
            "status": "running",
            "ended_at": null,
            "worktree": "/repo/.bitrouter/worktrees/claude-abc",
            "branch": "bitrouter/claude-abc",
            "base_ref": "deadbeef"
        }"#;
        let parsed: SessionRecord = serde_json::from_str(legacy).expect("legacy record parses");
        assert_eq!(parsed.record_id, "r1");
        assert_eq!(parsed.status, RecordStatus::Running);
        assert_eq!(parsed.acp_session_id.as_deref(), Some("u1"));
    }

    #[tokio::test]
    async fn writes_are_atomic_and_leave_no_temp_files() {
        let base = tempfile::tempdir().expect("tempdir");
        let store = RecordStore::new(base.path());
        store.write(&record("r1")).await.expect("write");
        let leftovers: Vec<_> = std::fs::read_dir(base.path().join(".bitrouter/sessions"))
            .expect("read dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp files must be renamed away");
    }

    #[tokio::test]
    async fn store_makes_the_dot_dir_self_ignoring() {
        let base = tempfile::tempdir().expect("tempdir");
        let store = RecordStore::new(base.path());
        store.write(&record("r1")).await.expect("write");
        assert_eq!(
            std::fs::read_to_string(base.path().join(".bitrouter/.gitignore")).expect("read"),
            "*\n"
        );
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
