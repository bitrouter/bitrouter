use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::model::{EntryKind, SearchState};

use super::helpers::PermissionChoice;
use super::{App, InputMode};

impl App {
    pub(super) fn handle_key(&mut self, key: KeyEvent) {
        // Global: Ctrl-C always exits.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.running = false;
            return;
        }

        // Dispatch to current mode.
        match self.state.mode {
            InputMode::Normal => self.handle_normal_key(key),
            InputMode::Scroll => self.handle_scroll_key(key),
            InputMode::Search => self.handle_search_mode_key(key),
            InputMode::Permission => self.handle_permission_key(key),
            InputMode::Picker => self.handle_picker_key(key),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        // Tab / Shift+Tab: when autocomplete is open, accept the
        // candidate (or move within it). Otherwise cycle session tabs.
        if key.code == KeyCode::Tab && !key.modifiers.contains(KeyModifiers::SHIFT) {
            if self.state.autocomplete.is_some() {
                self.accept_autocomplete();
            } else {
                self.cycle_session_tab(true);
            }
            return;
        }
        if key.code == KeyCode::BackTab
            || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
        {
            if self.state.autocomplete.is_some() {
                self.autocomplete_prev();
            } else {
                self.cycle_session_tab(false);
            }
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
            KeyCode::Esc => {
                if self.state.autocomplete.is_some() {
                    self.close_autocomplete();
                } else {
                    // Enter scroll mode on the active session.
                    if let Some(sb) = self.state.active_scrollback_mut() {
                        sb.follow = false;
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
            KeyCode::Up => {
                if self.state.autocomplete.is_some() {
                    self.autocomplete_prev();
                } else {
                    self.state.input.move_up();
                }
            }
            KeyCode::Down => {
                if self.state.autocomplete.is_some() {
                    self.autocomplete_next();
                } else {
                    self.state.input.move_down();
                }
            }
            KeyCode::Home => self.state.input.home(),
            KeyCode::End => self.state.input.end(),
            KeyCode::Char('?') if self.state.input.is_empty() => {
                // `?` on an empty prompt is the one-keystroke shortcut
                // for `/help`.
                self.run_help_command();
            }
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
                if let Some(sb) = self.state.active_scrollback_mut()
                    && let Some(cursor_idx) = sb.scroll_cursor
                    && let Some(entry) = sb.entries.get_mut(cursor_idx)
                {
                    entry.collapsed = !entry.collapsed;
                    sb.invalidate_entry(cursor_idx);
                }
            }
            KeyCode::Char('G') | KeyCode::Char('i') => {
                if let Some(sb) = self.state.active_scrollback_mut() {
                    sb.follow = true;
                    sb.scroll_cursor = None;
                }
                self.state.mode = InputMode::Normal;
            }
            KeyCode::Char('?') => {
                if let Some(sb) = self.state.active_scrollback_mut() {
                    sb.follow = true;
                    sb.scroll_cursor = None;
                }
                self.state.mode = InputMode::Normal;
                self.run_help_command();
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

    fn handle_search_mode_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
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
            self.state.mode = InputMode::Normal;
            return;
        };

        match key.code {
            KeyCode::Char('y') => self.resolve_permission(perm_idx, PermissionChoice::Yes),
            KeyCode::Char('n') => self.resolve_permission(perm_idx, PermissionChoice::No),
            KeyCode::Char('a') => self.resolve_permission(perm_idx, PermissionChoice::Always),
            _ => {}
        }
    }
}
