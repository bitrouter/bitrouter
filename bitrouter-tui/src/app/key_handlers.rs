use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::model::{EntryKind, Modal, SearchState};

use super::helpers::PermissionChoice;
use super::{App, InputMode};

impl App {
    pub(super) fn handle_key(&mut self, key: KeyEvent) {
        // Global: Ctrl-C always exits.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.running = false;
            return;
        }

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
                _ => {}
            }
        }

        // Alt+1..9 or Ctrl+1..9: direct tab switch from any mode (except Permission).
        if self.state.mode != InputMode::Permission
            && (key.modifiers.contains(KeyModifiers::ALT)
                || key.modifiers.contains(KeyModifiers::CONTROL))
            && let KeyCode::Char(c @ '1'..='9') = key.code
        {
            let idx = (c as usize) - ('1' as usize);
            self.switch_tab(idx);
            if self.state.mode == InputMode::Tab {
                self.state.mode = InputMode::Normal;
            }
            return;
        }

        // '?' opens help (only in non-typing modes).
        if key.code == KeyCode::Char('?')
            && !matches!(
                self.state.mode,
                InputMode::Normal | InputMode::Search | InputMode::Permission
            )
        {
            self.state.modal = Some(Modal::Help);
            return;
        }

        // Dispatch to current mode.
        match self.state.mode {
            InputMode::Normal => self.handle_normal_key(key),
            InputMode::Scroll => self.handle_scroll_key(key),
            InputMode::Tab => self.handle_tab_mode_key(key),
            InputMode::Agent => self.handle_agent_mode_key(key),
            InputMode::Search => self.handle_search_mode_key(key),
            InputMode::Permission => self.handle_permission_key(key),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        // Alt+T or Ctrl+T enters Tab mode.
        if (key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('t'))
            || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t'))
        {
            self.state.mode = InputMode::Tab;
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
            KeyCode::Tab => {
                if self.state.autocomplete.is_some() {
                    self.accept_autocomplete();
                }
            }
            KeyCode::Esc => {
                if self.state.autocomplete.is_some() {
                    self.close_autocomplete();
                } else {
                    // Enter scroll mode on the active tab.
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

    fn handle_tab_mode_key(&mut self, key: KeyEvent) {
        let tab_count = self.state.tabs.len();
        match key.code {
            KeyCode::Char('h') | KeyCode::Left => {
                if tab_count > 0 && self.state.active_tab > 0 {
                    let idx = self.state.active_tab - 1;
                    self.switch_tab(idx);
                }
            }
            KeyCode::Char('l') | KeyCode::Right => {
                if tab_count > 0 && self.state.active_tab + 1 < tab_count {
                    let idx = self.state.active_tab + 1;
                    self.switch_tab(idx);
                }
            }
            KeyCode::Char(c @ '1'..='9') => {
                let idx = (c as usize) - ('1' as usize);
                self.switch_tab(idx);
                self.state.mode = InputMode::Normal;
            }
            KeyCode::Char('n') => {
                // New tab → enter Agent mode to pick agent.
                self.state.mode = InputMode::Agent;
            }
            KeyCode::Char('x') => {
                if tab_count > 0 {
                    self.close_current_tab();
                }
                self.state.mode = InputMode::Normal;
            }
            KeyCode::Esc => {
                self.state.mode = InputMode::Normal;
            }
            _ => {}
        }
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
                    if !self.agent_providers.contains_key(&name) {
                        self.connect_agent(&name);
                    }
                    // Switch to the agent's tab.
                    let tab_idx = self.ensure_tab(&name);
                    self.switch_tab(tab_idx);
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
        // Find the unresolved permission entry in the active tab.
        let perm_idx = self.state.active_scrollback().and_then(|sb| {
            sb.entries
                .iter()
                .position(|e| matches!(&e.kind, EntryKind::Permission(p) if !p.resolved))
        });

        let Some(perm_idx) = perm_idx else {
            // No pending permission in active tab — return to Normal.
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
