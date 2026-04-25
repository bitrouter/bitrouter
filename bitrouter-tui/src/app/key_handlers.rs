use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::model::{EntryKind, Modal, SearchState, SessionSearchState};

use super::helpers::PermissionChoice;
use super::{App, InputMode};

impl App {
    pub(super) fn handle_key(&mut self, key: KeyEvent) {
        // Global: Ctrl-C always exits.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.running = false;
            return;
        }

        // Ctrl-Tab / Ctrl-Shift-Tab: MRU session cycle. Handled before
        // any "commit cycle on non-cycle key" logic so repeated presses
        // can walk through `focus_history` without committing.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && self.state.mode != InputMode::Permission
            && self.state.mode != InputMode::SessionSearch
        {
            // Crossterm reports Ctrl-Shift-Tab as `BackTab` on most
            // terminals; some emit `Tab` with CTRL|SHIFT. Accept both.
            let is_back_tab = key.code == KeyCode::BackTab
                || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT));
            let is_tab = key.code == KeyCode::Tab && !key.modifiers.contains(KeyModifiers::SHIFT);
            if is_tab || is_back_tab {
                self.cycle_focus(is_tab);
                return;
            }
        }

        // Any non-cycle key commits the in-progress Ctrl-Tab cycle.
        self.commit_cycle_if_active();

        // If a modal is open, route all keys to modal handler.
        if self.state.modal.is_some() {
            self.handle_modal_key(key);
            return;
        }

        // Global shortcuts (work in any mode except Permission).
        if self.state.mode != InputMode::Permission && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            match key.code {
                KeyCode::Char('p') => {
                    self.open_command_palette();
                    return;
                }
                KeyCode::Char('o') => {
                    self.open_observability();
                    return;
                }
                KeyCode::Char('b') => {
                    self.state.sidebar_visible = !self.state.sidebar_visible;
                    return;
                }
                KeyCode::Char('n') => {
                    // New session: open the agent picker. The picker
                    // existed before for connect/disconnect; in the
                    // multi-session world Enter on a chosen agent
                    // always spawns a fresh session.
                    self.state.mode = InputMode::Agent;
                    return;
                }
                KeyCode::Char('i') => {
                    // Open the import modal. No-op if the startup
                    // scan turned up nothing — no modal to show.
                    self.open_import_modal();
                    return;
                }
                _ => {}
            }
        }

        // Alt+1..9 or Ctrl+1..9: direct session switch from any mode (except Permission).
        if self.state.mode != InputMode::Permission
            && (key.modifiers.contains(KeyModifiers::ALT)
                || key.modifiers.contains(KeyModifiers::CONTROL))
            && let KeyCode::Char(c @ '1'..='9') = key.code
        {
            let idx = (c as usize) - ('1' as usize);
            self.switch_session(idx);
            if self.state.mode == InputMode::Session {
                self.state.mode = InputMode::Normal;
            }
            return;
        }

        // '?' opens help (only in non-typing modes).
        if key.code == KeyCode::Char('?')
            && !matches!(
                self.state.mode,
                InputMode::Normal
                    | InputMode::Search
                    | InputMode::SessionSearch
                    | InputMode::Permission
            )
        {
            self.state.modal = Some(Modal::Help);
            return;
        }

        // Dispatch to current mode.
        match self.state.mode {
            InputMode::Normal => self.handle_normal_key(key),
            InputMode::Scroll => self.handle_scroll_key(key),
            InputMode::Session => self.handle_session_mode_key(key),
            InputMode::SessionSearch => self.handle_session_search_key(key),
            InputMode::Agent => self.handle_agent_mode_key(key),
            InputMode::Search => self.handle_search_mode_key(key),
            InputMode::Permission => self.handle_permission_key(key),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        // Alt+T or Ctrl+T enters Session mode.
        if (key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('t'))
            || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t'))
        {
            self.state.mode = InputMode::Session;
            return;
        }
        // Alt+A enters Agent mode (Ctrl+A is now readline home).
        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('a') {
            self.state.mode = InputMode::Agent;
            return;
        }
        // Ctrl+W — delete word backward
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('w') {
            self.state.input.delete_word_backward();
            self.after_input_char();
            return;
        }
        // Ctrl+U — delete to line start
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u') {
            self.state.input.delete_to_line_start();
            self.after_input_char();
            return;
        }
        // Ctrl+K — delete to line end
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('k') {
            self.state.input.delete_to_line_end();
            self.after_input_char();
            return;
        }
        // Ctrl+A — move to line start (readline home)
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a') {
            self.state.input.home();
            return;
        }
        // Ctrl+E — move to line end (readline end)
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('e') {
            self.state.input.end();
            return;
        }
        // Alt+Left — word left
        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Left {
            self.state.input.word_left();
            return;
        }
        // Alt+Right — word right
        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Right {
            self.state.input.word_right();
            return;
        }

        match key.code {
            KeyCode::Enter => {
                // Check for autocomplete first.
                if self.state.autocomplete.is_some() {
                    self.accept_autocomplete();
                    return;
                }
                // Shift+Enter, Alt+Enter, or Ctrl+Enter inserts a newline.
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT)
                    || key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    self.state.input.newline();
                    return;
                }
                self.submit_input();
            }
            KeyCode::Tab if self.state.autocomplete.is_some() => {
                self.accept_autocomplete();
            }
            KeyCode::Esc => {
                if self.state.autocomplete.is_some() {
                    self.close_autocomplete();
                } else {
                    // Enter scroll mode on the active session.
                    if let Some(sb) = self.state.active_scrollback_mut() {
                        sb.follow = false;
                        // Place cursor at last entry.
                        let entry_count = sb.entries.len();
                        sb.scroll_cursor = if entry_count > 0 {
                            Some(entry_count - 1)
                        } else {
                            None
                        };
                    }
                    self.state.mode = InputMode::Scroll;
                }
            }
            KeyCode::Backspace => {
                self.state.input.backspace();
                self.after_input_char();
            }
            KeyCode::Delete => {
                self.state.input.delete_char();
                self.after_input_char();
            }
            KeyCode::Left => self.state.input.move_left(),
            KeyCode::Right => self.state.input.move_right(),
            KeyCode::Up => self.state.input.move_up(),
            KeyCode::Down => self.state.input.move_down(),
            KeyCode::Home => self.state.input.home(),
            KeyCode::End => self.state.input.end(),
            KeyCode::Char(c) => {
                self.state.input.insert_char(c);
                self.after_input_char();
            }
            _ => {}
        }
    }

    fn handle_scroll_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(sb) = self.state.active_scrollback_mut() {
                    sb.scroll_offset = sb.scroll_offset.saturating_add(1);
                    sb.follow = false;
                    // Move scroll cursor down.
                    if let Some(cur) = sb.scroll_cursor {
                        let max = sb.entries.len().saturating_sub(1);
                        sb.scroll_cursor = Some((cur + 1).min(max));
                    }
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(sb) = self.state.active_scrollback_mut() {
                    sb.scroll_offset = sb.scroll_offset.saturating_sub(1);
                    sb.follow = false;
                    // Move scroll cursor up.
                    if let Some(cur) = sb.scroll_cursor {
                        sb.scroll_cursor = Some(cur.saturating_sub(1));
                    }
                }
            }
            KeyCode::PageDown => {
                if let Some(sb) = self.state.active_scrollback_mut() {
                    sb.scroll_offset = sb.scroll_offset.saturating_add(20);
                    sb.follow = false;
                }
            }
            KeyCode::PageUp => {
                if let Some(sb) = self.state.active_scrollback_mut() {
                    sb.scroll_offset = sb.scroll_offset.saturating_sub(20);
                    sb.follow = false;
                }
            }
            KeyCode::Char('c') => {
                // Toggle collapse on the entry under the scroll cursor.
                if let Some(sb) = self.state.active_scrollback_mut()
                    && let Some(cursor_idx) = sb.scroll_cursor
                    && let Some(entry) = sb.entries.get_mut(cursor_idx)
                {
                    entry.collapsed = !entry.collapsed;
                    sb.invalidate_entry(cursor_idx);
                }
            }
            KeyCode::Char('G') => {
                if let Some(sb) = self.state.active_scrollback_mut() {
                    sb.follow = true;
                    sb.scroll_cursor = None;
                }
                self.state.mode = InputMode::Normal;
            }
            KeyCode::Char('i') => {
                if let Some(sb) = self.state.active_scrollback_mut() {
                    sb.follow = true;
                    sb.scroll_cursor = None;
                }
                self.state.mode = InputMode::Normal;
            }
            KeyCode::Char('/') => {
                self.state.search = Some(SearchState {
                    query: String::new(),
                    matches: Vec::new(),
                    current_match: 0,
                });
                self.state.mode = InputMode::Search;
            }
            KeyCode::Esc => {
                if let Some(sb) = self.state.active_scrollback_mut() {
                    sb.follow = true;
                    sb.scroll_cursor = None;
                }
                self.state.mode = InputMode::Normal;
            }
            _ => {
                // Any printable char returns to Normal mode.
                if let KeyCode::Char(c) = key.code {
                    if let Some(sb) = self.state.active_scrollback_mut() {
                        sb.follow = true;
                    }
                    self.state.mode = InputMode::Normal;
                    self.state.input.insert_char(c);
                    self.after_input_char();
                }
            }
        }
    }

    fn handle_session_mode_key(&mut self, key: KeyEvent) {
        let session_count = self.state.session_store.active.len();
        match key.code {
            KeyCode::Char('h') | KeyCode::Left
                if session_count > 0 && self.state.active_session > 0 =>
            {
                let idx = self.state.active_session - 1;
                self.switch_session(idx);
            }
            KeyCode::Char('l') | KeyCode::Right
                if session_count > 0 && self.state.active_session + 1 < session_count =>
            {
                let idx = self.state.active_session + 1;
                self.switch_session(idx);
            }
            KeyCode::Char(c @ '1'..='9') => {
                let idx = (c as usize) - ('1' as usize);
                self.switch_session(idx);
                self.state.mode = InputMode::Normal;
            }
            KeyCode::Char('n') => {
                // New session → enter Agent mode to pick agent.
                self.state.mode = InputMode::Agent;
            }
            KeyCode::Char('x') => {
                if session_count > 0 {
                    self.close_current_session();
                }
                self.state.mode = InputMode::Normal;
            }
            KeyCode::Char('/') => {
                self.enter_session_search();
            }
            KeyCode::Esc => {
                self.state.mode = InputMode::Normal;
            }
            _ => {}
        }
    }

    fn handle_session_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.exit_session_search();
            }
            KeyCode::Enter => {
                // Commit selection (if any) and exit search.
                let target = self
                    .state
                    .session_search
                    .as_ref()
                    .and_then(|s| s.matches.get(s.selected).copied());
                if let Some(idx) = target {
                    self.switch_session(idx);
                }
                self.exit_session_search();
            }
            KeyCode::Down => {
                if let Some(s) = self.state.session_search.as_mut()
                    && !s.matches.is_empty()
                {
                    s.selected = (s.selected + 1) % s.matches.len();
                }
            }
            KeyCode::Up => {
                if let Some(s) = self.state.session_search.as_mut()
                    && !s.matches.is_empty()
                {
                    s.selected = if s.selected == 0 {
                        s.matches.len() - 1
                    } else {
                        s.selected - 1
                    };
                }
            }
            KeyCode::Backspace => {
                if let Some(s) = self.state.session_search.as_mut() {
                    s.query.pop();
                }
                self.recompute_session_search();
            }
            KeyCode::Char(c) => {
                if let Some(s) = self.state.session_search.as_mut() {
                    s.query.push(c);
                }
                self.recompute_session_search();
            }
            _ => {}
        }
    }

    fn enter_session_search(&mut self) {
        let mut state = SessionSearchState {
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
        };
        // Empty query matches everything — seed with all sessions so the
        // user sees the full list and can narrow it from there.
        state.matches = (0..self.state.session_store.active.len()).collect();
        // Anchor the selection on the currently-active session if it's
        // present in the unfiltered match list.
        if let Some(pos) = state
            .matches
            .iter()
            .position(|&i| i == self.state.active_session)
        {
            state.selected = pos;
        }
        self.state.session_search = Some(state);
        self.state.mode = InputMode::SessionSearch;
    }

    fn exit_session_search(&mut self) {
        self.state.session_search = None;
        self.state.mode = InputMode::Session;
    }

    fn handle_agent_mode_key(&mut self, key: KeyEvent) {
        let agent_count = self.state.agents.len();
        if agent_count == 0 {
            if key.code == KeyCode::Esc {
                self.state.mode = InputMode::Normal;
            }
            return;
        }

        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.state.agent_list_selected = (self.state.agent_list_selected + 1) % agent_count;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.state.agent_list_selected > 0 {
                    self.state.agent_list_selected -= 1;
                } else {
                    self.state.agent_list_selected = agent_count - 1;
                }
            }
            KeyCode::Enter | KeyCode::Char('c') => {
                let selected = self.state.agent_list_selected;
                if let Some(agent) = self.state.agents.get(selected) {
                    let name = agent.name.clone();
                    // Always spawn a fresh session for the chosen agent.
                    // (Use Alt+1..9 to switch back to existing ones.)
                    // `connect_agent` may early-return without creating
                    // a session (already installing, no config) — only
                    // switch when a new entry actually appeared.
                    let len_before = self.state.session_store.active.len();
                    self.connect_agent(&name);
                    let len_after = self.state.session_store.active.len();
                    if len_after > len_before {
                        self.switch_session(len_after - 1);
                    }
                    self.state.mode = InputMode::Normal;
                }
            }
            KeyCode::Char('d') => {
                let selected = self.state.agent_list_selected;
                if let Some(agent) = self.state.agents.get(selected) {
                    let name = agent.name.clone();
                    self.disconnect_agent(&name);
                }
            }
            KeyCode::Char('r') => {
                self.rediscover_agents();
            }
            KeyCode::Esc => {
                self.state.mode = InputMode::Normal;
            }
            _ => {}
        }
    }

    fn handle_search_mode_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                // Jump to next match.
                if let Some(search) = &mut self.state.search
                    && !search.matches.is_empty()
                {
                    search.current_match = (search.current_match + 1) % search.matches.len();
                }
                self.scroll_to_search_match();
            }
            KeyCode::Backspace => {
                if let Some(search) = &mut self.state.search {
                    search.query.pop();
                }
                self.recompute_search();
            }
            KeyCode::Char(c) => {
                if let Some(search) = &mut self.state.search {
                    search.query.push(c);
                }
                self.recompute_search();
            }
            KeyCode::Esc => {
                self.state.search = None;
                self.state.mode = InputMode::Scroll;
            }
            _ => {}
        }
    }

    fn handle_permission_key(&mut self, key: KeyEvent) {
        // Find the unresolved permission entry in the active session.
        let perm_idx = self.state.active_scrollback().and_then(|sb| {
            sb.entries
                .iter()
                .position(|e| matches!(&e.kind, EntryKind::Permission(p) if !p.resolved))
        });

        let Some(perm_idx) = perm_idx else {
            // No pending permission in active session — return to Normal.
            self.state.mode = InputMode::Normal;
            return;
        };

        match key.code {
            KeyCode::Char('y') => self.resolve_permission(perm_idx, PermissionChoice::Yes),
            KeyCode::Char('n') => self.resolve_permission(perm_idx, PermissionChoice::No),
            KeyCode::Char('a') => self.resolve_permission(perm_idx, PermissionChoice::Always),
            _ => {} // Ignore all other keys during permission.
        }
    }
}
