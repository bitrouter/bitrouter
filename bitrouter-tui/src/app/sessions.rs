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
    /// Records the focus in the MRU `focus_history`. Cycle commands use
    /// [`Self::cycle_focus`] instead so they can navigate without
    /// reordering history.
    pub(super) fn switch_session(&mut self, idx: usize) {
        if idx >= self.state.session_store.active.len() {
            return;
        }
        self.state.active_session = idx;
        let id = self.state.session_store.active[idx].id;
        self.state.session_store.active[idx].badge = SessionBadge::None;
        self.state.session_store.record_focus(id);
        // Search state references entries from the old session — invalidate it.
        if self.state.search.is_some() {
            self.state.search = None;
            if self.state.mode == InputMode::Search {
                self.state.mode = InputMode::Normal;
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
        let session_id = session.id;
        let agent_id = session.agent_id.clone();
        let acp_session_id = session.acp_session_id.clone();

        if let Some(acp_id) = acp_session_id {
            self.session_system.disconnect_session(&agent_id, &acp_id);
        }

        self.state.session_store.active.remove(idx);
        self.state.session_store.forget(session_id);
        self.state.cycle_pos = None;
        self.state.active_session = if self.state.session_store.active.is_empty() {
            0
        } else {
            idx.min(self.state.session_store.active.len() - 1)
        };
        // Record the new active session as most-recently-focused so the
        // next Ctrl-Tab cycle starts from a sensible place.
        if let Some(new_active) = self
            .state
            .session_store
            .active
            .get(self.state.active_session)
        {
            let id = new_active.id;
            self.state.session_store.record_focus(id);
        }

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

    /// Advance the MRU cycle cursor and switch to the session under it.
    /// `forward = true` walks toward older entries (Ctrl-Tab); `forward
    /// = false` walks toward newer ones (Ctrl-Shift-Tab). Wraps at the
    /// ends of `focus_history` so the cycle is closed.
    ///
    /// Unlike [`Self::switch_session`], this does NOT call `record_focus`
    /// — doing so would collapse the history into a constant toggle
    /// between two entries. The cursor stays live until any non-cycle
    /// key event hits [`Self::commit_cycle_if_active`].
    pub(super) fn cycle_focus(&mut self, forward: bool) {
        let history = self.state.session_store.focus_history();
        if history.len() < 2 {
            return;
        }
        let len = history.len();
        let next_pos = match self.state.cycle_pos {
            None => {
                if forward {
                    1
                } else {
                    len - 1
                }
            }
            Some(p) => {
                if forward {
                    (p + 1) % len
                } else {
                    (p + len - 1) % len
                }
            }
        };
        let target_id = history[next_pos];
        let Some(idx) = self.state.session_store.index_of(target_id) else {
            return;
        };
        self.state.active_session = idx;
        self.state.session_store.active[idx].badge = SessionBadge::None;
        if self.state.search.is_some() {
            self.state.search = None;
            if self.state.mode == InputMode::Search {
                self.state.mode = InputMode::Normal;
            }
        }
        self.state.cycle_pos = Some(next_pos);
    }

    /// If a Ctrl-Tab cycle was in progress, commit it: clear the cursor
    /// and front-load the now-active session in `focus_history`. Called
    /// at the top of `handle_key` for any key that isn't itself a cycle
    /// command, so the next cycle starts from the latest focus.
    pub(super) fn commit_cycle_if_active(&mut self) {
        if self.state.cycle_pos.is_none() {
            return;
        }
        self.state.cycle_pos = None;
        if let Some(active) = self
            .state
            .session_store
            .active
            .get(self.state.active_session)
        {
            let id = active.id;
            self.state.session_store.record_focus(id);
        }
    }
}
