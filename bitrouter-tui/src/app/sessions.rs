use crate::model::{ScrollbackState, Session, SessionBadge};

use super::{App, InputMode};

impl App {
    /// Find the session index for a given agent id. Unique under the
    /// current one-session-per-agent invariant.
    pub(super) fn session_for_agent(&self, agent_id: &str) -> Option<usize> {
        self.state
            .sessions
            .iter()
            .position(|s| s.agent_id == agent_id)
    }

    /// Get a mutable reference to an agent's first session scrollback.
    pub(super) fn scrollback_for_agent(&mut self, agent_id: &str) -> Option<&mut ScrollbackState> {
        self.state
            .sessions
            .iter_mut()
            .find(|s| s.agent_id == agent_id)
            .map(|s| &mut s.scrollback)
    }

    /// Switch to a session by index, clearing its badge and resetting search.
    pub(super) fn switch_session(&mut self, idx: usize) {
        if idx < self.state.sessions.len() {
            self.state.active_session = idx;
            self.state.sessions[idx].badge = SessionBadge::None;
            // Search state references entries from the old session — invalidate it.
            if self.state.search.is_some() {
                self.state.search = None;
                if self.state.mode == InputMode::Search {
                    self.state.mode = InputMode::Normal;
                }
            }
        }
    }

    /// Create a session for an agent if one doesn't already exist. Returns the session index.
    pub(super) fn ensure_session_for_agent(&mut self, agent_id: &str) -> usize {
        if let Some(idx) = self.session_for_agent(agent_id) {
            return idx;
        }
        self.state.sessions.push(Session {
            agent_id: agent_id.to_string(),
            agent_name: agent_id.to_string(),
            scrollback: ScrollbackState::new(),
            badge: SessionBadge::None,
        });
        self.state.sessions.len() - 1
    }

    /// Increment unread badge on a background session.
    pub(super) fn badge_background_session(&mut self, agent_id: &str) {
        if let Some(idx) = self.session_for_agent(agent_id)
            && idx != self.state.active_session
        {
            let session = &mut self.state.sessions[idx];
            session.badge = match &session.badge {
                SessionBadge::None => SessionBadge::Unread(1),
                SessionBadge::Unread(n) => SessionBadge::Unread(n + 1),
                SessionBadge::Permission => SessionBadge::Permission, // Don't downgrade
            };
        }
    }

    /// Close the current session and disconnect its agent.
    pub(super) fn close_current_session(&mut self) {
        if self.state.sessions.is_empty() {
            return;
        }
        let idx = self.state.active_session;
        let agent_id = self.state.sessions[idx].agent_id.clone();

        // Disconnect the agent if connected.
        self.disconnect_agent(&agent_id);

        self.state.sessions.remove(idx);
        // Immediately clamp active_session to valid range.
        self.state.active_session = if self.state.sessions.is_empty() {
            0
        } else {
            idx.min(self.state.sessions.len() - 1)
        };
    }
}
