//! Durable fleet-manager state — `<base_repo>/.bitrouter/fleet-state.json`.
//!
//! The manager layer's memory across stops and crashes: which agents formed
//! the fleet and the *judgments* made about them (autonomy tier, review
//! status, allocated port, an unsent draft). Everything session-scoped —
//! worktree, branch, base_ref, session ids, the conversation itself — lives
//! in the per-session [`SessionRecord`](crate::record::SessionRecord) and
//! transcript; a [`FleetAgent`] only *references* them by `record_id`, so
//! every fact has exactly one durable home.
//!
//! This is memory, **not** auto-resume: nothing reads the file to relaunch
//! anything. Consumers today are the TUI's startup notice and anyone who
//! `cat`s it; the schema is deliberately the seed of the fleet daemon's
//! registry (TUI_SPEC §2), which will take over as the writer.
//!
//! Written atomically (temp + rename) on durable state changes and once at
//! teardown with `clean_shutdown: true` — so an unclean stop is readable as
//! such. Readers follow the house rule: never trust liveness from disk.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Current schema version. Readers skip (with a warning) files written by a
/// NEWER version rather than misreading them; older files parse via serde
/// defaults.
pub const FLEET_STATE_VERSION: u32 = 1;

/// The durable form of the fleet manager's state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetState {
    pub version: u32,
    /// Unix seconds when written (stamped by [`FleetStore::save`]).
    pub saved_at: u64,
    /// `true` only on the final write of an orderly teardown — a `false` in
    /// a file whose writer is gone means the manager crashed or was killed.
    pub clean_shutdown: bool,
    /// Pid of the writing manager process.
    pub writer_pid: u32,
    /// The hosted orchestrator, when the manager ran one. Its conversation
    /// is resumable only through the harness's own `--continue`/`--resume`
    /// for this cwd — an opaque PTY exposes no session id to record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestrator: Option<OrchestratorState>,
    pub agents: Vec<FleetAgent>,
}

/// The orchestrator pane's identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorState {
    /// Interactive binary hosted in the PTY pane (`claude`, `codex`, …).
    pub binary: String,
    /// The `--model` pin it launched with, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// One fleet agent's manager-layer state. Session-scoped facts live in the
/// session record this `record_id` points at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetAgent {
    /// Join key → `.bitrouter/sessions/<record_id>.json` + transcript.
    pub record_id: String,
    /// Autonomy tier label (`manual` / `assisted` / `auto`).
    pub autonomy: String,
    /// Ready-to-review diff stat `(files, adds, dels)`, when one is pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<(u64, u64, u64)>,
    /// The fleet-allocated dev-server `PORT`, when one was drawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Title of a permission that was pending at write time. A manager that
    /// stops while this is set denies it (dropping the handle is a deny), so
    /// a reader knows the request did not survive the stop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<String>,
    /// Unsent composer draft — the most-felt crash loss. Plaintext, same
    /// exposure class as the transcripts beside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<String>,
    /// A prompt turn was in flight at write time.
    pub turn_active: bool,
    /// The agent process had already exited at write time.
    pub exited: bool,
}

/// Reads/writes the fleet state at `<base_repo>/.bitrouter/fleet-state.json`.
pub struct FleetStore {
    dot_dir: PathBuf,
    path: PathBuf,
}

impl FleetStore {
    pub fn new(base_repo: &Path) -> Self {
        let dot_dir = base_repo.join(".bitrouter");
        let path = dot_dir.join("fleet-state.json");
        Self { dot_dir, path }
    }

    /// The on-disk path (for messages that point readers at the file).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the previous fleet state. `None` when the file is missing,
    /// unparseable, or written by a newer schema — a durable-memory reader
    /// must degrade to "no memory", never fail the caller.
    pub async fn load(&self) -> Option<FleetState> {
        let raw = match tokio::fs::read_to_string(&self.path).await {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!(error = %e, path = %self.path.display(), "unreadable fleet state");
                return None;
            }
        };
        let state: FleetState = match serde_json::from_str(&raw) {
            Ok(state) => state,
            Err(e) => {
                tracing::warn!(error = %e, path = %self.path.display(), "invalid fleet state");
                return None;
            }
        };
        if state.version > FLEET_STATE_VERSION {
            tracing::warn!(
                version = state.version,
                supported = FLEET_STATE_VERSION,
                "fleet state written by a newer bitrouter; ignoring"
            );
            return None;
        }
        Some(state)
    }

    /// Write `state` atomically, stamping `saved_at` (the caller builds the
    /// struct with `saved_at: 0` so successive builds stay comparable).
    pub async fn save(&self, state: &FleetState) -> Result<()> {
        crate::dotdir::ensure_self_ignored(&self.dot_dir)
            .with_context(|| format!("creating {}", self.dot_dir.display()))?;
        let mut stamped = state.clone();
        stamped.saved_at = crate::record::now_unix();
        let json = serde_json::to_string_pretty(&stamped).context("serialising fleet state")?;
        crate::record::write_atomic(&self.path, &json).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> FleetState {
        FleetState {
            version: FLEET_STATE_VERSION,
            saved_at: 0,
            clean_shutdown: false,
            writer_pid: 4242,
            orchestrator: Some(OrchestratorState {
                binary: "claude".to_string(),
                model: None,
            }),
            agents: vec![FleetAgent {
                record_id: "r1".to_string(),
                autonomy: "assisted".to_string(),
                review: Some((3, 42, 7)),
                port: Some(3101),
                pending: Some("rm -rf scratch".to_string()),
                draft: Some("half-typed".to_string()),
                turn_active: true,
                exited: false,
            }],
        }
    }

    #[tokio::test]
    async fn save_then_load_round_trips_and_stamps_saved_at() {
        let base = tempfile::tempdir().expect("tempdir");
        let store = FleetStore::new(base.path());
        store.save(&state()).await.expect("save");
        let loaded = store.load().await.expect("some");
        assert!(loaded.saved_at > 0, "saved_at stamped by save");
        assert_eq!(loaded.agents, state().agents);
        assert_eq!(loaded.orchestrator, state().orchestrator);
        // The dot dir became self-ignoring on first save.
        assert_eq!(
            std::fs::read_to_string(base.path().join(".bitrouter/.gitignore")).expect("read"),
            "*\n"
        );
    }

    #[tokio::test]
    async fn load_is_none_when_missing_or_malformed_or_newer() {
        let base = tempfile::tempdir().expect("tempdir");
        let store = FleetStore::new(base.path());
        assert!(store.load().await.is_none(), "missing file");

        std::fs::create_dir_all(base.path().join(".bitrouter")).expect("mkdir");
        std::fs::write(store.path(), "not json").expect("write junk");
        assert!(store.load().await.is_none(), "malformed file");

        let mut newer = state();
        newer.version = FLEET_STATE_VERSION + 1;
        std::fs::write(
            store.path(),
            serde_json::to_string(&newer).expect("serialise"),
        )
        .expect("write newer");
        assert!(store.load().await.is_none(), "newer schema is not misread");
    }

    #[tokio::test]
    async fn save_leaves_no_temp_files() {
        let base = tempfile::tempdir().expect("tempdir");
        let store = FleetStore::new(base.path());
        store.save(&state()).await.expect("save");
        let leftovers: Vec<_> = std::fs::read_dir(base.path().join(".bitrouter"))
            .expect("read dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[tokio::test]
    async fn optional_fields_are_omitted_when_none() {
        let base = tempfile::tempdir().expect("tempdir");
        let store = FleetStore::new(base.path());
        let mut s = state();
        s.orchestrator = None;
        s.agents[0].review = None;
        s.agents[0].pending = None;
        s.agents[0].draft = None;
        s.agents[0].port = None;
        store.save(&s).await.expect("save");
        let raw = std::fs::read_to_string(store.path()).expect("read");
        for absent in ["orchestrator", "review", "pending", "draft", "port"] {
            assert!(!raw.contains(absent), "{absent} serialized despite None");
        }
    }
}
