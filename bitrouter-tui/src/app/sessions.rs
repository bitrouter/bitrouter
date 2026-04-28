use std::path::PathBuf;

use crate::model::{
    ScrollbackState, Session, SessionBadge, SessionSource, SessionStatus, agent_color,
};

use super::{App, InputMode};

impl App {
    /// Find the session index for a given agent id. Returns the FIRST
    /// match — callers that may have multiple sessions per agent must
    /// look up by [`SessionId`](crate::model::SessionId) instead.
    pub(super) fn session_for_agent(&self, agent_id: &str) -> Option<usize> {
        self.state.session_store.find_by_agent(agent_id)
    }

    /// Switch to a session by index, clearing its badge and resetting search.
    /// Records the focus in the MRU `focus_history`.
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
            source: SessionSource::Native,
            external_session_id: None,
        });
        self.state.session_store.active.len() - 1
    }

    /// Create a session entry for an in-progress import. The session is
    /// in `Connecting` state until the agent finishes the
    /// `session/load` handshake; replay events stream in via the same
    /// `AppEvent::Session` path used for live prompts.
    ///
    /// `source_path` records the artifact the import was sourced from
    /// (a `.jsonl` for Claude/Codex). It's stored on the `Session` so
    /// the UI can label imported sessions and so duplicate imports of
    /// the same `external_id` don't silently merge.
    pub(super) fn create_imported_session(
        &mut self,
        agent_id: &str,
        external_session_id: String,
        source_path: PathBuf,
        title_hint: Option<String>,
    ) -> usize {
        let id = self.state.session_store.allocate_id();
        let color = agent_color(id.0 as usize);
        self.state.session_store.active.push(Session {
            id,
            agent_id: agent_id.to_string(),
            title: title_hint,
            color,
            acp_session_id: None,
            status: SessionStatus::Connecting,
            scrollback: ScrollbackState::new(),
            badge: SessionBadge::None,
            source: SessionSource::Imported { source_path },
            external_session_id: Some(external_session_id),
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
        self.state.active_session = if self.state.session_store.active.is_empty() {
            0
        } else {
            idx.min(self.state.session_store.active.len() - 1)
        };
        // Record the new active session as most-recently-focused so the
        // next focus event starts from a sensible place.
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

    /// Cycle the active session tab in left-to-right order.
    /// `forward = true` moves to the next tab; `false` moves to the
    /// previous. Wraps at the ends. No-op when there are fewer than
    /// two active sessions.
    ///
    /// This is the `Tab` / `Shift+Tab` cycle from the product doc.
    /// Unlike the older MRU-based `Ctrl+Tab`, this is plain
    /// tab-order: predictable and shallow. Records the new focus in
    /// `focus_history` so the cycle isn't recursive.
    pub(super) fn cycle_session_tab(&mut self, forward: bool) {
        let len = self.state.session_store.active.len();
        if len < 2 {
            return;
        }
        let next = if forward {
            (self.state.active_session + 1) % len
        } else {
            (self.state.active_session + len - 1) % len
        };
        self.switch_session(next);
    }
}
