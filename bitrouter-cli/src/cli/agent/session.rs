//! Named ACP session persistence.
//!
//! A user-chosen `--session <name>` is mapped to the ACP-assigned
//! `acp_session_id` so subsequent invocations can resume the session via
//! `AgentProvider::load_session`. The store is a single JSON file at
//! `<home>/sessions/agent-sessions.json`, written atomically
//! (temp + rename). No locking; concurrent writers are not expected in v1.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub name: String,
    pub agent: String,
    pub acp_session_id: String,
    pub cwd: PathBuf,
    pub created_at: u64,
    pub last_used: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionFile {
    sessions: Vec<SessionRecord>,
}

/// On-disk store for named ACP sessions.
pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    /// Construct a store backed by the given file path.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Load all session records, sorted by `last_used` descending.
    pub fn list(&self) -> io::Result<Vec<SessionRecord>> {
        let file = self.read_file()?;
        let mut sessions = file.sessions;
        sessions.sort_by(|a, b| b.last_used.cmp(&a.last_used));
        Ok(sessions)
    }

    /// Look up a session by name. Returns `Ok(None)` when absent.
    pub fn load(&self, name: &str) -> io::Result<Option<SessionRecord>> {
        let file = self.read_file()?;
        Ok(file.sessions.into_iter().find(|s| s.name == name))
    }

    /// Insert or refresh a named session.
    ///
    /// `created_at` should be `Some(previous_value)` when refreshing an
    /// existing record (e.g. a resumed `--session` whose prior record
    /// we just loaded) and `None` when creating a brand new entry. The
    /// stored `last_used` is always set to now.
    pub fn upsert(
        &self,
        name: &str,
        agent: &str,
        acp_session_id: &str,
        cwd: &Path,
        created_at: Option<u64>,
    ) -> io::Result<()> {
        let now = now_unix_seconds();
        self.save(SessionRecord {
            name: name.to_owned(),
            agent: agent.to_owned(),
            acp_session_id: acp_session_id.to_owned(),
            cwd: cwd.to_owned(),
            created_at: created_at.unwrap_or(now),
            last_used: now,
        })
    }

    /// Insert or update a session record. Sets `last_used` to now. If a
    /// record with the same name exists, its `created_at` is preserved.
    pub fn save(&self, mut record: SessionRecord) -> io::Result<()> {
        let mut file = self.read_file()?;
        record.last_used = now_unix_seconds();
        if let Some(existing) = file.sessions.iter_mut().find(|s| s.name == record.name) {
            record.created_at = existing.created_at;
            *existing = record;
        } else {
            file.sessions.push(record);
        }
        self.write_file(&file)
    }

    /// Remove a session by name. No-op if absent.
    pub fn remove(&self, name: &str) -> io::Result<()> {
        let mut file = self.read_file()?;
        let before = file.sessions.len();
        file.sessions.retain(|s| s.name != name);
        if file.sessions.len() == before {
            return Ok(());
        }
        self.write_file(&file)
    }

    fn read_file(&self) -> io::Result<SessionFile> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).or_else(|_| Ok(SessionFile::default())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(SessionFile::default()),
            Err(e) => Err(e),
        }
    }

    fn write_file(&self, file: &SessionFile) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = tmp_path(&self.path);
        let json = serde_json::to_vec_pretty(file).map_err(io::Error::other)?;
        fs::write(&tmp, json)?;
        fs::rename(&tmp, &self.path)
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

/// Seconds since the Unix epoch. Saturates at `0` if the system clock is
/// before the epoch (impossible in practice; this avoids the panic).
pub fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn rec(name: &str, agent: &str, sid: &str) -> SessionRecord {
        SessionRecord {
            name: name.to_owned(),
            agent: agent.to_owned(),
            acp_session_id: sid.to_owned(),
            cwd: PathBuf::from("/tmp"),
            created_at: 100,
            last_used: 100,
        }
    }

    #[test]
    fn load_missing_returns_none() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("sessions.json"));
        assert!(store.load("foo").unwrap().is_none());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("sessions.json"));
        store.save(rec("alpha", "claude", "acp-1")).unwrap();
        let got = store.load("alpha").unwrap().unwrap();
        assert_eq!(got.acp_session_id, "acp-1");
        assert!(got.last_used >= 100);
    }

    #[test]
    fn save_upserts_by_name() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("sessions.json"));
        store.save(rec("alpha", "claude", "acp-1")).unwrap();
        store.save(rec("alpha", "claude", "acp-2")).unwrap();
        let all = store.list().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].acp_session_id, "acp-2");
    }

    #[test]
    fn remove_is_idempotent() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("sessions.json"));
        store.save(rec("alpha", "claude", "acp-1")).unwrap();
        store.remove("alpha").unwrap();
        store.remove("alpha").unwrap();
        assert!(store.load("alpha").unwrap().is_none());
    }

    #[test]
    fn list_returns_all_saved_records() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("sessions.json"));
        store.save(rec("a", "x", "1")).unwrap();
        store.save(rec("b", "x", "2")).unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);
        let names: Vec<&str> = list.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
    }
}
