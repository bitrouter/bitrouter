use crate::model::{ScrollbackState, Session, SessionBadge, SessionStatus, agent_color};

use super::{App, InputMode};

impl App {
    /// Find the session index for a given agent id. Returns the FIRST
    /// match — callers that may have multiple sessions per agent must
    /// look up by [`SessionId`](crate::model::SessionId) instead.
    pub(super) fn session_for_agent(&self, agent_id: &str) -> Option<usize> {
        self.state.session_store.find_by_agent(agent_id)
    }

    /// Switch to a session by index, clearing its badge and resetting search.
    pub(super) fn switch_session(&mut self, idx: usize) {
        if idx < self.state.session_store.active.len() {
            self.state.active_session = idx;
            self.state.session_store.active[idx].badge = SessionBadge::None;
            // Search state references entries from the old session — invalidate it.
            if self.state.search.is_some() {
                self.state.search = None;
                if self.state.mode == InputMode::Search {
                    self.state.mode = InputMode::Normal;
                }
            }
        }
    }

    /// Find or create a session for an agent. Returns the session index.
    /// Used by event handlers that need to surface a message even when
    /// no session exists yet for the agent (e.g. error toasts).
    pub(super) fn ensure_session_for_agent(&mut self, agent_id: &str) -> usize {
        if let Some(idx) = self.session_for_agent(agent_id) {
            return idx;
        }
        self.create_session_for_agent(agent_id)
    }

    /// Always create a new session for an agent, even if one already
    /// exists. Returns the index of the new session.
    pub(super) fn create_session_for_agent(&mut self, agent_id: &str) -> usize {
        let id = self.state.session_store.allocate_id();
        // Per-session color: round-robin through the palette indexed
        // by the SessionId so two sessions on the same agent are
        // visually distinct in the sidebar.
        let color = agent_color(id.0 as usize);
        self.state.session_store.active.push(Session {
            id,
            agent_id: agent_id.to_string(),
            title: None,
            color,
            acp_session_id: None,
            status: SessionStatus::Connecting,
            scrollback: ScrollbackState::new(),
            badge: SessionBadge::None,
        });
        self.state.session_store.active.len() - 1
    }

    /// Increment unread badge on a background session, addressed by index.
    pub(super) fn badge_background_session(&mut self, session_idx: usize) {
        if session_idx == self.state.active_session {
            return;
        }
        let Some(session) = self.state.session_store.active.get_mut(session_idx) else {
            return;
        };
        session.badge = match &session.badge {
            SessionBadge::None => SessionBadge::Unread(1),
            SessionBadge::Unread(n) => SessionBadge::Unread(n + 1),
            SessionBadge::Permission => SessionBadge::Permission, // Don't downgrade
        };
    }

    /// Close the current session. Sends a per-session disconnect to the
    /// provider; the agent's other sessions (if any) are unaffected.
    pub(super) fn close_current_session(&mut self) {
        if self.state.session_store.active.is_empty() {
            return;
        }
        let idx = self.state.active_session;
        let session = &self.state.session_store.active[idx];
        let agent_id = session.agent_id.clone();
        let acp_session_id = session.acp_session_id.clone();

        if let Some(acp_id) = acp_session_id {
            self.session_system.disconnect_session(&agent_id, &acp_id);
        }

        self.state.session_store.active.remove(idx);
        self.state.active_session = if self.state.session_store.active.is_empty() {
            0
        } else {
            idx.min(self.state.session_store.active.len() - 1)
        };

        // If no sessions remain on this agent, drop the provider too.
        let agent_still_has_sessions = self
            .state
            .session_store
            .active
            .iter()
            .any(|s| s.agent_id == agent_id);
        if !agent_still_has_sessions {
            self.session_system.forget_provider(&agent_id);
        }
    }
}
