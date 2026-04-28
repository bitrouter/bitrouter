//! Picker popup micro-mode.
//!
//! When a slash command needs the user to choose from a list (an
//! agent for `/session new`, a session for `/session switch`,
//! on-disk sessions for `/session import`), the TUI opens a floating
//! popup anchored above the input bar (rendered by
//! [`crate::ui::scrollback::render`]) and switches to
//! [`InputMode::Picker`]. While the picker is open `j/k`, Enter, Esc,
//! and Space are scoped to it; other input is suspended. Confirm or
//! cancel closes the popup and leaves a one-line breadcrumb in the
//! active scrollback.

use crossterm::event::{KeyCode, KeyEvent};

use crate::model::{PickerAction, PickerItem, PickerState};

use super::{App, InputMode};

/// Move the picker cursor by `delta` (1 forward, -1 backward),
/// skipping non-selectable rows. Wraps at the ends.
fn step_picker_cursor(picker: &mut PickerState, delta: i32) {
    let len = picker.items.len();
    if len == 0 {
        return;
    }
    let mut next = picker.cursor as i32;
    for _ in 0..len as i32 {
        next = (next + delta).rem_euclid(len as i32);
        if let Some(it) = picker.items.get(next as usize)
            && it.selectable
        {
            picker.cursor = next as usize;
            return;
        }
    }
}

impl App {
    /// Open a picker popup. Switches to [`InputMode::Picker`] until
    /// the user confirms or cancels.
    pub(super) fn open_picker(
        &mut self,
        title: String,
        items: Vec<PickerItem>,
        action: PickerAction,
        multiselect: bool,
    ) {
        if items.is_empty() || !items.iter().any(|it| it.selectable) {
            self.push_system_msg(&format!("{title}: (nothing to pick)"));
            return;
        }
        let cursor = items.iter().position(|it| it.selectable).unwrap_or(0);
        self.state.picker = Some(PickerState {
            title,
            items,
            cursor,
            selected: std::collections::HashSet::new(),
            multiselect,
            action,
        });
        self.state.mode = InputMode::Picker;
    }

    pub(super) fn handle_picker_key(&mut self, key: KeyEvent) {
        let Some(picker) = self.state.picker.as_mut() else {
            self.state.mode = InputMode::Normal;
            return;
        };

        match key.code {
            KeyCode::Esc => {
                let title = picker.title.clone();
                self.state.picker = None;
                self.state.mode = InputMode::Normal;
                self.push_system_msg(&format!("✗ {title} cancelled"));
            }
            KeyCode::Enter => {
                let chosen: Vec<usize> = if picker.multiselect {
                    let mut v: Vec<usize> = picker.selected.iter().copied().collect();
                    v.sort_unstable();
                    v
                } else {
                    vec![picker.cursor]
                };
                let action = picker.action.clone();
                let summary = picker_summary(picker, &chosen);
                self.state.picker = None;
                self.state.mode = InputMode::Normal;
                self.dispatch_picker_action(action, chosen);
                if let Some(text) = summary {
                    self.push_system_msg(&format!("✓ {text}"));
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                step_picker_cursor(picker, 1);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                step_picker_cursor(picker, -1);
            }
            KeyCode::Char(' ') if picker.multiselect => {
                let cur = picker.cursor;
                if !picker.selected.insert(cur) {
                    picker.selected.remove(&cur);
                }
            }
            _ => {}
        }
    }

    fn dispatch_picker_action(&mut self, action: PickerAction, indices: Vec<usize>) {
        match action {
            PickerAction::NewSession { agents } => {
                if let Some(&i) = indices.first()
                    && let Some(name) = agents.get(i)
                {
                    let len_before = self.state.session_store.active.len();
                    self.connect_agent(name);
                    let len_after = self.state.session_store.active.len();
                    if len_after > len_before {
                        self.switch_session(len_after - 1);
                    }
                }
            }
            PickerAction::SwitchSession { ids } => {
                if let Some(&i) = indices.first()
                    && let Some(target_id) = ids.get(i)
                    && let Some(idx) = self.state.session_store.index_of(*target_id)
                {
                    self.switch_session(idx);
                }
            }
            PickerAction::Import { candidates } => {
                let mut imported = 0usize;
                for &i in &indices {
                    if let Some(c) = candidates.get(i) {
                        self.import_session(
                            &c.agent_id,
                            c.external_session_id.clone(),
                            c.source_path.clone(),
                            c.title_hint.clone(),
                        );
                        imported += 1;
                    }
                }
                if imported > 0 {
                    self.push_system_msg(&format!("Importing {imported} session(s)..."));
                }
                let _ = self.write_import_marker();
            }
        }
    }
}

/// One-line breadcrumb for the active scrollback after a picker
/// confirms. Returns `None` when the picker had no chosen items so we
/// don't push an empty `✓` line.
fn picker_summary(picker: &PickerState, indices: &[usize]) -> Option<String> {
    let chosen: Vec<String> = indices
        .iter()
        .filter_map(|&i| picker.items.get(i).map(|it| it.label.clone()))
        .collect();
    if chosen.is_empty() {
        None
    } else {
        Some(chosen.join(", "))
    }
}
