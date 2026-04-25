//! Import-existing-sessions modal: state transitions, marker file,
//! first-launch toast.
//!
//! The on-disk scan is kicked off in `App::new` (see
//! `app/mod.rs::spawn_import_scan`); this module owns the post-scan
//! UX: storing results, deciding when to nag the user, building modal
//! state when `Ctrl-I` is pressed, and committing selections.

use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::model::{ImportCandidate, ImportEntry, ImportState, Modal};

use super::App;

impl App {
    pub(super) fn handle_import_scan_result(&mut self, sessions: Vec<ImportCandidate>) {
        self.state.discovered_sessions = sessions;
        self.maybe_show_import_nag();
    }

    /// Show the first-launch nag toast if the scan found sessions and
    /// the user hasn't dismissed the prompt for this cwd before. Idempotent
    /// within a single TUI process via `state.import_nag_shown`.
    fn maybe_show_import_nag(&mut self) {
        if self.state.import_nag_shown {
            return;
        }
        let count = self.state.discovered_sessions.len();
        if count == 0 {
            return;
        }
        if self.import_marker_path().exists() {
            self.state.import_nag_shown = true;
            return;
        }
        self.push_system_msg(&format!(
            "Found {count} existing thread(s) in this cwd. Press Ctrl-I to import."
        ));
        self.state.import_nag_shown = true;
    }

    /// Open the import modal. No-op when the scan hasn't surfaced any
    /// sessions — `Ctrl-I` is silently ignored in that state rather
    /// than opening an empty modal.
    pub(super) fn open_import_modal(&mut self) {
        if self.state.discovered_sessions.is_empty() {
            return;
        }
        let entries = build_import_entries(&self.state.discovered_sessions);
        // Anchor cursor on the first selectable item, skipping the
        // leading group header.
        let cursor = entries
            .iter()
            .position(|e| matches!(e, ImportEntry::Item(_)))
            .unwrap_or(0);
        self.state.modal = Some(Modal::ImportThreads(ImportState {
            entries,
            cursor,
            selected: HashSet::new(),
        }));
    }

    /// Spawn imports for everything currently selected, close the
    /// modal, and write the dismissal marker for this cwd.
    pub(super) fn confirm_import_modal(&mut self) {
        let selections: Vec<ImportCandidate> = match &self.state.modal {
            Some(Modal::ImportThreads(state)) => state
                .selected
                .iter()
                .filter_map(|&idx| match state.entries.get(idx) {
                    Some(ImportEntry::Item(c)) => Some(c.clone()),
                    _ => None,
                })
                .collect(),
            _ => return,
        };

        for cand in &selections {
            self.import_session(
                &cand.agent_id,
                cand.external_session_id.clone(),
                cand.source_path.clone(),
                cand.title_hint.clone(),
            );
        }
        if !selections.is_empty() {
            self.push_system_msg(&format!("Importing {n} thread(s)...", n = selections.len()));
        }

        self.dismiss_import_modal();
    }

    /// Close the modal and write the dismissal marker so the toast
    /// doesn't reappear on future runs in this cwd.
    pub(super) fn dismiss_import_modal(&mut self) {
        self.state.modal = None;
        if let Err(err) = self.write_import_marker() {
            // Marker write is best-effort — log via system message but
            // don't fail the dismissal. Without it the toast just
            // reappears next launch.
            self.push_system_msg(&format!(
                "(could not persist import-dismissed marker: {err})"
            ));
        }
    }

    /// Path of the per-cwd marker file. Lives under `cache_dir` so we
    /// reuse an existing TUI-managed directory; the cwd is hashed via
    /// the std `DefaultHasher` (SipHash) since the path is not
    /// security-sensitive and we don't want a new dependency.
    pub(super) fn import_marker_path(&self) -> PathBuf {
        let cwd = self.session_system.launch_cwd();
        marker_path_for(&self.state.config.cache_dir, cwd)
    }

    fn write_import_marker(&self) -> std::io::Result<()> {
        let path = self.import_marker_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, b"")
    }
}

pub(super) fn marker_path_for(cache_dir: &Path, cwd: &Path) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    cwd.hash(&mut hasher);
    let hash = hasher.finish();
    cache_dir.join(format!("import_dismissed_{hash:016x}"))
}

/// Group the scanner's flat output by agent_id (alphabetic) and emit
/// header-then-items rows. Items inside each group are mtime-descending
/// (the scanners already return them in that order, so we preserve
/// stream order within an agent).
pub(super) fn build_import_entries(candidates: &[ImportCandidate]) -> Vec<ImportEntry> {
    // Stable per-agent grouping without disturbing within-group order.
    let mut by_agent: Vec<(String, Vec<ImportCandidate>)> = Vec::new();
    for c in candidates {
        match by_agent.iter_mut().find(|(name, _)| name == &c.agent_id) {
            Some((_, group)) => group.push(c.clone()),
            None => by_agent.push((c.agent_id.clone(), vec![c.clone()])),
        }
    }
    by_agent.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = Vec::new();
    for (agent_id, items) in by_agent {
        out.push(ImportEntry::Group {
            agent_id,
            count: items.len(),
        });
        for item in items {
            out.push(ImportEntry::Item(item));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cand(agent: &str, id: &str, mtime: i64) -> ImportCandidate {
        ImportCandidate {
            agent_id: agent.to_string(),
            external_session_id: id.to_string(),
            title_hint: Some(format!("{id}-title")),
            last_active_at: mtime,
            source_path: PathBuf::from(format!("/tmp/{id}.jsonl")),
        }
    }

    #[test]
    fn build_entries_groups_per_agent_with_header_first() {
        let entries = build_import_entries(&[
            cand("claude-code", "a", 100),
            cand("codex", "b", 200),
            cand("claude-code", "c", 50),
        ]);
        // claude-code (Group + 2 items) then codex (Group + 1 item).
        assert_eq!(entries.len(), 5);
        match &entries[0] {
            ImportEntry::Group { agent_id, count } => {
                assert_eq!(agent_id, "claude-code");
                assert_eq!(*count, 2);
            }
            _ => panic!("expected group header at 0"),
        }
        // First item under claude-code retains scanner order (`a` first).
        match &entries[1] {
            ImportEntry::Item(c) => assert_eq!(c.external_session_id, "a"),
            _ => panic!("expected item at 1"),
        }
        match &entries[3] {
            ImportEntry::Group { agent_id, .. } => assert_eq!(agent_id, "codex"),
            _ => panic!("expected group at 3"),
        }
    }

    #[test]
    fn build_entries_empty_input_returns_empty() {
        assert!(build_import_entries(&[]).is_empty());
    }

    #[test]
    fn marker_path_differs_for_different_cwds() {
        let cache = PathBuf::from("/cache");
        let a = marker_path_for(&cache, &PathBuf::from("/proj/a"));
        let b = marker_path_for(&cache, &PathBuf::from("/proj/b"));
        assert_ne!(a, b);
        // Both land under cache_dir.
        assert!(a.starts_with(&cache));
        assert!(b.starts_with(&cache));
    }

    #[test]
    fn marker_path_stable_for_same_cwd() {
        let cache = PathBuf::from("/cache");
        let cwd = PathBuf::from("/proj/x");
        assert_eq!(marker_path_for(&cache, &cwd), marker_path_for(&cache, &cwd));
    }
}
