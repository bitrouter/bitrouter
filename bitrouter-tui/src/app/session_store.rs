use std::collections::VecDeque;

use crate::model::{Session, SessionId};

/// Maximum length of the MRU focus history. Beyond this, the oldest entry
/// is discarded when a new focus is recorded. 50 entries is plenty for any
/// realistic workflow and keeps cycling lookups O(50) at worst.
const FOCUS_HISTORY_MAX: usize = 50;

/// Owns all active sessions and allocates monotonic [`SessionId`]s.
///
/// Callers use `store.active` directly for iteration and indexed access;
/// `allocate_id` is the one bit of bookkeeping the store owns.
pub struct SessionStore {
    /// Currently-active sessions, rendered in the sidebar.
    pub active: Vec<Session>,
    next_id: u64,
    /// MRU history of focused [`SessionId`]s — front is most recent.
    /// No duplicates; bounded to [`FOCUS_HISTORY_MAX`]. Read by the
    /// `Ctrl-Tab` / `Ctrl-Shift-Tab` cycle commands; updated by
    /// `record_focus` and `forget`.
    focus_history: VecDeque<SessionId>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            active: Vec::new(),
            next_id: 0,
            focus_history: VecDeque::new(),
        }
    }

    /// Allocate the next monotonic session id.
    pub fn allocate_id(&mut self) -> SessionId {
        let id = SessionId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Find the index of the first session bound to `agent_id`.
    pub fn find_by_agent(&self, agent_id: &str) -> Option<usize> {
        self.active.iter().position(|s| s.agent_id == agent_id)
    }

    /// Find the index of a session by its [`SessionId`].
    pub fn index_of(&self, id: SessionId) -> Option<usize> {
        self.active.iter().position(|s| s.id == id)
    }

    /// Mark `id` as the most-recently-focused session. Removes any prior
    /// occurrence so the deque stays duplicate-free.
    pub fn record_focus(&mut self, id: SessionId) {
        self.focus_history.retain(|x| *x != id);
        self.focus_history.push_front(id);
        while self.focus_history.len() > FOCUS_HISTORY_MAX {
            self.focus_history.pop_back();
        }
    }

    /// Drop `id` from the focus history (e.g. when a session is closed).
    pub fn forget(&mut self, id: SessionId) {
        self.focus_history.retain(|x| *x != id);
    }

    /// Read-only view of the focus history. Front is most-recently-focused.
    pub fn focus_history(&self) -> &VecDeque<SessionId> {
        &self.focus_history
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ScrollbackState, Session, SessionBadge, SessionStatus, agent_color};

    fn mk_session(store: &mut SessionStore, agent_id: &str) -> SessionId {
        let id = store.allocate_id();
        store.active.push(Session {
            id,
            agent_id: agent_id.to_string(),
            title: None,
            color: agent_color(id.0 as usize),
            acp_session_id: None,
            status: SessionStatus::Connecting,
            scrollback: ScrollbackState::new(),
            badge: SessionBadge::None,
        });
        id
    }

    #[test]
    fn allocate_id_is_monotonic() {
        let mut store = SessionStore::new();
        let a = store.allocate_id();
        let b = store.allocate_id();
        let c = store.allocate_id();
        assert_eq!(a.0, 0);
        assert_eq!(b.0, 1);
        assert_eq!(c.0, 2);
    }

    #[test]
    fn find_by_agent_returns_first_match() {
        let mut store = SessionStore::new();
        mk_session(&mut store, "claude-code");
        mk_session(&mut store, "codex");
        assert_eq!(store.find_by_agent("codex"), Some(1));
        assert_eq!(store.find_by_agent("missing"), None);
    }

    #[test]
    fn index_of_round_trips() {
        let mut store = SessionStore::new();
        let first = mk_session(&mut store, "claude-code");
        let second = mk_session(&mut store, "codex");
        assert_eq!(store.index_of(first), Some(0));
        assert_eq!(store.index_of(second), Some(1));
        assert_eq!(store.index_of(SessionId(999)), None);
    }

    #[test]
    fn allows_multiple_sessions_per_agent() {
        let mut store = SessionStore::new();
        let first = mk_session(&mut store, "claude-code");
        let second = mk_session(&mut store, "claude-code");
        assert_ne!(first, second);
        assert_eq!(store.active.len(), 2);
        // find_by_agent returns the FIRST one — callers that need a
        // specific session must look it up by SessionId.
        assert_eq!(store.find_by_agent("claude-code"), Some(0));
    }

    #[test]
    fn record_focus_promotes_to_front() {
        let mut store = SessionStore::new();
        let a = mk_session(&mut store, "a");
        let b = mk_session(&mut store, "b");
        let c = mk_session(&mut store, "c");
        store.record_focus(a);
        store.record_focus(b);
        store.record_focus(c);
        assert_eq!(
            store.focus_history().iter().copied().collect::<Vec<_>>(),
            vec![c, b, a]
        );
        // Re-focusing a moves it to the front, no duplicates.
        store.record_focus(a);
        assert_eq!(
            store.focus_history().iter().copied().collect::<Vec<_>>(),
            vec![a, c, b]
        );
    }

    #[test]
    fn forget_removes_from_history() {
        let mut store = SessionStore::new();
        let a = mk_session(&mut store, "a");
        let b = mk_session(&mut store, "b");
        store.record_focus(a);
        store.record_focus(b);
        store.forget(a);
        assert_eq!(
            store.focus_history().iter().copied().collect::<Vec<_>>(),
            vec![b]
        );
    }

    #[test]
    fn record_focus_caps_at_max() {
        let mut store = SessionStore::new();
        // Create more than FOCUS_HISTORY_MAX sessions and focus each.
        let ids: Vec<SessionId> = (0..FOCUS_HISTORY_MAX + 5)
            .map(|i| mk_session(&mut store, &format!("agent{i}")))
            .collect();
        for id in &ids {
            store.record_focus(*id);
        }
        assert_eq!(store.focus_history().len(), FOCUS_HISTORY_MAX);
        // Front is the most-recently-focused.
        assert_eq!(store.focus_history().front().copied(), ids.last().copied());
    }
}
