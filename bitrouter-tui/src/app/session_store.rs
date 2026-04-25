use crate::model::{Session, SessionId};

/// Owns all active sessions and allocates monotonic [`SessionId`]s.
///
/// Callers use `store.active` directly for iteration and indexed access;
/// `allocate_id` is the one bit of bookkeeping the store owns. `archived`
/// and `focus_history` fields land in later PRs (see multi-session-tui.md
/// §4 PRs 5 and 6) so they aren't declared here yet.
pub struct SessionStore {
    /// Currently-active sessions, rendered in the sidebar.
    pub active: Vec<Session>,
    next_id: u64,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            active: Vec::new(),
            next_id: 0,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ScrollbackState, Session, SessionBadge, SessionStatus};

    fn mk_session(store: &mut SessionStore, agent_id: &str) -> SessionId {
        let id = store.allocate_id();
        store.active.push(Session {
            id,
            agent_id: agent_id.to_string(),
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
}
