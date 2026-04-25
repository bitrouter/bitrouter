use super::App;
use super::helpers::entry_contains_text;
use crate::model::Session;

/// Whether a session matches the sidebar-search query. Case-insensitive
/// substring against `title || agent_id`. Empty query matches everything.
pub(super) fn session_matches_query(session: &Session, query_lower: &str) -> bool {
    if query_lower.is_empty() {
        return true;
    }
    let title_hit = session
        .title
        .as_deref()
        .is_some_and(|t| t.to_lowercase().contains(query_lower));
    title_hit || session.agent_id.to_lowercase().contains(query_lower)
}

impl App {
    /// Recompute sidebar-filter matches from the current
    /// [`SessionSearchState::query`](crate::model::SessionSearchState).
    /// An empty query matches every session so the sidebar shows the
    /// full list. Substring match is case-insensitive against
    /// `title || agent_id`.
    pub(super) fn recompute_session_search(&mut self) {
        let query = match self.state.session_search.as_ref() {
            Some(s) => s.query.to_lowercase(),
            None => return,
        };
        let active_idx = self.state.active_session;
        let matches: Vec<usize> = self
            .state
            .session_store
            .active
            .iter()
            .enumerate()
            .filter(|(_, s)| session_matches_query(s, &query))
            .map(|(i, _)| i)
            .collect();
        if let Some(search) = self.state.session_search.as_mut() {
            // Try to keep the selection on the active session if it's
            // visible, else clamp to in-range.
            let selected = matches
                .iter()
                .position(|&i| i == active_idx)
                .unwrap_or_else(|| matches.len().saturating_sub(1).min(search.selected));
            search.matches = matches;
            search.selected = selected;
        }
    }

    pub(super) fn recompute_search(&mut self) {
        let query = match &self.state.search {
            Some(s) if !s.query.is_empty() => s.query.to_lowercase(),
            _ => {
                if let Some(search) = &mut self.state.search {
                    search.matches.clear();
                    search.current_match = 0;
                }
                return;
            }
        };

        let matches: Vec<u64> = if let Some(sb) = self.state.active_scrollback() {
            sb.entries
                .iter()
                .filter(|e| entry_contains_text(&e.kind, &query))
                .map(|e| e.id)
                .collect()
        } else {
            Vec::new()
        };

        if let Some(search) = &mut self.state.search {
            search.matches = matches;
            search.current_match = 0;
        }
    }

    pub(super) fn scroll_to_search_match(&mut self) {
        let target_id = match &self.state.search {
            Some(s) => s.matches.get(s.current_match).copied(),
            None => None,
        };
        let Some(target_id) = target_id else { return };

        if let Some(sb) = self.state.active_scrollback_mut() {
            let Some(idx) = sb.index_of(target_id) else {
                return;
            };
            // Use exact line offsets if available (populated by render loop).
            if sb.line_offsets.len() > idx {
                let line_pos = sb.line_offsets[idx];
                sb.scroll_offset = line_pos.saturating_sub(3);
                sb.follow = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::session_matches_query;
    use crate::model::{
        ScrollbackState, Session, SessionBadge, SessionId, SessionSource, SessionStatus,
    };
    use ratatui::style::Color;

    fn mk_session(id: u64, agent_id: &str, title: Option<&str>) -> Session {
        Session {
            id: SessionId(id),
            agent_id: agent_id.to_string(),
            title: title.map(|t| t.to_string()),
            color: Color::Green,
            acp_session_id: None,
            status: SessionStatus::Connected,
            scrollback: ScrollbackState::new(),
            badge: SessionBadge::None,
            source: SessionSource::Native,
            external_session_id: None,
        }
    }

    #[test]
    fn empty_query_matches_every_session() {
        let s = mk_session(0, "claude-code", None);
        assert!(session_matches_query(&s, ""));
    }

    #[test]
    fn matches_against_agent_id_case_insensitive() {
        let s = mk_session(0, "Claude-Code", None);
        assert!(session_matches_query(&s, "claude"));
        assert!(session_matches_query(&s, "code"));
        assert!(!session_matches_query(&s, "codex"));
    }

    #[test]
    fn matches_against_title_when_present() {
        let s = mk_session(0, "claude-code", Some("Refactor router"));
        assert!(session_matches_query(&s, "router"));
        assert!(session_matches_query(&s, "refactor"));
        // Falls through to agent_id even when title is set.
        assert!(session_matches_query(&s, "claude"));
        assert!(!session_matches_query(&s, "missing"));
    }
}
